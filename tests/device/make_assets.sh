#!/usr/bin/env bash
# 在 macOS 上生成真机测试素材：物体照片（取自系统自带的用户头像图片）与提问录音（系统自带语音）。
# 这些素材版权归 Apple，不随仓库分发，跑测试前在本机生成一次即可。依赖：sips、say、afconvert（系统自带）、Pillow。
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
PICS="/Library/User Pictures"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$HERE/images" "$HERE/audio"

for pair in "Animals/Penguin:企鹅" "Flowers/Sunflower:向日葵" "Instruments/Violin:小提琴" "Nature/Cactus:仙人掌" \
            "Nature/Leaf:树叶" "Animals/Owl:猫头鹰" "Sports/Basketball:篮球" "Animals/Zebra:斑马"; do
  sips -s format png "$PICS/${pair%%:*}.heic" --out "$TMP/${pair##*:}.png" >/dev/null
done
python3 - "$TMP" "$HERE/images" <<'PY'
import os, sys
from PIL import Image, ImageFilter
src, out = sys.argv[1], sys.argv[2]
for f in os.listdir(src):
    im = Image.open(os.path.join(src, f)).convert("RGB")
    s = min(im.size)
    im = im.crop(((im.width - s) // 2, (im.height - s) // 2, (im.width + s) // 2, (im.height + s) // 2)).resize((440, 440))
    c = Image.new("RGB", (640, 480), (205, 200, 190))
    c.paste(im, (100, 20))
    c.save(os.path.join(out, f))
    if f == "向日葵.png":
        c.filter(ImageFilter.GaussianBlur(9)).save(os.path.join(out, "模糊向日葵.png"))
PY

say -v "Flo (中文（中国大陆）)" -o "$TMP/q.aiff" "小松果老师，企鹅吃什么呀？"
say -v "Eddy (中文（中国大陆）)" -r 170 -o "$TMP/a.aiff" "这个方案的排版保持不变，把右边的标题字体全部换成正文字体，周五之前发给我。"
for pair in q:q_penguin_food a:ambient_adults; do
  afconvert -f WAVE -d LEI16@16000 -c 1 "$TMP/${pair%%:*}.aiff" "$TMP/${pair##*:}.wav"
  python3 -c 'import wave,sys; w=wave.open(sys.argv[1]); open(sys.argv[2],"wb").write(w.readframes(w.getnframes()))' \
    "$TMP/${pair##*:}.wav" "$HERE/audio/${pair##*:}.pcm"
done
echo "素材已生成：$HERE/images、$HERE/audio"
