import paramiko, os
cli = paramiko.SSHClient()
cli.set_missing_host_key_policy(paramiko.AutoAddPolicy())
cli.connect("10.239.93.1", 22, "root", "sipeed", timeout=8)
sftp = cli.open_sftp()  # try sftp; fall back to base64 if restricted
local = os.path.expandvars(r"C:\Users\1\AppData\Local\Temp\xtest\target\aarch64-unknown-linux-gnu\release\xtest")
try:
    sftp.put(local, "/tmp/xtest")
    print("sftp ok")
except Exception as e:
    print("sftp failed:", e)
    import base64
    _, out, _ = cli.exec_command("cat > /tmp/xtest.b64")
    with open(local, "rb") as f:
        data = base64.b64encode(f.read()).decode()
    for i in range(0, len(data), 60000):
        _, out2, _ = cli.exec_command("echo -n '%s' >> /tmp/xtest.b64" % data[i:i+60000])
        out2.channel.recv_exit_status()
    _, o, e = cli.exec_command("base64 -d /tmp/xtest.b64 > /tmp/xtest && chmod +x /tmp/xtest && /tmp/xtest; echo rc=$?")
    print(o.read().decode(), e.read().decode())
else:
    _, o, e = cli.exec_command("chmod +x /tmp/xtest && /tmp/xtest; echo rc=$?")
    print(o.read().decode(), e.read().decode())
cli.close()
