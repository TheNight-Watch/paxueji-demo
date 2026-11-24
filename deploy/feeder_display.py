#!/usr/bin/env python3
"""MaixCAM2 O5 Voice (Rust) — 显示喂流进程。

Rust 主程序合成整帧 640x480 RGB888 后经 stdin 推给本进程：
    [u32 LE len][len 字节 RGB888]
本进程只做 from_bytes + disp.show()（重活全部在 Rust 侧）。

用法（由 Rust 进程拉起，也可手动测试）：
    python3 feeder_display.py            # 640x480
    python3 feeder_display.py 640 480
手动测试：head -c $((640*480*3+4)) /dev/urandom | python3 feeder_display.py
"""
import os
import signal
import struct
import sys
import time

from maix import app, display, image

W, H = 640, 480
if len(sys.argv) >= 3:
    W, H = int(sys.argv[1]), int(sys.argv[2])

_running = True


def _stop(*_):
    global _running
    _running = False


signal.signal(signal.SIGTERM, _stop)
signal.signal(signal.SIGINT, _stop)
# 由 Rust 主进程退出时随管道 EOF 自然结束；防御 SIGPIPE（BrokenPipeError 已捕获）。

disp = display.Display(width=W, height=H)


def main():
    frame_len = W * H * 3
    raw = sys.stdin.buffer  # 二进制管道
    n = 0
    t0 = time.time()
    while _running:
        hdr = raw.read(4)
        if not hdr or len(hdr) < 4:
            break  # EOF：Rust 主进程已退出
        (ln,) = struct.unpack("<I", hdr)
        if ln != frame_len:
            # 尺寸不符：丢弃本帧数据并继续（防错位扩散）
            remain = ln
            while remain > 0:
                chunk = raw.read(min(remain, 1 << 20))
                if not chunk:
                    return
                remain -= len(chunk)
            continue
        data = raw.read(ln)
        if len(data) < ln:
            break
        try:
            img = image.from_bytes(W, H, image.Format.FMT_RGB888, data, copy=False)
            disp.show(img)
            n += 1
        except BrokenPipeError:
            break
        except Exception as e:
            print(f"[feeder_display] show failed: {e}", file=sys.stderr, flush=True)
    dt = time.time() - t0
    print(f"[feeder_display] {n} frames in {dt:.1f}s ({n / dt if dt > 0 else 0:.1f}fps)",
          file=sys.stderr, flush=True)


if __name__ == "__main__":
    try:
        main()
    finally:
        try:
            disp.close()
        except Exception:
            pass
