#!/usr/bin/env python3
"""MaixPy launcher 入口包装：launcher 只会运行 app 目录下的 main.py，
这里直接 exec 同目录的 Rust 二进制 o5-harness-rs（显示/摄像头喂流由
Rust 进程自行拉起）。"""
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
BIN = os.path.join(HERE, "o5-harness-rs")
if not os.access(BIN, os.X_OK):
    print(f"[paxueji wrapper] missing binary: {BIN}", file=sys.stderr)
    sys.exit(1)
argv = [BIN] + sys.argv[1:]
os.environ.setdefault("O5_HARNESS_CONFIG", "/root/paxueji_config.json")
os.execv(BIN, argv)
