import paramiko, base64, time, sys

cli = paramiko.SSHClient()
cli.set_missing_host_key_policy(paramiko.AutoAddPolicy())
cli.connect("10.239.93.1", 22, "root", "sipeed", timeout=8)

def run(cmd, timeout=90):
    _, out, err = cli.exec_command(cmd, timeout=timeout)
    return out.read().decode("utf-8","replace"), err.read().decode("utf-8","replace")

# upload feeders
for name in ["feeder_display.py", "feeder_camera.py"]:
    with open(f"deploy/{name}", "rb") as f:
        b64 = base64.b64encode(f.read()).decode()
    run(f"echo {b64} | base64 -d > /root/{name} && chmod +x /root/{name}")

# --- camera feeder test: 6 seconds, count framed RGB output ---
print("== camera feeder test (6s @512x384, fps counter) ==")
driver = '''
import subprocess, struct, sys, time, os
p = subprocess.Popen(["python3", "/root/feeder_camera.py", "512", "384", "--fps", "30"],
                     stdout=subprocess.PIPE, stderr=subprocess.PIPE)
t0 = time.time(); n = 0; total = 0
p.terminate_after = None
import threading
def killer():
    time.sleep(6.0)
    p.terminate()
threading.Thread(target=killer, daemon=True).start()
try:
    while time.time() - t0 < 7.5:
        hdr = p.stdout.read(4)
        if not hdr or len(hdr) < 4:
            break
        (ln,) = struct.unpack("<I", hdr)
        data = p.stdout.read(ln)
        if len(data) < ln:
            break
        n += 1; total += ln
except Exception as e:
    print("reader err:", e)
try:
    p.wait(timeout=3)
except Exception:
    p.kill()
dt = time.time() - t0
print(f"frames={n} bytes={total} fps={n/max(dt-1,0.001):.1f} frame_len_expect={512*384*3}")
print("stderr:", p.stderr.read().decode("utf-8","replace")[:500])
'''
run(f"echo {base64.b64encode(driver.encode()).decode()} | base64 -d > /tmp/drv_cam.py")
o, e = run("python3 /tmp/drv_cam.py", timeout=60)
print(o or e)

# --- display feeder test: 30 solid frames, verify consumption rate + no error ---
print("== display feeder test (30 frames 640x480) ==")
driver2 = '''
import subprocess, struct, time
p = subprocess.Popen(["python3", "/root/feeder_display.py", "640", "480"],
                     stdin=subprocess.PIPE, stderr=subprocess.PIPE)
frame = bytes([255, 0, 0]) * (640 * 480)
t0 = time.time()
for i in range(30):
    p.stdin.write(struct.pack("<I", len(frame)))
    p.stdin.write(frame)
    p.stdin.flush()
p.stdin.close()
try:
    p.wait(timeout=10)
except Exception:
    p.kill()
dt = time.time() - t0
print(f"30 frames written+consumed in {dt:.2f}s -> {30/dt:.1f}fps sustained")
print("stderr:", p.stderr.read().decode("utf-8","replace")[:500])
'''
run(f"echo {base64.b64encode(driver2.encode()).decode()} | base64 -d > /tmp/drv_disp.py")
o, e = run("systemctl stop launcher.service && sleep 1 && python3 /tmp/drv_disp.py; systemctl start launcher.service", timeout=120)
print(o or e)
o2, _ = run("systemctl is-active launcher.service")
print("launcher:", o2.strip())
cli.close()
