#!/usr/bin/env python3
"""拍学机音频卡顿自动归因：每轮 1 分钟（拍照→讲解→语音追问→联网查），拉 trace 自动分类。

用法：
  python3 tests/device/stutter.py                    # 正常跑一轮
  python3 tests/device/stutter.py --fault jitter     # 注入故障验证归因：jitter|upslow|cpu|echo
  python3 tests/device/stutter.py --analyze 某个.log  # 只分析已有日志

卡顿定义：一个回合未结束时，喇叭的播放队列放空超过 150ms，下一块才到。
归因看卡顿前 2—3 秒哪一段先异常（规则见 classify）。
"""
import argparse
import datetime as dt
import json
import os
import re
import statistics
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from run_e2e import APP_DIR, AUDIO_DIR, CTL, IMG_DIR, Board, Run, prepare  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
LOG = "/tmp/paxueji_stutter.log"
STUTTER_MS = 150
ROUND_S = 60

TRACE_RE = re.compile(r"\[trace\] (\d+) (\w+)(.*)$")
KV_RE = re.compile(r"(\w+)=(\S+)")

FAULTS = {
    "jitter": {"env": "PAXUEJI_FAULT=net_jitter:400", "expect": "网络抖动"},
    "upslow": {"env": "PAXUEJI_FAULT=ul_slow:900", "expect": "上传拖慢"},
    "cpu": {"env": "", "expect": "板子播放"},
    "echo": {"env": "", "expect": "回声当插话"},
}


# --------------------------------------------------------------------------
# 解析
# --------------------------------------------------------------------------

def parse(lines):
    ev = []
    turns = []
    session = None
    for l in lines:
        m = TRACE_RE.search(l)
        if m:
            kv = dict(KV_RE.findall(m.group(3)))
            ev.append((int(m.group(1)), m.group(2), kv))
            continue
        if "session.created: id=" in l:
            session = l.split("id=", 1)[1].strip()
        if "[口播回合]" in l:
            turns.append((l[:8], l.split("[口播回合]", 1)[1].strip()))
    ev.sort(key=lambda e: e[0])
    return ev, turns, session


def num(kv, k, default=0.0):
    try:
        return float(kv.get(k, default))
    except ValueError:
        return default


def find_stutters(ev):
    """相邻两块播放之间，前一块排期的结束时刻到后一块开始写之间空了 >150ms，且中间没有回合边界。"""
    plays = [(t, kv) for t, name, kv in ev if name == "play"]
    boundaries = [t for t, name, kv in ev
                  if name in ("flush", "shutter", "sink_open")
                  or (name == "spoken_end" and kv.get("reason") == "turn_eos")]
    out = []
    for (t0, a), (t1, b) in zip(plays, plays[1:]):
        end0 = t0 + num(a, "ahead_ms")
        start1 = t1 - num(b, "write_ms")
        gap = start1 - end0
        if gap <= STUTTER_MS or gap > 8000:
            continue
        if any(end0 - 300 <= bt <= start1 for bt in boundaries):
            continue
        out.append({"start": end0, "end": start1, "gap_ms": round(gap)})
    return out


def classify(st, ev, base_rtt):
    g0, g1 = st["start"], st["end"]
    win = [(t, n, kv) for t, n, kv in ev if g0 - 3000 <= t <= g1]
    pre = [(t, n, kv) for t, n, kv in win if t <= g0 + 300]

    yielded = [x for x in pre if (x[1] == "spoken_end" and x[2].get("reason") == "listen"
                                   and x[2].get("speaking") == "true")
               or (x[1] == "interrupt" and x[2].get("src") == "server_yield")]
    if yielded:
        mic = [num(kv, "rms") for t, n, kv in win if n == "mic_unit" and t <= g0]
        if mic and max(mic) > 800:
            return "回声当插话", f"句中让位，前 2 秒麦克风 RMS 最高 {max(mic):.0f}"
        return "模型句中停顿", "句中让位，麦克风安静"

    dls = [t for t, n, kv in ev if n == "dl"]
    last_dl = max([t for t in dls if t <= g0], default=None)
    next_dl = min([t for t in dls if t > g0], default=None)
    starved = last_dl is None or next_dl is None or (next_dl - last_dl) > 400
    if starved:
        send = [num(kv, "send_ms") for t, n, kv in pre if n == "ul_sent"]
        rtts = [num(kv, "rtt_ms") for t, n, kv in pre if n == "ack"]
        if send and max(send) > 300:
            return "上传拖慢", f"上行单元发送最长 {max(send):.0f}ms"
        if rtts and max(rtts) > 2 * base_rtt + 300:
            return "网络抖动", f"ACK 往返最高 {max(rtts):.0f}ms（基线 {base_rtt:.0f}ms），下行断了 {((next_dl or g1) - (last_dl or g0)):.0f}ms"
        return "服务端慢", f"上下行与网络都正常，但下行 {((next_dl or g1) - (last_dl or g0)):.0f}ms 没来新音频"
    writes = [num(kv, "write_ms") for t, n, kv in ev if n == "play" and g0 - 3000 <= t <= g1 + 1500]
    return "板子播放", f"下行音频在，但喇叭断了；写入最长 {max(writes, default=0):.0f}ms"


def analyze(lines):
    ev, turns, session = parse(lines)
    rtts = [num(kv, "rtt_ms") for t, n, kv in ev if n == "ack"]
    base_rtt = statistics.median(rtts) if rtts else 500.0
    stutters = find_stutters(ev)
    for s in stutters:
        s["cause"], s["evidence"] = classify(s, ev, base_rtt)
        s["at"] = dt.datetime.fromtimestamp(s["start"] / 1000).strftime("%H:%M:%S.%f")[:-3]
    repeats = [(a, b) for (a, x), (b, y) in zip(turns, turns[1:]) if x and x == y]
    counts = {}
    for s in stutters:
        counts[s["cause"]] = counts.get(s["cause"], 0) + 1
    if repeats:
        counts["回声自循环（同一句重复）"] = len(repeats)
    plays = sum(1 for t, n, kv in ev if n == "play")
    return {"session": session, "stutters": stutters, "counts": counts, "repeats": repeats,
            "base_ack_rtt_ms": round(base_rtt), "plays": plays, "turns": len(turns), "events": len(ev)}


def timeline(lines, st, before=2500, after=800):
    ev, _, _ = parse(lines)
    keep = ("dl", "play", "spoken_end", "ack", "ul_sent", "gate", "interrupt", "flush", "sink_open", "force_listen")
    rows = []
    for t, n, kv in ev:
        if st["start"] - before <= t <= st["end"] + after and n in keep:
            rel = t - st["start"]
            rows.append(f"{rel:+6.0f}ms {n} " + " ".join(f"{k}={v}" for k, v in kv.items()))
    return rows


# --------------------------------------------------------------------------
# 一轮 1 分钟
# --------------------------------------------------------------------------

def run_round(board, fault):
    f = FAULTS.get(fault, {})
    env = f"PAXUEJI_TRACE=1 PAXUEJI_TEST_CTL={CTL} {f.get('env', '')}"
    board.sh("pkill -f 'paxueji/o5-harness-r[s]'; systemctl stop launcher.service; "
             "pkill -9 -f 'launcher_[d]aemon'; sleep 2; "
             f"rm -f {CTL}; mkfifo {CTL}; : > {LOG}", timeout=60)
    if fault == "echo":
        board.sh("cp /root/paxueji_config.json /root/paxueji_config.json.bak && python3 -c \""
                 "import json;p='/root/paxueji_config.json';c=json.load(open(p));"
                 "c['audio'].update({'micGateWhilePlaying':False,'outputVolume':90});"
                 "json.dump(c,open(p,'w'),ensure_ascii=False,indent=2)\"")
    board.sh(f"cd {APP_DIR} && ({env} setsid nohup python3 main.py > {LOG} 2>&1 < /dev/null &)", timeout=30)
    if fault == "cpu":
        board.sh("for i in 1 2; do (setsid nohup sh -c 'while :; do :; done' >/dev/null 2>&1 &); done")
    run = Run(board)
    board.log_lines = lambda: board.sh(f"cat {LOG} 2>/dev/null", timeout=30).splitlines()
    ok = run.wait(r"state -> Ready", 40)
    try:
        if not ok:
            raise RuntimeError("会话没就绪")
        time.sleep(2)
        t_end = time.time() + ROUND_S
        script = [
            (f"shutter_rgb {IMG_DIR}/企鹅.rgb", 14),
            (f"say_pcm {AUDIO_DIR}/q_penguin_food.pcm", 14),
            ("say 企鹅最深能潜到多深？帮我查一下", 18),
            ("shutter", 1),
            (f"shutter_rgb {IMG_DIR}/向日葵.rgb", 14),
        ]
        i = 0
        while time.time() < t_end:
            cmd, wait_s = script[i % len(script)]
            board.send(cmd)
            time.sleep(min(wait_s, max(0, t_end - time.time())))
            i += 1
    finally:
        if fault == "cpu":
            board.sh("pkill -f \"while :; do :; d[o]ne\"")
        lines = board.log_lines()
        board.sh(f"pkill -f 'paxueji/o5-harness-r[s]'; sleep 2; rm -f {CTL}")
        if fault == "echo":
            board.sh("mv /root/paxueji_config.json.bak /root/paxueji_config.json")
        board.sh("systemctl start launcher.service")
    return lines


def report(res, lines, fault, out_dir):
    os.makedirs(out_dir, exist_ok=True)
    n = len(res["stutters"])
    summary = "，".join(f"{k} {v} 次" for k, v in sorted(res["counts"].items(), key=lambda x: -x[1])) or "无"
    head = (f"本轮 1 分钟{('（注入故障：' + fault + '）') if fault else ''}，播放块 {res['plays']} 个，"
            f"口播回合 {res['turns']} 个，卡顿 {n} 次：{summary}。ACK 基线 {res['base_ack_rtt_ms']}ms，"
            f"会话 {res['session']}")
    md = [f"# 拍学机音频卡顿归因 {os.path.basename(out_dir)}", "", head, ""]
    if fault:
        exp = FAULTS[fault]["expect"]
        hit = res["counts"].get(exp, 0)
        md += [f"验证：期望主要归为「{exp}」，实际 {hit}/{n} 次。", ""]
    for i, s in enumerate(res["stutters"], 1):
        md += [f"## 卡顿 {i}：{s['at']}，空了 {s['gap_ms']}ms → {s['cause']}", "", s["evidence"], "", "```"]
        md += timeline(lines, s)
        md += ["```", ""]
    for a, b in res["repeats"]:
        md.append(f"- 同一句重复：{a} 与 {b}")
    with open(os.path.join(out_dir, "归因报告.md"), "w") as f:
        f.write("\n".join(md))
    with open(os.path.join(out_dir, "result.json"), "w") as f:
        json.dump(res, f, ensure_ascii=False, indent=2)
    with open(os.path.join(out_dir, "device.log"), "w") as f:
        f.write("\n".join(lines))
    print(head)
    if fault:
        print(md[4])
    print(f"报告：{out_dir}/归因报告.md")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default=os.environ.get("PAXUEJI_HOST", "10.15.125.1"))
    ap.add_argument("--fault", choices=list(FAULTS), default=None)
    ap.add_argument("--analyze", default=None)
    a = ap.parse_args()
    stamp = dt.datetime.now().strftime("%Y%m%d-%H%M%S")
    out = os.path.join(HERE, "reports", f"stutter-{stamp}{'-' + a.fault if a.fault else ''}")
    if a.analyze:
        lines = open(a.analyze).read().splitlines()
        report(analyze(lines), lines, a.fault, out)
        return
    board = Board(a.host)
    prepare(board)
    lines = run_round(board, a.fault)
    report(analyze(lines), lines, a.fault, out)


if __name__ == "__main__":
    main()
