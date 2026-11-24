# MaixCAM2 设备探测结论（2026-11-05，device 03 @ 10.239.93.1）

所有 agent 实现设备相关代码前必读。原始探测脚本见仓库根 `probe_device.py` / `probe2.py` / `probe3_local.py`。

## 设备概况

- SoC: AX630C，2× Cortex-A53 @1.2GHz（`nproc`=2），内核 4.19.125 aarch64，Ubuntu 22.04（glibc 2.35）
- 内存: 447MB total / ~330MB available
- MaixPy 4.12.5（Python 3.13.2），launcher.service 常驻（拥有 VO 显示所有权）
- 设备上无 /dev/video*、无 zig；gcc 11.4 有
- SFTP 可用（sshx.py 所述"受限"已不成立，部署直接 sftp.put）
- root/sipeed，SSH 10.239.93.1

## 显示（关键决策）

- **/dev/fb0 是 "vfb"（Virtual FB，/sys/devices/platform/vfb@0），MaixPy Display 不写它**。
  实测：停 launcher → MaixPy `disp.show()` 红屏 → fb0 内容不变（全零）。
  MaixPy 渲染路径 = `display.Display()` → SAMPLE-VO 硬件（Win-info 480x640）→ CMM pool 4
  （4 blocks × 0x70800，NV12）→ MIPI DSI 面板。
- **结论：Rust 不能直接写 fbdev 上屏。显示走 MaixPy 喂流子进程**：
  Rust 合成整帧 640x480 RGB888 → stdin 管道 → `deploy/feeder_display.py`
  （`image.Image` from bytes + `disp.show()`）。
- fb1（64x64）无关，忽略。
- 停 launcher 后 SSH 会话内 Display() 初始化成功（fbtest.py 全程无错），背光 pwm 由 MaixPy 自动处理。
- 喂流进程协议：Rust → stdin 写 `[u32 LE len][RGB888 640*480*3 字节]`；
  feeder 每收一帧 show 一帧（show 内部节拍到显示刷新）。feeder 需处理 SIGTERM 优雅退出并 close display。
- **注意**：UI 运行需 launcher 停止（或作为 launcher app 启动）；headless 模式不需要显示。

## 触摸

- 设备 /dev/input/event1，`hyn_ts`（Hynitron 电容屏），MT type-B 协议
- input_event 结构：64-bit 时间戳（struct `qqHHi`，16 字节 pad 到 24，16 字节格式会 EINVAL）
- EVIOCGABS 实测（s32×6）：
  - MT_SLOT: 0..5；MT_TRACKING_ID: 0..5；MT_PRESSURE: 0..255
  - **MT_POSITION_X: 0..480；MT_POSITION_Y: 0..640 —— 原始坐标是竖屏空间（宽480×高640）**
- 屏幕逻辑坐标 640x480 横屏 → 映射需 8 变体之一（swap/flip 组合），原项目 tap_test.py 对
  device 2 判定 variant 2。**本设备（03）映射未标定**：实现为可配置（config `device.touchVariant`，
  默认 2），并提供 /tmp 下写映射结果的自检模式。BTN_TOUCH(0x14a)/BTN_TOOL_FINGER(0x145)+SYN_REPORT。
- 事件读取：纯 read() 即可（无 ioctl 依赖，范围用上面实测值），去抖/按下-抬起沿判定与 Python 版一致。

## 音频（关键决策）

- ALSA card 0 "Audio"（Axera）。**采集 = `plughw:CARD=Audio,DEV=0`（i2s_slv+audio_codec）**，
  **播放 = `plughw:CARD=Audio,DEV=1`**（与 Python 版 `plughw:0,1` 一致）。
- 硬件原生参数（hw 层）：仅 2 声道、S16/S24/S32_LE、8000-192000Hz —— plughw 负责转换。
- **实测**：`arecord -q -D plughw:CARD=Audio,DEV=0 -d 3 -r 16000 -f S16_LE -c 1` → 96044 字节
  （96000 数据 + 44 头）= 精确 3.0s ✓
- Rust 侧：`arecord`/`aplay` 子进程方案（stdin/stdout 管道，参数同 Python 版 audio_io.py），
  藏在 `CaptureSource`/`PlaybackSink` trait 后，将来可换直接 ALSA。
  arecord 参数：`-q -D <dev> -r 16000 -f S16_LE -c 1 --buffer-size=1600 --period-size=400`
  （实测加 `--buffer-size/--period-size` 参数被接受，缓存更小唤醒更准；
  若启动失败回退无这些参数的版本）。
  aplay 参数：`-q -D plughw:0,1 -r <rate> -f S16_LE -c 1`。

## 摄像头（关键决策）

- **设备上没有 /dev/video\*、/dev/media\*** —— 摄像头完全走 libmaix 私有 VI 管线，V4L2 不可用。
- **结论：摄像头走 MaixPy 喂流子进程**：`deploy/feeder_camera.py`
  （`camera.Camera(w,h,FMT_RGB888)` + `set_fps(30)` + **`exp_mode(AeMode.Auto)`（set_fps 后必须，
  否则曝光 Manual 增益拉爆 → 白屏）** + `read()`）。
- 喂流协议：stdout 写 `[u32 LE len][RGB888 w*h*3]`，约 10-15fps 全速推帧，Rust 侧按需取用
  （1fps 上行 JPEG 编码 + 8Hz 预览缩放都在 Rust 内做）。
- hmirror/vflip 由 feeder 参数处理（命令行传入）。

## 字体

- `/maixapp/share/font/SourceHanSansCN-Regular.otf`（8.3MB，思源黑体）存在；
  候选列表同 Python 版 ui.py：先 config 指定 → /maixapp/share/font/SourceHanSansCN-Regular.otf →
  随二进制部署的 assets 字体兜底。

## 设备当前部署形态（对齐目标）

- /root/o5_config.json：mode=audio、protocolVersion=4、ASR enabled、echoMode=subband、
  micDuck=0.2、maxSessionMinutes=15、background=qwen3.7-plus enabled、video 640x480（代码默认 512x384）
- Rust 版部署路径：`/root/o5_harness_rs/`（二进制 + assets + feeders + config），与 Python 版
  `/root/o5_harness/` 共存，便于 A/B。

## 调度策略（Rust 版核心优化）

- mic/speaker 线程尝试 `SCHED_RR` rtprio=10（root 下应成功；EPERM 则回退 `nice(-5)`），
  `#[cfg(target_os="linux")]` 门控，Windows 测试构建走 no-op。
- 摄像头 feeder 子进程 `nice +5`。
- arecord/aplay 由内核管道唤醒（无轮询无 GIL）。

## 2× 采样率定律（02 实测，2026-11-06）

- **设备 02** 的 `plughw:CARD=Audio,DEV=0` 在 `-r 16000 -c 1` 下实际产出 64718 B/s ≈
  2.011× 请求速率（驱动把 8k 帧率跑成 16.07k，2 声道交织）。
- 正确姿势（现行实现）：`arecord -r 8000 -c 2` + **StereoToMono 抽左声道** → 精确实时 16k
  单声道（15974.8 样本/s，慢 0.16%，由 far servo 消化，见下）。
- `device.rs` 启动时**码流校准**：Drain（排掉 arecord 启动积压）→ Measure →
  速率 ≈2× 则自动切 8000/2ch 抽声道；速率正常则维持 16000/1ch。respawn 同流程。
- 03 未复现（`-r 16000 -c 1` 精确 96000+44B/3s）——同型不同命，校准兜底必要。

## 回声链路三层修复（02 实测收敛）

问题链（dump 实证，`/root/o5_harness_rs/o5_dbg`）：扬声器→麦克风近场耦合 + 麦克风
×3.98 软增益 → 回环增益 +14~20dB（回声比 far 参考还响）；far/near 相对时钟漂移
0.55%/s（arecord -0.16% + aplay ≈-0.4%）→ 延迟锁持续漂移 → subband AEC 抑制跌到
0.3dB → 服务器 VAD 误判抢话 → 自触发循环。Python 版不炸是因为 MaixPy Recorder
（Axera 管线）时钟精确且自带 AGC（纯音 4259→210）。

修复三层（`o5-audio`）：

1. **近端 AGC**（`agc.rs`）：20ms 子块下压压缩器，阈值 4500（dump 实测人声峰值
   ≤4037、回声均值 ~5000、噪声 p95=154），快攻（0.35）慢放（0.005，4s 时间常数，
   词间隙不释放），只下压不上放。配置项 `audio.nearAgc`（默认 true）。
2. **far 时钟伺服**（`agc.rs` `FarServo`）：5s 观测窗 EMA（α=0.4，±5% 离群剔除）
   估计 far/near 时钟比，对全部 far 数据线性插值重采样对表；静音 keepalive 同样过
   伺服。延迟锁 312ms 稳定（此前 24.6s 内漂 921.8→787.3ms）。
3. **Drain 校准**（见上）保证 respawn 后量测不被管道积压污染。

效果：口播完整、AEC 抑制 4.2~5.2dB（超 Python 基线 4.6dB）、无自触发循环。

## 打断链路（cpm-harness 让位语义，2026-11-06 二次修正）

`spoken.end` 的四种形态（cpm-harness interaction-bridge 口径，全部真机事故换来的）：

| reason | 语义 | 端上动作 |
|---|---|---|
| `turn_eos` | 回合正常结束 | final 口播；**不清**播放（积压自然排空） |
| `listen` **且口播中** | **让位**（被用户插话打断、让出话轮） | 清播放积压快停（Interrupted） |
| `listen` **且空闲** | 每 unit 常规收据（空闲期每秒一条） | 忽略 |
| `slot_eos` / `tts_pad` | unit 内收束，口播延续 | 忽略 |

- 事故 1（v4 时代）：listen 无条件当打断 → 空闲收据每秒误清 →「说一半就没了」。
- 事故 2（v5 透明池）：任何 spoken.end 都复位 speaking → 口播中每 unit 的
  slot_eos 复位 → 后续 delta 误判新回合 → 每秒误清 →「重复一句话」（每清一次
  从头重播）+ aplay 每秒重建爆音。
- v5 透明池行为：口播中每 unit 一条 `spoken.end`（unit 粒度收据），回合仍只有
  turn_eos 收束；让位时转 listen（`listen && speaking` 才是让位判定）。
- **快停机制**（`audio_io.rs`）：`SpkItem::Flush` 哨兵——清队列 + 作废当前合块 +
  重建 aplay（立即静音），播放线程保持运行。app 端 `playing(0.5)` 兜底：让位收据
  到达时积压恰好刚播完就不必重建（免一次爆音）。队列双上限（120 块 /
  24k×2×3 B 字节水位）防积压无界。
- 集成测试 `spoken_end_interruption_semantics`（session_pc.rs）：空闲 listen 忽略、
  slot_eos 忽略、让位触发、turn_eos 不清、让位后 listen 回到忽略。
- v5 协议（auto 协商生效后）：URL mode 只认 audio/omni、init 带 `input_mode`、
  `standalone_event` 发裸文本；`protocolVersion: "auto"` → 连接前 GET `/v1/info`
  协商（透明池 backend v5+init_passthrough → 说 v5）。公网调用开关
  （`network.publicNetwork`，更多菜单）：前台端点域名换 `o5gw.modelbest.cn`。
- 日志双份坑：logger 内部已写文件队列，启动命令 stdout 别 `>>` 同一个日志文件
  （重定向 console.log）。nohup 必须 `< /dev/null`，且 paramiko 信道会被后台进程
  挂住（读超时但进程正常，重连查即可）。

## ASR 重复上屏 + 偶发爆音（02 实测，2026-11-06 三修）

**「用户句在 AI 回复后再出现一次」**：日志铁证 = 每条 `[ASR 用户]` 都在
`[口播回合]` 后 2-4s、全程零条 `[ASR 回声过滤]`。机理：ASR gate（口播期间
不 feed）造成**断流**，服务端把残余解码缓冲再 flush 一条重复 final；加上
ASR channel 有 ≤10 unit 积压（慢网/公网下转写迟到数秒）。迟到 final 到达时
口播刚结束，`echo_filtered` 的 `playing(1.5)` 时机门已关、且重复的是用户话
（与 model 行 bigram Jaccard ≈0.05，阈值 0.5 拦不住——Python「干净回声
0.6+」的前提是长句完整回声，AEC 残留短转写不成立），`user_line_open=false`
→ `add_user` push 重复行。修复两层（`app.rs`）：

1. **gate 改喂等长静音而非断流**：流连续 → 服务端不触发流结束 flush，
   回声也压根进不了 ASR（音频层面解决，echo_filtered 退回防门沿漏网）。
2. **迟到 final 去重**（`user_dup_filtered`）：距上一条 user final <4s、
   与最近 user 行（隔着 model 行也要找到）bigram >0.6 → 丢弃。间隔长的
   是用户真在重复问，放行。实测语料：「你现在能什么？现在能看到什么？」
   vs 迟到「你现在能看到什么？」= 0.8 拦下；改问新内容 <0.6 放行。

**偶发爆音**：aplay 重建后新流**首块从任意振幅起播** → click（打断让位
快停后的主要残留）。修复：`ensure_sink` 返回「是否重建」（**坑**：速率切换
重建时 sink 先置 None 再建，外部的 had_sink 检测看不出来，必须由 ensure_sink
自己报告），重建后首块头部 ~3ms 线性淡入（`fade_in_head`，覆盖首次启动 /
Flush 两处 / 写入错误 / 速率切换）。停侧 click 不可控（数据已进硬件缓冲）。
后续连续流不做块缘淡变（周期性凹陷，既有设计）。

## 设备音频标定表（device.label，2026-11-09）

同型机器（02/03）硬件个体差异大，音频参数**不写死全局默认**：
`o5-config::device_audio_profile(label)` 按设备编号查差异化默认，优先级 =
**设备 json 显式值 > 标定表 > 全局默认**。新机器实测出回声特征后往表里
加一行。目前表内容：

| label | echoMaxDelayMs | micDuck | 依据 |
|---|---|---|---|
| 03 | 1600 | 0.2 | 回声延迟 ~975ms 贴默认 1000ms 窗口顶（AEC 锁不住）；近场耦合强 |
| （其他） | 1000（默认） | 0.4（默认） | 02 实测延迟 ~312ms，默认窗口富余 |

- **坑**：`defaults_value()` 里不能放这两个键——defaults merge 会把键
  伪装成"用户显式值"压过标定表（单测逮到后已移除）。
- **"AI 说到一半就没了"（03）**：回声延迟贴顶 → AEC 全程未收敛（抑制
  0.0dB）→ 回声进麦克风被服务器 VAD 当插话 → 让位收回话轮 → 口播腰斩。
- **mute 期 AEC 时间轴坑**：静音时 near 若不过 AEC（时间轴停走）而 far
  keepalive 持续推进 → far 环形缓冲覆盖对齐窗 → 解除静音"环形缓冲读取
  失败"、锁定全掉。修复：gate/mute 期 near 零进零出过 `cancel()`。

## 03 多媒体/launcher 坑（2026-11-09，固件 2025-06-01 版）

03 与 02 同型但固件行为不同，四个连环坑（每个都实测+修复）：

1. **launcher_daemon 残留占 IVPS 组 0**：桌面退出后 daemon（GUI 守护壳，
   同一进程）继续持有摄像头管线 → feeder `vi init failed`
   （`AX_IVPS_CreateGrp 0x800d0115`）。对照实验：杀 daemon → 立即可用。
   02 无此行为。
2. **杀 daemon 必须 SIGKILL**：daemon 的 TERM 处理器（`on_signal` →
   `stop_run_app`）会在退出前把运行中的 app 按 pid 杀掉——cgroup 迁移 +
   setsid 全逃也没用（它按 pid 直杀）。且 app 是 GUI fork 的、在
   launcher.service cgroup 里，escape（迁根 cgroup + setsid）必须在
   **main 入口**（App 构造的字体加载阶段也会被 systemd 清 unit 连坐）。
3. **display feeder 先 init 也占 IVPS 组 0**：camera 后起必败；反向顺序
   （camera 先占、display 后起）实测共存。修复：omni 模式构造尾部预拉
   camera（赶在 UI 的 display feeder 前）；切全模时先杀 display 让位
   （FeederSink 投递线程断开后 2s respawn 自愈，≤8 次）。
4. **libmaix native 日志污染帧流**：C 层 printf 直写 fd 1（`-- [I] [Maix
   Comm Protocol]...`、vi init 的 ERROR），Python 层拦不住。feeder 的 fd
   换位（管道挪备用 fd、fd1→/dev/null）必须在 **import maix 之前**——
   Camera() 构造期的日志同样污染（失步字节 "\r\n\r[" 即其错误日志前缀）。

**网络**：03 经 USB 直连管理（usb0 10.239.92.1 / 10.239.93.1 两个网段都
见过），WiFi 走 ModelBest-IoT（192.168.11.x，与办公网同段）。
