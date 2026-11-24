//! 显示后端：`DisplaySink` trait + 设备 feeder 子进程 + 测试 sink。
//!
//! 设备事实（device-findings）：MaixPy 独占 VO 显示（fb0 是 Virtual FB，
//! 写了不上屏），Rust 侧显示只能走喂流子进程：合成整帧 RGB888 →
//! stdin `[u32 LE len][帧字节]` → `python3 feeder_display.py 640 480`
//! （`image.from_bytes` + `disp.show()`）。show 内部有节拍，管道可能
//! 短时拥塞——因此 feeder 写入放在**独立投递线程 + drop-frame 队列**上，
//! UI 线程的 `push_frame` 永不阻塞在管道写（慢了就丢旧帧保最新）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

// feeder 子进程后端（仅 Linux 设备）用的工具 import
#[cfg(target_os = "linux")]
use std::path::Path;

/// 帧投递后端（UI 线程每帧调用；实现必须非阻塞或可接受地快）。
pub trait DisplaySink: Send {
    fn push_frame(&self, rgb: &[u8]);
}

/// 丢弃一切帧（headless / 测试基准）。
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl DisplaySink for NullSink {
    fn push_frame(&self, _rgb: &[u8]) {}
}

/// PNG 转储 sink：每帧写 `dir/frame_{n:05}.png`（编号取模 `max_files`
/// 循环覆盖，默认 40，磁盘占用有界）。测试断言 / 调试用。
pub struct PngDumpSink {
    dir: PathBuf,
    width: u32,
    height: u32,
    counter: Arc<AtomicU64>,
    max_files: u64,
}

impl PngDumpSink {
    pub fn new(dir: impl Into<PathBuf>, width: u32, height: u32) -> PngDumpSink {
        PngDumpSink {
            dir: dir.into(),
            width,
            height,
            counter: Arc::new(AtomicU64::new(0)),
            max_files: 40,
        }
    }

    /// 最多保留的文件数（循环覆盖）。
    pub fn with_max_files(mut self, n: u64) -> Self {
        self.max_files = n.max(1);
        self
    }

    /// 已收到的帧数。
    pub fn frames(&self) -> u64 {
        self.counter.load(Ordering::Relaxed)
    }
}

impl DisplaySink for PngDumpSink {
    fn push_frame(&self, rgb: &[u8]) {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let path = self.dir.join(format!("frame_{:05}.png", n % self.max_files));
        crate::png::write_png(&path, self.width, self.height, rgb);
    }
}

/// 设备显示 sink：spawn `python3 feeder_display.py <w> <h>` 子进程，
/// 独立线程写 stdin（「最新帧」槽位——投递线程慢时直接覆盖旧帧）。
///
/// 仅 Linux（设备路径 / python3 依赖）；PC 测试用 [`NullSink`] / [`PngDumpSink`]。
#[cfg(target_os = "linux")]
pub mod feeder {
    use std::io::Write;
    use std::process::{Child, Command, Stdio};
    use std::sync::{Condvar, Mutex};

    use super::*;

    /// 最新帧槽位（深度 1 的 drop-frame 队列：满了就覆盖，投递线程永远拿最新帧）。
    struct Latest {
        frame: Mutex<Option<Vec<u8>>>,
        signal: Condvar,
        dead: std::sync::atomic::AtomicBool,
    }

    /// 投递线程 + 子进程句柄（Drop 时关管道、杀进程）。
    pub struct FeederSink {
        latest: Arc<Latest>,
        child: Arc<Mutex<Option<Child>>>,
        frame_len: usize,
    }

    impl FeederSink {
        /// 启动 feeder 子进程与投递线程。
        pub fn spawn(feeder_path: &Path, width: u32, height: u32) -> std::io::Result<FeederSink> {
            let spawn_child =
                |path: &Path, w: u32, h: u32| -> std::io::Result<(std::process::Child, std::process::ChildStdin)> {
                    let mut child = Command::new("python3")
                        .arg(path)
                        .arg(w.to_string())
                        .arg(h.to_string())
                        .stdin(Stdio::piped())
                        .stdout(Stdio::null())
                        .stderr(Stdio::inherit())
                        .spawn()?;
                    let stdin = child.stdin.take().expect("stdin piped");
                    Ok((child, stdin))
                };
            let (child, stdin) = spawn_child(feeder_path, width, height)?;
            let frame_len = width as usize * height as usize * 3;
            let latest = Arc::new(Latest {
                frame: Mutex::new(None),
                signal: Condvar::new(),
                dead: std::sync::atomic::AtomicBool::new(false),
            });
            let latest2 = Arc::clone(&latest);
            let child_slot = Arc::new(Mutex::new(Some(child)));
            let child_slot2 = Arc::clone(&child_slot);
            let path = feeder_path.to_path_buf();
            // 投递线程：UI 线程绝不阻塞在管道写（管道拥塞时旧帧被覆盖丢弃）。
            // 管道断开（display 让位被杀：03 固件下 camera 与 display 抢
            // IVPS 组 0，app 切 omni 时会先杀 display feeder 让位）→ 退避
            // 2s 后 respawn 自愈（等 camera 先完成 init），连续失败 8 次才放弃。
            let (w, h) = (width, height);
            std::thread::Builder::new()
                .name("o5-ui-feeder".into())
                .spawn(move || {
                    let mut stdin = stdin;
                    let mut fails = 0u32;
                    loop {
                        let frame = {
                            let mut g = latest2.frame.lock().unwrap();
                            loop {
                                if let Some(f) = g.take() {
                                    break f;
                                }
                                if latest2.dead.load(Ordering::Relaxed) {
                                    return;
                                }
                                g = latest2.signal.wait(g).unwrap();
                            }
                        };
                        let mut hdr = [0u8; 4];
                        let len = frame.len() as u32;
                        hdr.copy_from_slice(&len.to_le_bytes());
                        if let Err(e) =
                            stdin.write_all(&hdr).and_then(|_| stdin.write_all(&frame))
                        {
                            if latest2.dead.load(Ordering::Relaxed) {
                                return;
                            }
                            fails += 1;
                            if fails > 8 {
                                log::warn!("显示 feeder 管道断开（重试耗尽）: {}", e);
                                latest2.dead.store(true, Ordering::Relaxed);
                                latest2.signal.notify_all();
                                return;
                            }
                            log::info!("显示 feeder 管道断开，{}s 后 respawn（第 {fails} 次）: {}",
                                       2, e);
                            // 让旧子进程死透（Drop 之外的手动清理）
                            if let Some(mut c) = child_slot2.lock().unwrap().take() {
                                let _ = c.kill();
                                let _ = c.wait();
                            }
                            // 退避给 camera init 让位
                            for _ in 0..20 {
                                if latest2.dead.load(Ordering::Relaxed) {
                                    return;
                                }
                                std::thread::sleep(std::time::Duration::from_millis(100));
                            }
                            match spawn_child(&path, w, h) {
                                Ok((c, s)) => {
                                    *child_slot2.lock().unwrap() = Some(c);
                                    stdin = s;
                                    fails = 0;
                                }
                                Err(e2) => {
                                    log::warn!("显示 feeder respawn 失败: {e2}");
                                }
                            }
                            continue;
                        }
                    }
                })?;
            Ok(FeederSink {
                latest,
                child: child_slot,
                frame_len,
            })
        }
    }

    impl DisplaySink for FeederSink {
        fn push_frame(&self, rgb: &[u8]) {
            if self.latest.dead.load(Ordering::Relaxed) {
                return;
            }
            if rgb.len() != self.frame_len {
                log::warn!("帧大小不符（{} != {}），丢弃", rgb.len(), self.frame_len);
                return;
            }
            // 覆盖旧帧保新帧（投递线程阻塞在慢管道上时，UI 不被拖住）
            let mut g = self.latest.frame.lock().unwrap();
            *g = Some(rgb.to_vec());
            drop(g);
            self.latest.signal.notify_one();
        }
    }

    impl Drop for FeederSink {
        fn drop(&mut self) {
            // 先叫醒投递线程退出，再杀子进程（阻塞中的 write 得到 EPIPE）
            self.latest.dead.store(true, Ordering::Relaxed);
            self.latest.signal.notify_all();
            if let Some(mut child) = self.child.lock().unwrap().take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub use feeder::FeederSink;

/// 打开默认显示后端：设备 = feeder 子进程（路径来自 config，默认 exe 同目录）；
/// 打不开（headless 调试等）回退 NullSink。PC 上恒为 NullSink。
pub fn open_default(cfg: &o5_config::Config) -> Box<dyn DisplaySink> {
    #[cfg(target_os = "linux")]
    {
        let path = cfg.display_feeder_path();
        match FeederSink::spawn(&path, crate::layout::SCREEN_W as u32, crate::layout::SCREEN_H as u32)
        {
            Ok(s) => Box::new(s),
            Err(e) => {
                log::warn!(
                    "启动显示 feeder 失败（{}: {}），显示走 NullSink",
                    path.display(),
                    e
                );
                Box::new(NullSink)
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = cfg;
        Box::new(NullSink)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::{Color, Image};
    use crate::png::decode_png_store;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("o5_ui_test_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn png_dump_sink_writes_frames() {
        let dir = tmpdir("pngdump");
        let sink = PngDumpSink::new(&dir, 16, 8);
        let mut im = Image::new(16, 8);
        im.fill(Color(10, 20, 30));
        sink.push_frame(&im.data);
        assert_eq!(sink.frames(), 1);
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1);
        let bytes = std::fs::read(&files[0]).unwrap();
        let (w, h, rgb) = decode_png_store(&bytes);
        assert_eq!((w, h), (16, 8));
        assert_eq!(rgb, im.data);
        // 第二帧不同色
        im.fill(Color(200, 100, 50));
        sink.push_frame(&im.data);
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(files.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn png_dump_sink_cycles_files() {
        let dir = tmpdir("pngcycle");
        let sink = PngDumpSink::new(&dir, 4, 4).with_max_files(3);
        let im = Image::new(4, 4);
        for i in 0..10u8 {
            let mut f = im.clone();
            f.fill(Color(i, i, i));
            sink.push_frame(&f.data);
        }
        assert_eq!(sink.frames(), 10);
        let files: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(files.len(), 3, "循环覆盖应只有 max_files 个文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn null_sink_noop() {
        NullSink.push_frame(&[0u8; 10]);
    }

    #[test]
    fn open_default_on_pc() {
        let cfg = o5_config::Config::defaults();
        let _sink = open_default(&cfg); // Windows: NullSink；linux 测试机上可能 spawn feeder（无 python3 时回退）
    }
}
