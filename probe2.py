import paramiko

cli = paramiko.SSHClient()
cli.set_missing_host_key_policy(paramiko.AutoAddPolicy())
cli.connect("10.239.93.1", 22, "root", "sipeed", timeout=8, banner_timeout=8, auth_timeout=8)

def run(cmd, timeout=30):
    _, out, err = cli.exec_command(cmd, timeout=timeout)
    return out.read().decode("utf-8", "replace").strip(), err.read().decode("utf-8", "replace").strip()

print("== fb names/modes ==")
print(run("for f in /sys/class/graphics/fb*; do echo $f: $(cat $f/name 2>/dev/null) mode=$(cat $f/mode 2>/dev/null) vsz=$(cat $f/virtual_size 2>/dev/null) bpp=$(cat $f/bits_per_pixel 2>/dev/null) blank=$(cat $f/blank 2>/dev/null); done")[0])
print("== fb0 content check (numpy) ==")
print(run("python3 -c \"import numpy as np; a=np.fromfile('/dev/fb0', dtype=np.uint8, count=480*640*4); a=a.reshape(640,480,4); print('shape',a.shape,'mean',a.mean(),'std',a.std(),'uniq16',len(np.unique(a[::1000])))\"")[0])
print("== fb1 content check ==")
print(run("python3 -c \"import numpy as np; a=np.fromfile('/dev/fb1', dtype=np.uint8, count=480*640*4); print('mean',a.mean(),'std',a.std())\" 2>&1")[0])
print("== touch abs (EVIOCGABS) ==")
print(run("python3 -c \"\nimport fcntl,struct\nfd=open('/dev/input/event1','rb')\nfor name,ax in [('X',0x35),('Y',0x36),('SLOT',0x2f),('TRACK',0x39),('PRESS',0x3a)]:\n    buf=bytearray(24)\n    try:\n        fcntl.ioctl(fd,(2<<30)|(24<<16)|(0x45<<8)|(0x40+ax),buf,True)\n        v=struct.unpack('qqqqqq',bytes(buf))\n        print(name,v)\n    except OSError as e: print(name,'ERR',e)\n\"")[0])
print("== arecord 2s test 16k mono S16 ==")
print(run("timeout 3 arecord -q -D plughw:CARD=Audio,DEV=0 -r 16000 -f S16_LE -c 1 --buffer-size=1600 --period-size=400 /tmp/t16k.wav 2>&1; ls -la /tmp/t16k.wav; python3 -c \"import wave; w=wave.open('/tmp/t16k.wav'); print('frames',w.getnframes(),'rate',w.getframerate(),'ch',w.getnchannels(),'width',w.getsampwidth())\"")[0])
print("== running processes ==")
print(run("ps aux | grep -E 'launcher|python|o5' | grep -v grep")[0])
print("== systemd ==")
print(run("systemctl list-units --type=service --state=running | grep -Ei 'launcher|maix|o5|getty' ; systemctl is-active o5-harness 2>&1")[0])
print("== ax_pool ==")
print(run("ls -la /dev/ax_pool /dev/dsp /dev/gnl 2>&1")[0])
print("== device o5 config ==")
print(run("cat /root/o5_config.json")[0])
cli.close()
