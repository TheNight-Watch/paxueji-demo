import paramiko, base64, time

SCRIPTS = {}

SCRIPTS["abs_probe.py"] = '''
import fcntl, struct
fd = open("/dev/input/event1", "rb")
# input_absinfo: 6 x s32 = 24 bytes; EVIOCGABS = _IOR('E', 0x40+abs, 24)
for name, ax in [("MT_SLOT",0x2f),("MT_POSITION_X",0x35),("MT_POSITION_Y",0x36),
                 ("MT_TRACKING_ID",0x39),("MT_PRESSURE",0x3a),("MT_TOUCH_MAJOR",0x30),
                 ("MT_WIDTH_MAJOR",0x32)]:
    buf = bytearray(24)
    req = (2 << 30) | (24 << 16) | (0x45 << 8) | (0x40 + ax)
    try:
        fcntl.ioctl(fd, req, buf, True)
        v, mn, mx, fuzz, flat, res = struct.unpack("iiiiii", bytes(buf))
        print(name, "value=%d min=%d max=%d fuzz=%d flat=%d res=%d" % (v, mn, mx, fuzz, flat, res))
    except OSError as e:
        print(name, "ERR", e)
fd.close()
'''

SCRIPTS["fbtest.py"] = '''
import os, sys, time
import numpy as np
FB = "/dev/fb0"
def read_fb(n=480*640*4):
    fd = os.open(FB, os.O_RDONLY)
    d = os.read(fd, n)
    os.close(fd)
    return d
before = read_fb()
print("fb0 before: nonzero_bytes=%d" % (np.frombuffer(before, dtype=np.uint8) != 0).sum())
# maixpy display test
from maix import image, display
d = display.Display()
print("device path:", d.device())
img = image.Image(640, 480, image.Format.FMT_RGB888)
img.draw_rect(0, 0, 640, 480, image.Color.from_rgb(255, 0, 0), thickness=-1)
d.show(img)
time.sleep(1.0)
after = read_fb()
print("fb0 after: nonzero_bytes=%d" % (np.frombuffer(after, dtype=np.uint8) != 0).sum())
print("fb0 changed:", before != after)
if before != after:
    a = np.frombuffer(after, dtype=np.uint8).reshape(640, 480, 4)
    print("fb0 corners BGRA:", a[0,0], a[0,-1], a[-1,0], a[-1,-1])
    print("fb0 mid:", a[320, 240])
    # which half changed? (double buffer: offset 0 or 1228800)
    h1 = (np.frombuffer(before, dtype=np.uint8) != 0).sum()
    print("fb0 second-half nonzero:", (np.frombuffer(after, dtype=np.uint8)[1228800:] != 0).sum())
d.close()
print("OK")
'''

cli = paramiko.SSHClient()
cli.set_missing_host_key_policy(paramiko.AutoAddPolicy())
cli.connect("10.239.93.1", 22, "root", "sipeed", timeout=8, banner_timeout=8, auth_timeout=8)

def run(cmd, timeout=60):
    _, out, err = cli.exec_command(cmd, timeout=timeout)
    return out.read().decode("utf-8", "replace").strip(), err.read().decode("utf-8", "replace").strip()

# upload scripts via base64 (SFTP restricted)
for name, body in SCRIPTS.items():
    b64 = base64.b64encode(body.encode()).decode()
    run("echo %s | base64 -d > /tmp/%s" % (b64, name))
print("== uploaded ==")

print("== abs probe ==")
print(run("python3 /tmp/abs_probe.py")[0])

print("== arecord exact 3s ==")
print(run("rm -f /tmp/t3.wav; arecord -q -D plughw:CARD=Audio,DEV=0 -d 3 -r 16000 -f S16_LE -c 1 /tmp/t3.wav && ls -la /tmp/t3.wav")[0])

print("== hw params ==")
print(run("arecord -D hw:CARD=Audio,DEV=0 --dump-hw-params -d 1 /dev/null 2>&1 | head -30")[0])

print("== fb0 sysfs device ==")
print(run("ls -la /sys/class/graphics/fb0/device 2>&1; readlink -f /sys/class/graphics/fb0/device 2>&1; ls /sys/class/graphics/fb0/")[0])

print("== STOP launcher ==")
print(run("systemctl stop launcher.service && sleep 1 && systemctl is-active launcher.service")[0])
try:
    print("== maixpy display -> fb0 test ==")
    print(run("python3 /tmp/fbtest.py", timeout=90))
finally:
    print("== RESTART launcher ==")
    print(run("systemctl start launcher.service && sleep 1 && systemctl is-active launcher.service")[0])
    print(run("ps aux | grep launcher | grep -v grep")[0])
cli.close()
