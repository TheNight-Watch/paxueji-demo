# 拍学机（paxueji-rs）

基于鹏越 [O5 Voice Rust 版](git@codehub.devcloud.cn-north-4.huaweicloud.com:5a5fe37f080d4dcdab39d8b3ea837bb1/maixcam2_o5_voice_rust.git)（分叉点 `30c0744`）改成的松果CAM 官方案例：对准东西按「拍」，画面定格，小松果老师（MiniCPM-o 5）语音讲解并在屏幕上出知识卡片；孩子可以随时插话追问，不确定的会联网查并标出来源。

## 相对 O5 Voice 改了什么

| 位置 | 改动 |
|---|---|
| `crates/o5-app/src/app/paxueji.rs` | 快门定格、`show_card` 卡片、联网查询芯片。定格帧同时用于屏显和上行；「我拍了新照片」等定格帧随下一个 unit 上行后再发 |
| `crates/o5-app/src/prompt.rs` | 新增 `show_card` 工具声明与路由提示 |
| `crates/o5-config/src/lib.rs` | 默认任务提示词换成「小松果老师」 |
| `crates/o5-ui` | 主界面换成 更多 / 拍 / 静音；定格画面、知识卡片、查询芯片、两行字幕；点状态章重连 |
| `crates/o5-camera` | `PreviewFrame::to_jpeg_b64` |
| `deploy/` | 应用 id `paxueji`、独立配置 `/root/paxueji_config.json`、`deploy.sh`（ssh/scp，无 Python 依赖） |

## 部署

```bash
PAXUEJI_O5_API_KEY=<O5 Key> deploy/deploy.sh   # 首次：编译、安装、写设备配置
deploy/deploy.sh                               # 之后：只更新应用
```

离线看界面：`cargo run -p o5-ui --example paxueji_preview -- <字体.otf> <背景.rgb|-> <输出目录>`。

---

以下为上游 O5 Voice 原文。

# O5 Voice RS — MaixCAM2 上的 MiniCPM-o 实时语音接入（Rust 版）

`MaixCAM2_Cpm_O5_Harness`（Python 版）的 Rust 重写。目标：**消除 GIL 调度病根**——
Python 版的所有性能补丁（aplay 子进程播放、`block=False` 轮询采音、相机非阻塞读、
`nice(-5)`、`setswitchinterval(0.003)`、GC 调参、预览降频保麦克风、退出看门狗 +
`os._exit`）本质上都是在绕 GIL。Rust 版用真线程 + 内核唤醒 + SCHED_RR 实时优先级
让这一整类问题消失。

```
┌────────────────────────── MaixCAM2（AX630C, 2×A53, Ubuntu 22.04）────────────────────┐
│                                                                                      │
│  Rust 主进程（o5-harness-rs，全部热路径）                                             │
│  ┌────────────────────────────────────────────────────────────────────────────┐      │
│  │ o5-mic 线程[SCHED_RR]   arecord 16k ← 内核唤醒 → AEC → 门控/闪避 → 1s unit   │      │
│  │ o5-speaker 线程[SCHED_RR] 队列(120) → 突发lead-in → 块合并 → aplay 24k        │      │
│  │              ↕ far 参考（16k 重采样）                                         │      │
│  │ o5-session-rx/tx 线程   WS (rustls, 无压缩)  ACK 流控 COLD30/WARM2             │      │
│  │ o5-asr 线程            tts-center 实时字幕                                    │      │
│  │ o5-camera 线程[nice+5] 喂流子进程 → RGB → 1fps JPEG 上行 + 8Hz 预览            │      │
│  │ main 线程              UI 全软件渲染 15fps（字形缓存 blit）→ 显示喂流           │      │
│  │ 工具/委派线程          web_search 降级链 / 高德 / 后台大脑 OpenAI 工具循环     │      │
│  └────────────────────────────────────────────────────────────────────────────┘      │
│         │ stdin 帧管道                │ stdout 帧管道                               │
│  feeder_display.py（MaixPy VO 上屏）  feeder_camera.py（MaixPy VI 采集+ISP）          │
│  （fb0 是 vfb 不可直写；摄像头无 /dev/video* —— 见 docs/device-findings.md）           │
│  触摸：/dev/input/event1 直读 evdev（MT-B，竖屏坐标 → 可配置 8 变体横屏映射）           │
└──────────────────────────────────────────────────────────────────────────────────────┘
```

## 为什么这样设计

- **GIL 类病根清零**：采音由 arecord 管道内核唤醒（无轮询循环），播放写入不再冻结进程，
  相机读帧不再把 UI 拖到 2fps，无 GC 停顿，无事件循环卡顿（没有事件循环）。
- **实时优先级**：mic/speaker 线程 SCHED_RR（root 下），摄像头让路——Python 版实测
  "UI+全模态 99-108% CPU、麦克风丢 40% 音频"的场景应归零。
- **硬件路径全部真机验证过**（见 `docs/device-findings.md`）：ALSA 采集/播放参数、
  显示/摄像头喂流、触摸轴范围、字体路径。
- **忠实移植**：协议 v3/4/5、状态机、流控常数、AEC 算法（NLMS + 子带包络）、
  工具降级链、委派语义、所有中文文案，均与 Python 版 1:1，用黄金向量数值级对拍
  （`tests/fixtures/aec/`）。

## Workspace 结构

```
crates/o5-protocol   WS 消息编解码（v3/4/5）+ PCM/f32/base64 编解码 + 黑帧占位
crates/o5-config     配置（同 schema/搜索顺序/原子保存）+ 音色 WAV 规范化
crates/o5-perf       时延埋点（四关键时刻）+ /proc 采样 + netmode/RTT 探测
crates/o5-tools      web_search 降级链 + 高德 map_search + 工具 schema
crates/o5-audio      arecord/aplay + 门控/闪避 + NLMS AEC + 子带 AEC
crates/o5-session    WS 客户端/状态机/ACK 流控/看门狗/优雅关闭
crates/o5-asr        tts-center 实时 ASR 字幕
crates/o5-delegation 后台大脑 delegate_task 工具循环
crates/o5-camera     摄像头喂流消费 + JPEG 上行 + 预览缩放
crates/o5-ui         全软件 UI（字形缓存/控件/触摸/显示喂流）
crates/o5-app        main：UI/--headless/--test-pc/--selftest + 退出看门狗
deploy/              喂流脚本、systemd、launcher 包装、部署器（SFTP）
tests/fixtures/aec/  AEC 黄金向量（Python 参考实跑生成，tests/gen_aec_fixtures.py）
docs/                device-findings.md（硬件探测结论）、CONTRACTS.md（跨 crate 契约）
```

## 构建

PC（Windows/任意）开发与测试：

```bash
cargo test --workspace            # 全部单测（AEC 黄金向量也在内）
cargo clippy --workspace --all-targets -- -D warnings
```

aarch64 交叉编译（目标设备）：

```bash
# 一次性安装：rustup target add aarch64-unknown-linux-gnu; pip install ziglang; cargo install cargo-zigbuild
export PATH="$PATH:$(python -c "import ziglang,os;print(os.path.dirname(ziglang.__file__))")"
cargo zigbuild --release --target aarch64-unknown-linux-gnu -p o5-app
# 产物 target/aarch64-unknown-linux-gnu/release/o5-harness-rs
```

## 部署（设备 /root/o5_harness_rs，与 Python 版 /root/o5_harness 共存便于 A/B）

```bash
python deploy/deploy.py              # 编译 + 上传二进制/喂流/资产/配置模板
python deploy/deploy.py --systemd    # 安装 headless 服务（Restart=always）
python deploy/deploy.py --app       # 安装为 MaixPy launcher 应用
python deploy/deploy.py --smoke      # headless 冒烟 45s（自动停/重启 launcher）
```

连接参数 `O5_SSH_HOST/O5_SSH_USER/O5_SSH_PASS`（默认 10.239.93.1 root/sipeed）。

## 验证口径（与 Python 版 A/B）

| 指标 | Python 版实测 | Rust 版目标 |
|---|---|---|
| 麦克风间隙 | 满载 17 次/10s（最大 1.9s，丢 40%） | 0 |
| UI 帧率（含相机预览） | 掉到 2.2fps | 稳定 15fps |
| 事件循环滞后 | 数百 ms | 无事件循环 |
| CPU（UI+全模态） | 99-108% | <60% |
| 退出挂死 | 看门狗 + os._exit 绕 | 退出看门狗线程（语义保留） |

## 与 Python 版的已知偏差

- 显示与摄像头通过 MaixPy 喂流子进程（纯 Rust 直写 fbdev 不可行：fb0 是 vfb；
  摄像头无 V4L2 节点——探测记录见 docs/device-findings.md）。喂流只做 memcpy 级转发，
  重活全部在 Rust 侧。
- 触摸横屏映射默认 variant 2（device 2 实测），本设备可用 `device.touchVariant` 配置。
- 详见各 crate 报告的"有意偏差"清单（ docs/CONTRACTS.md 附带约束）。
