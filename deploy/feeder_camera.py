#!/usr/bin/env python3
"""MaixCAM2 O5 Voice (Rust) — 摄像头喂流进程。

设备摄像头走 libmaix 私有 VI 管线（无 /dev/video*，V4L2 不可用），
因此由本进程采集并全速推帧给 Rust 主程序：

    stdout: [u32 LE len][len 字节 RGB888 w*h*3]

Rust 侧按需取用：1fps 上行 JPEG 编码 + 8Hz 预览缩放。

用法（由 Rust 进程拉起）：
    python3 feeder_camera.py [width height] [--fps N] [--hmirror] [--vflip]
注意（来自 Python 版踩坑记录，camera_feed.py）：
  - 构造器 fps 参数实测不生效，必须 set_fps()
  - set_fps 会把曝光切 Manual 且增益拉爆（整屏过曝白屏），
    之后必须 cam.exp_mode(AeMode.Auto) 恢复自动曝光
  - 阻塞 read() 会长时间占用解释器，这里非阻塞 + 短 sleep 轮询
"""
import os
import struct
import sys
import time

# libmaix 的 native 日志（"-- [I] [Maix Comm Protocol] ..."、vi init failed
# 的 ERROR 等）由 C 层 printf 直写 fd 1，Python 层拦不住，会污染帧流
# （03 实测帧长读出文本值判定失步）。必须在 import maix / Camera() 构造
# **之前**把帧流管道挪到备用 fd、fd 1 换成 /dev/null——构造期的日志同样
# 要吞。
PIPE = os.dup(1)
os.dup2(os.open("/dev/null", os.O_WRONLY), 1)

from maix import camera, image
from maix._maix.camera import AeMode

args = sys.argv[1:]
W, H = 512, 384
if len(args) >= 2 and args[0].isdigit():
    W, H = int(args[0]), int(args[1])
    args = args[2:]
FPS = 30
if "--fps" in args:
    FPS = int(args[args.index("--fps") + 1])
HMIRROR = "--hmirror" in args
VFLIP = "--vflip" in args

cam = camera.Camera(W, H, image.Format.FMT_RGB888)
cam.set_fps(FPS)               # 构造器 fps 参数不生效，必须 set_fps
cam.exp_mode(AeMode.Auto)     # set_fps 后必须恢复自动曝光，否则白屏
if HMIRROR:
    cam.hmirror(1)
if VFLIP:
    cam.vflip(1)

out = os.fdopen(PIPE, "wb", buffering=0)
n = 0
t0 = time.time()
try:
    while True:
        img = cam.read(block=False)
        if img is None:
            time.sleep(0.005)
            continue
        data = img.to_bytes()
        out.write(struct.pack("<I", len(data)))
        out.write(data)
        out.flush()
        n += 1
except BrokenPipeError:
    pass  # Rust 主进程退出
except KeyboardInterrupt:
    pass
finally:
    dt = time.time() - t0
    print(f"[feeder_camera] {n} frames in {dt:.1f}s ({n / dt if dt > 0 else 0:.1f}fps)",
          file=sys.stderr, flush=True)
    try:
        cam.close()
    except Exception:
        pass
