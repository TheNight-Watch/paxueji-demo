#!/usr/bin/env bash
# 拍学机一键部署：交叉编译 → 打包成 MaixPy launcher 应用 → 装到 /maixapp/apps/paxueji。
#
#   deploy/deploy.sh               编译并安装
#   deploy/deploy.sh --no-build    只安装
#   deploy/deploy.sh --config      同时重写设备配置（需 PAXUEJI_O5_API_KEY）
#
# 设备配置 /root/paxueji_config.json 不存在时自动用模板生成，Key 只从环境变量读，不进仓库。
set -euo pipefail

HOST=${PAXUEJI_HOST:-10.15.125.1}
APP_DIR=/maixapp/apps/paxueji
DEVICE_CONFIG=/root/paxueji_config.json
ROOT=$(cd "$(dirname "$0")/.." && pwd)
ZIG_DIR=${ZIG_DIR:-$HOME/.local/zigvenv/lib/python3.14/site-packages/ziglang}
SIGNATURE_DIR=${PAXUEJI_ASSETS:-}   # 可选：存放 MiniCPM 标志音色 ref_minicpm_signature_fix.wav 的目录
SSH=(ssh -o BatchMode=yes -o ConnectTimeout=5 "root@$HOST")

build=1 write_config=0
for a in "$@"; do
  case $a in
    --no-build) build=0 ;;
    --config) write_config=1 ;;
    *) echo "未知参数 $a" >&2; exit 2 ;;
  esac
done

BIN=$ROOT/target/aarch64-unknown-linux-gnu/release/o5-harness-rs
if [[ $build == 1 ]]; then
  (cd "$ROOT" && PATH="$PATH:$ZIG_DIR" cargo zigbuild --release --target aarch64-unknown-linux-gnu -p o5-app)
fi
[[ -x $BIN ]] || { echo "找不到二进制 $BIN" >&2; exit 1; }
# 音色：仓库自带开源中文音色（assets/voices，默认 zh-female-bright）。MiniCPM 标志音色是内部素材、
# 不随仓库分发；PAXUEJI_ASSETS 指向它所在目录，或板子上装过「A0 O5 Voice」时，会一并带上。
BOARD_SIGNATURE=/maixapp/apps/o5_harness_rs/assets/ref_minicpm_signature_fix.wav
SIGNATURE=""
if [[ -n $SIGNATURE_DIR && -f $SIGNATURE_DIR/ref_minicpm_signature_fix.wav ]]; then
  SIGNATURE=$SIGNATURE_DIR/ref_minicpm_signature_fix.wav
elif "${SSH[@]}" "test -f $BOARD_SIGNATURE"; then
  SIGNATURE=board
fi

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cp "$BIN" "$ROOT"/deploy/{feeder_display.py,feeder_camera.py,app.yaml,main.py,icon.png} "$stage/"
mkdir "$stage/assets"
cp "$ROOT"/assets/voices/*.wav "$stage/assets/"
[[ -n $SIGNATURE && $SIGNATURE != board ]] && cp "$SIGNATURE" "$stage/assets/"

"${SSH[@]}" "pkill -f '$APP_DIR/o5-harness-r[s]' 2>/dev/null; mkdir -p $APP_DIR; true"
scp -q -o BatchMode=yes -r "$stage"/. "root@$HOST:$APP_DIR/"
[[ $SIGNATURE == board ]] && "${SSH[@]}" "cp $BOARD_SIGNATURE $APP_DIR/assets/"
"${SSH[@]}" "chmod 755 $APP_DIR && chmod +x $APP_DIR/o5-harness-rs $APP_DIR/main.py && python3 /maixapp/apps/gen_app_info.py >/dev/null && sync"

if [[ $write_config == 1 ]] || ! "${SSH[@]}" "test -f $DEVICE_CONFIG"; then
  [[ -n ${PAXUEJI_O5_API_KEY:-} ]] || { echo "设备上还没有配置，请设 PAXUEJI_O5_API_KEY 后重跑" >&2; exit 1; }
  python3 -I -c 'import json,sys; c=json.load(open(sys.argv[1])); c["foreground"]["apiKey"]=sys.argv[2]; json.dump(c,sys.stdout,ensure_ascii=False,indent=2)' \
    "$ROOT/deploy/paxueji.config.json" "$PAXUEJI_O5_API_KEY" > "$stage/config.json"
  scp -q -o BatchMode=yes "$stage/config.json" "root@$HOST:$DEVICE_CONFIG"
  "${SSH[@]}" "chmod 600 $DEVICE_CONFIG"
  echo "已写入设备配置 $DEVICE_CONFIG"
fi
# 部署会杀掉正在跑的拍学机；它启动时击杀了桌面守护进程，这里把桌面拉回来，屏幕才不会停在旧画面
"${SSH[@]}" "pgrep -f launcher_[d]aemon >/dev/null || systemctl start launcher.service"
echo "已安装到 $HOST:$APP_DIR，在桌面点「拍学机」启动"
