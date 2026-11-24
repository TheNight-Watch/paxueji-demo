//! 拍学机实体快门键：gpio_keys（/dev/input/event0）键码 431，按下沿算一次快门。

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;

const KEY_DEVICE: &str = "/dev/input/event0";
const SHUTTER_CODE: u16 = 431;
const EV_KEY: u16 = 1;
const EVENT_SIZE: usize = 24;

pub struct ShutterKey {
    file: Option<File>,
    carry: Vec<u8>,
}

impl ShutterKey {
    /// 设备不存在（PC）或打不开时退化为永不触发。
    pub fn open() -> ShutterKey {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(KEY_DEVICE)
            .map_err(|e| log::info!("实体快门键不可用（{KEY_DEVICE}: {e}）"))
            .ok();
        ShutterKey { file, carry: Vec::new() }
    }

    /// 非阻塞读空事件队列，返回期间按下的次数。
    pub fn poll_presses(&mut self) -> usize {
        let Some(f) = self.file.as_mut() else { return 0 };
        let mut buf = [0u8; EVENT_SIZE * 16];
        let mut presses = 0;
        loop {
            match f.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    self.carry.extend_from_slice(&buf[..n]);
                    let whole = self.carry.len() / EVENT_SIZE * EVENT_SIZE;
                    presses += count_presses(&self.carry[..whole]);
                    self.carry.drain(..whole);
                }
                Err(_) => break,
            }
        }
        presses
    }
}

/// 24 字节 `input_event`（sec, usec, type, code, value）序列里快门键按下沿的个数。
fn count_presses(events: &[u8]) -> usize {
    events
        .chunks_exact(EVENT_SIZE)
        .filter(|e| {
            let typ = u16::from_ne_bytes([e[16], e[17]]);
            let code = u16::from_ne_bytes([e[18], e[19]]);
            let value = i32::from_ne_bytes([e[20], e[21], e[22], e[23]]);
            typ == EV_KEY && code == SHUTTER_CODE && value == 1
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(typ: u16, code: u16, value: i32) -> Vec<u8> {
        let mut v = vec![0u8; 16];
        v.extend_from_slice(&typ.to_ne_bytes());
        v.extend_from_slice(&code.to_ne_bytes());
        v.extend_from_slice(&value.to_ne_bytes());
        v
    }

    #[test]
    fn counts_only_shutter_press_edges() {
        let stream = [
            ev(EV_KEY, SHUTTER_CODE, 1),
            ev(0, 0, 0),
            ev(EV_KEY, SHUTTER_CODE, 0),
            ev(EV_KEY, 30, 1),
            ev(EV_KEY, SHUTTER_CODE, 1),
        ]
        .concat();
        assert_eq!(count_presses(&stream), 2);
    }
}
