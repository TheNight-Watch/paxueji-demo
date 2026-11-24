//! 设备帧源：MaixPy 喂流子进程 `feeder_camera.py`。
//!
//! 设备（MaixCAM2 / AX630C）没有 /dev/video\*、没有 /dev/media\*——摄像头完全走
//! libmaix 私有 VI 管线，V4L2 不可用，因此由独立 Python 子进程采集并全速推帧：
//!
//! ```text
//! stdout: [u32 LE len][len 字节 RGB888 w*h*3]
//! ```
//!
//! Rust 侧按需取用：1fps 上行 JPEG 编码 + 8Hz 预览缩放都在 Rust 内做。
//! 实测全速 ~49fps 推帧，`read` 阻塞即可（无 GIL，不会像 Python 版那样
//! 把主线程拖进等帧——Python 版为此用非阻塞 read + sleep 让出 GIL）。
//!
//! 白屏坑（来自 Python 版踩坑记录，feeder_camera.py 内已处理，此处留档）：
//! 构造器 fps 参数实测不生效，必须 `set_fps()`；`set_fps` 会把曝光切 Manual
//! 且增益拉爆（实测 gain=65649，整屏过曝成白色），之后必须
//! `cam.exp_mode(AeMode.Auto)` 恢复自动曝光。
//!
//! hmirror/vflip 由 feeder 命令行参数处理。

#[cfg(any(test, target_os = "linux"))]
use std::io::ErrorKind;
#[cfg(any(test, target_os = "linux"))]
use std::io::Read;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::process::Child;
#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use crate::source::FrameSource;
#[cfg(target_os = "linux")]
use log::{error, info, warn};

/// 子进程句柄槽：读线程与 [`crate::CameraFeed::stop`] 共享，
/// 谁先拿到谁 kill + 收尸（EOF 时读线程收，stop 时主线程收）。
pub(crate) type ChildSlot = Arc<Mutex<Option<Child>>>;

/// 协议 sanity 上限：超过它的"帧长"只可能是流失步（拿数据当长度读了），
/// 再读下去只会 OOM，直接判协议错误退出。
#[cfg(any(test, target_os = "linux"))]
pub(crate) const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// 一次 feeder 读的结果。
#[cfg(any(test, target_os = "linux"))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FeederRead {
    /// 一帧完整 RGB888（长度 == 期望的 w*h*3）。
    Frame(Vec<u8>),
    /// 长度不符的帧：已按长度读完丢弃（跳帧）。
    Skipped,
    /// 干净 EOF（子进程退出 / 管道关闭）。
    Eof,
}

/// 从 `r` 读一帧 feeder 协议数据。
///
/// 纯函数（任何 `Read` 都行），Windows 上用 `Cursor` 直接单测协议解析；
/// 设备源只是把它套在子进程 stdout 上的 `BufReader`。
#[cfg(any(test, target_os = "linux"))]
pub(crate) fn read_feeder_frame<R: Read>(
    r: &mut R,
    expected: usize,
) -> std::io::Result<FeederRead> {
    let mut hdr = [0u8; 4];
    if let Err(e) = r.read_exact(&mut hdr) {
        return match e.kind() {
            // 头都不满：管道已关（子进程退出）
            ErrorKind::UnexpectedEof => Ok(FeederRead::Eof),
            _ => Err(e),
        };
    }
    let len = u32::from_le_bytes(hdr) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("feeder 帧长 {len} 超上限 {MAX_FRAME_BYTES}，判定流失步"),
        ));
    }
    if len == expected {
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf)
            .map(|_| FeederRead::Frame(buf))
            .or_else(|e| match e.kind() {
                ErrorKind::UnexpectedEof => Ok(FeederRead::Eof), // 尾帧截断
                _ => Err(e),
            })
    } else {
        // 长度不符：按长度读掉丢弃（跳帧），流位置保持帧同步
        let mut sink = [0u8; 4096];
        let mut remaining = len;
        while remaining > 0 {
            let n = remaining.min(sink.len());
            if let Err(e) = r.read_exact(&mut sink[..n]) {
                return match e.kind() {
                    ErrorKind::UnexpectedEof => Ok(FeederRead::Eof),
                    _ => Err(e),
                };
            }
            remaining -= n;
        }
        Ok(FeederRead::Skipped)
    }
}

/// kill + 收尸（忽略一切错误：进程可能已死）。
pub(crate) fn kill_child(slot: &ChildSlot) {
    if let Some(mut child) = slot.lock().unwrap().take() {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// 无操作帧源（非 linux 平台 / feeder 拉起失败）：立即结束，相机线程随之退出。
pub(crate) struct NoopFrameSource {
    _private: (),
}

impl NoopFrameSource {
    pub(crate) fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for NoopFrameSource {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameSource for NoopFrameSource {
    fn read_frame(&mut self) -> Option<(u32, u32, Vec<u8>)> {
        None
    }

    fn is_finished(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// 设备实现（linux）：拉起 feeder_camera.py 子进程读 stdout。
// ---------------------------------------------------------------------------

/// 喂流子进程帧源（`#[cfg(target_os = "linux")]` 门控；PC 走 MockFrameSource）。
#[cfg(target_os = "linux")]
pub struct FeederFrameSource {
    width: u32,
    height: u32,
    expected: usize,
    reader: std::io::BufReader<std::process::ChildStdout>,
    child: ChildSlot,
    finished: bool,
}

#[cfg(target_os = "linux")]
impl FeederFrameSource {
    /// 拉起 `python3 <feeder> <w> <h> --fps <fps> [--hmirror] [--vflip]`。
    ///
    /// 调度策略（device-findings）：feeder 子进程 `nice +5`，给音频/UI 线程让路；
    /// `nice` 不存在时回退直接启动。stderr 继承（feeder 的启动失败 traceback
    /// 和收尾统计行直接进主程序日志）。
    pub fn spawn(
        width: u32,
        height: u32,
        fps: u32,
        hmirror: bool,
        vflip: bool,
        feeder: &Path,
        child: &ChildSlot,
    ) -> std::io::Result<Self> {
        let mut argv: Vec<String> = vec![width.to_string(), height.to_string()];
        // fps=0 视为"不指定"（feeder 默认 set_fps(30)），避免把 set_fps(0) 灌进传感器
        if fps > 0 {
            argv.push("--fps".into());
            argv.push(fps.to_string());
        }
        if hmirror {
            argv.push("--hmirror".into());
        }
        if vflip {
            argv.push("--vflip".into());
        }

        // 先试 nice +5（coreutils；exec 后同 pid 就是 python3）
        let mut cmd = Command::new("nice");
        cmd.args(["-n", "5"]).arg("python3").arg(feeder).args(&argv);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped());
        let mut spawned = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                warn!("feeder 以 nice +5 启动失败（{e}），回退直接启动");
                let mut plain = Command::new("python3");
                plain.arg(feeder).args(&argv);
                plain.stdin(Stdio::null()).stdout(Stdio::piped()).spawn()?
            }
        };

        let stdout = spawned.stdout.take().expect("piped stdout"); // 上面显式 piped，take 必成
        *child.lock().unwrap() = Some(spawned);
        info!(
            "feeder_camera 子进程已启动：{}x{} fps<={fps}（{}）",
            width,
            height,
            feeder.display()
        );
        Ok(Self {
            width,
            height,
            expected: width as usize * height as usize * 3,
            reader: std::io::BufReader::new(stdout),
            child: Arc::clone(child),
            finished: false,
        })
    }
}

#[cfg(target_os = "linux")]
impl FrameSource for FeederFrameSource {
    fn read_frame(&mut self) -> Option<(u32, u32, Vec<u8>)> {
        if self.finished {
            return None;
        }
        loop {
            match read_feeder_frame(&mut self.reader, self.expected) {
                Ok(FeederRead::Frame(data)) => {
                    return Some((self.width, self.height, data));
                }
                Ok(FeederRead::Skipped) => {
                    // 跳帧：长度 != w*h*3（与 Python 版"没帧/坏帧跳过"同口径）
                    warn!("feeder 帧长度不符（期望 {} 字节），跳帧", self.expected);
                    continue;
                }
                Ok(FeederRead::Eof) => {
                    self.finished = true;
                    if let Some(mut c) = self.child.lock().unwrap().take() {
                        // 正常退出（BrokenPipe 路径）也会到这里：kill 对已退出
                        // 进程无害，wait 收尸防僵尸
                        let _ = c.kill();
                        let status = c.wait();
                        match status {
                            Ok(s) if s.success() => {
                                info!("feeder 子进程正常退出，相机采集结束");
                            }
                            Ok(s) => {
                                error!("feeder 子进程异常退出：{s}，相机采集结束");
                            }
                            Err(e) => {
                                error!("feeder 子进程 wait 失败：{e}，相机采集结束");
                            }
                        }
                    }
                    return None;
                }
                Err(e) => {
                    self.finished = true;
                    kill_child(&self.child);
                    error!("feeder 流读取失败：{e}，相机采集结束");
                    return None;
                }
            }
        }
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn frame(bytes: &[u8]) -> Vec<u8> {
        let mut v = (bytes.len() as u32).to_le_bytes().to_vec();
        v.extend_from_slice(bytes);
        v
    }

    #[test]
    fn reads_good_frames_in_order() {
        let expected = 6;
        let mut stream = Cursor::new(
            frame(b"abcdef")
                .into_iter()
                .chain(frame(b"123456"))
                .collect::<Vec<u8>>(),
        );
        assert_eq!(
            read_feeder_frame(&mut stream, expected).unwrap(),
            FeederRead::Frame(b"abcdef".to_vec())
        );
        assert_eq!(
            read_feeder_frame(&mut stream, expected).unwrap(),
            FeederRead::Frame(b"123456".to_vec())
        );
        assert_eq!(
            read_feeder_frame(&mut stream, expected).unwrap(),
            FeederRead::Eof
        );
    }

    #[test]
    fn wrong_length_frame_is_skipped_and_stream_stays_synced() {
        // 期望 6 字节帧；中间混一个 3 字节的坏帧和 9 字节的坏帧，都必须被跳过
        let data: Vec<u8> = frame(b"abcdef")
            .into_iter()
            .chain(frame(b"xyz"))
            .chain(frame(b"123456789"))
            .chain(frame(b"good12"))
            .collect();
        let mut stream = Cursor::new(data);
        assert_eq!(
            read_feeder_frame(&mut stream, 6).unwrap(),
            FeederRead::Frame(b"abcdef".to_vec())
        );
        assert_eq!(
            read_feeder_frame(&mut stream, 6).unwrap(),
            FeederRead::Skipped
        );
        assert_eq!(
            read_feeder_frame(&mut stream, 6).unwrap(),
            FeederRead::Skipped
        );
        assert_eq!(
            read_feeder_frame(&mut stream, 6).unwrap(),
            FeederRead::Frame(b"good12".to_vec())
        );
        assert_eq!(read_feeder_frame(&mut stream, 6).unwrap(), FeederRead::Eof);
    }

    #[test]
    fn oversized_length_is_protocol_error() {
        let len = (MAX_FRAME_BYTES + 1) as u32;
        let mut stream = Cursor::new(len.to_le_bytes().to_vec());
        assert!(read_feeder_frame(&mut stream, 6).is_err());
    }

    #[test]
    fn truncated_tail_is_eof() {
        // 头声明 6 字节（== 期望长度）但数据只剩 3 字节：尾帧截断
        let mut v = 6u32.to_le_bytes().to_vec();
        v.extend_from_slice(b"abc");
        let mut stream = Cursor::new(v);
        assert_eq!(read_feeder_frame(&mut stream, 6).unwrap(), FeederRead::Eof);
        // 半个头也是 EOF
        let mut stream = Cursor::new(vec![1u8, 2]);
        assert_eq!(read_feeder_frame(&mut stream, 6).unwrap(), FeederRead::Eof);
        // 空流 = EOF
        let mut stream = Cursor::new(Vec::<u8>::new());
        assert_eq!(read_feeder_frame(&mut stream, 6).unwrap(), FeederRead::Eof);
    }

    #[test]
    fn zero_length_frame_is_skipped() {
        let mut stream = Cursor::new(
            frame(b"")
                .into_iter()
                .chain(frame(b"abc"))
                .collect::<Vec<u8>>(),
        );
        assert_eq!(
            read_feeder_frame(&mut stream, 3).unwrap(),
            FeederRead::Skipped
        );
        assert_eq!(
            read_feeder_frame(&mut stream, 3).unwrap(),
            FeederRead::Frame(b"abc".to_vec())
        );
    }

    #[test]
    fn noop_source_is_immediately_finished() {
        let mut s = NoopFrameSource::new();
        assert!(s.read_frame().is_none());
        assert!(s.is_finished());
    }
}
