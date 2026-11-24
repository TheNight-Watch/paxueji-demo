"""One-shot device probe: fbdev / V4L2 / ALSA / touch / misc facts for the Rust port."""
import paramiko, json, sys

HOST, USER, PASS = "10.239.93.1", "root", "sipeed"
cli = paramiko.SSHClient()
cli.set_missing_host_key_policy(paramiko.AutoAddPolicy())
cli.connect(HOST, 22, USER, PASS, timeout=8, banner_timeout=8, auth_timeout=8)

def run(cmd, timeout=15):
    _, out, err = cli.exec_command(cmd, timeout=timeout)
    o, e = out.read().decode("utf-8", "replace"), err.read().decode("utf-8", "replace")
    return o.strip(), e.strip()

print("== uname =="); print(run("uname -a")[0])
print("== cpuinfo =="); print(run("grep -m1 -E 'model|Hardware' /proc/cpuinfo; nproc; grep -c processor /proc/cpuinfo; free -m | head -2")[0])
print("== fbdev nodes =="); print(run("ls -la /dev/fb* 2>&1")[0])
print("== fb0 mode (var) =="); print(run("cat /sys/class/graphics/fb0/mode 2>/dev/null; cat /sys/class/graphics/fb0/virtual_size 2>/dev/null; cat /sys/class/graphics/fb0/bits_per_pixel 2>/dev/null")[0])
print("== v4l2 nodes =="); print(run("ls -la /dev/video* /dev/media* 2>&1")[0])
for d in run("ls /dev/video* 2>/dev/null")[0].split():
    print(f"== {d} formats ==")
    print(run(f"v4l2-ctl -d {d} --list-formats-ext 2>&1 | head -40 || echo no-v4l2-ctl")[0])
    print(f"== {d} info ==")
    print(run(f"v4l2-ctl -d {d} -D 2>&1 | head -15")[0])
print("== alsa cards =="); print(run("cat /proc/asound/cards")[0])
print("== alsa pcms =="); print(run("arecord -L 2>/dev/null | head -25; echo ---; aplay -L 2>/dev/null | grep -E 'plughw|hw:' | head -10")[0])
print("== aplay/arecord binaries =="); print(run("command -v aplay arecord v4l2-ctl; ls /usr/share/alsa")[0])
print("== input devices =="); print(run("cat /proc/bus/input/devices")[0])
print("== fonts =="); print(run("ls -la /maixapp/share/font/ 2>&1 | head -10")[0])
print("== python/maix =="); print(run("python3 --version; python3 -c 'import maix; print(maix.__version__)' 2>&1")[0])
print("== app dirs =="); print(run("ls /maixapp/apps/ 2>/dev/null; ls /root/o5_harness 2>/dev/null | head; ls /root/o5_config.json 2>&1")[0])
print("== zig/cc on device =="); print(run("command -v gcc g++ cc python3; gcc --version 2>/dev/null | head -1")[0])
cli.close()
