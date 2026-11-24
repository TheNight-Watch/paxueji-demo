"""Generate golden test vectors for the Rust AEC port.

Runs the reference Python implementations (o5/aec.py NLMS + o5/subband_aec.py)
on synthetic scenarios and stores near/far inputs and filtered outputs so the
Rust port can be compared numerically (tolerance-based, not bit-exact).

Cadence: content-aligned blocks (same as the official tools/test_aec_pc.py
run_stream): push_far(far_block) then cancel(near_block), 25 ms blocks.

Signal structures that make each AEC lock/converge (verified):
  - NLMS: continuous speech-like far (short pauses only).
  - Subband: sentence-structured far (1.5 s on / 0.7 s off) — the lock needs
    the far active segment to end well before the near head so the echo of a
    full sentence is inside the near ring (this mirrors real TTS turns).

Usage (from PC):
    python tests/gen_aec_fixtures.py <python_project_root> <out_dir>
"""
import sys, os, json
import numpy as np

PY_ROOT = sys.argv[1] if len(sys.argv) > 1 else r"D:\code\MaixCAM2_Cpm_O5_Harness"
OUT = sys.argv[2] if len(sys.argv) > 2 else os.path.join(os.path.dirname(__file__), "fixtures", "aec")
os.makedirs(OUT, exist_ok=True)
sys.path.insert(0, PY_ROOT)

from o5.aec import EchoCanceler
from o5.subband_aec import SubbandSuppressor

SR = 16000
BLOCK_MS = 25
ECHO_DELAY_S = 0.530
ECHO_GAIN = 0.55

def speechish(n, seed, vol=0.2):
    """Continuous speech-like band (fixed f0/AM family; NLMS-friendly)."""
    r = np.random.default_rng(seed)
    t = np.arange(n) / SR
    f0 = 120 + 40 * np.sin(2 * np.pi * 0.7 * t)
    x = np.zeros(n)
    for k in range(1, 9):
        x += np.sin(2 * np.pi * f0 * k * t + r.uniform(0, 6.28)) / k
    am = 0.5 * (1 + np.sin(2 * np.pi * 3.1 * t + r.uniform(0, 6.28)))
    x = x * am + 0.05 * r.standard_normal(n)
    for s in range(0, n, SR):
        if r.uniform() < 0.3:
            x[s:s + int(0.2 * SR)] *= 0.01
    return x / (np.max(np.abs(x)) + 1e-9) * vol

def speechish_rand(n, seed, vol=0.2):
    """Randomized-envelope variant: per-seed f0/AM rates decorrelate the
    far-sentence and near-speech envelopes (prevents spurious envelope-xcorr
    locks in the subband suppressor). No periodic pauses — a 1 s-periodic
    pause grid creates envelope aliases that beat the true-peak vote."""
    r = np.random.default_rng(seed)
    t = np.arange(n) / SR
    f0 = r.uniform(100, 180) * (1 + 0.25 * np.sin(2 * np.pi * r.uniform(0.3, 1.1) * t))
    am_rate = r.uniform(1.5, 5.0)
    x = np.zeros(n)
    for k in range(1, 9):
        x += np.sin(2 * np.pi * f0 * k * t + r.uniform(0, 6.28)) / k
    x = x * (0.5 * (1 + np.sin(2 * np.pi * am_rate * t + r.uniform(0, 6.28)))) \
        + 0.05 * r.standard_normal(n)
    return x / (np.max(np.abs(x)) + 1e-9) * vol

def sentence_far(n, seed=100, vol=0.5):
    """Sentence-structured far: 1.5 s on / 0.7 s off (subband-friendly).
    seed=100 verified to lock at ~526 ms with suppression engaged."""
    far = np.zeros(n)
    r = np.random.default_rng(seed)
    for t0 in range(0, n - int(2.2 * SR), int(2.2 * SR)):
        L = int(1.5 * SR)
        far[t0:t0 + L] = speechish_rand(L, seed=int(r.integers(1, 10 ** 6)), vol=vol)
    return far

def to_s16(x):
    return np.clip(np.rint(x * 32767.0), -32768, 32767).astype(np.int16)

def make_echo(far):
    d = int(ECHO_DELAY_S * SR)
    echo = np.zeros(len(far))
    echo[d:] = far[:-d] * ECHO_GAIN
    return np.tanh(echo * 1.5) / 1.5  # speaker/PA soft clip

def run_scenario(name, make_aec, near, far):
    aec = make_aec()
    bs = int(SR * BLOCK_MS / 1000)
    near16, far16 = to_s16(near), to_s16(far)
    n_blocks = min(len(near16), len(far16)) // bs
    out = []
    for i in range(n_blocks):
        aec.push_far(far16[i * bs:(i + 1) * bs].tobytes())
        out.append(np.frombuffer(aec.cancel(near16[i * bs:(i + 1) * bs].tobytes()), dtype=np.int16))
    out = np.concatenate(out) if out else np.array([], dtype=np.int16)
    base = os.path.join(OUT, name)
    near16[:n_blocks * bs].astype("<i2").tofile(base + ".near.s16")
    far16[:n_blocks * bs].astype("<i2").tofile(base + ".far.s16")
    out.astype("<i2").tofile(base + ".out.s16")
    m = min(len(near16[:n_blocks * bs]), len(out))
    tail = slice(m // 2, m)
    n_rms = float(np.sqrt(np.mean(near16[:m][tail].astype(np.float64) ** 2)))
    o_rms = float(np.sqrt(np.mean(out[tail].astype(np.float64) ** 2)))
    meta = {
        "sample_rate": SR, "block_ms": BLOCK_MS, "blocks": n_blocks,
        "echo_delay_s": ECHO_DELAY_S, "class": type(aec).__name__,
        "stats": aec.stats(),
        "tail_erle_db": float(10 * np.log10((n_rms ** 2 + 1e-12) / (o_rms ** 2 + 1e-12))),
    }
    with open(base + ".meta.json", "w", encoding="utf-8") as f:
        json.dump(meta, f, indent=2, default=str, ensure_ascii=False)
    print(f"{name}: blocks={n_blocks} tailERLE={meta['tail_erle_db']:+.1f}dB stats={meta['stats']}")

NLMS = lambda: EchoCanceler(sample_rate=SR, max_delay_ms=1000.0)
SUB = lambda: SubbandSuppressor(sample_rate=SR, max_delay_ms=1000.0)

DUR = 24.0
N = int(SR * DUR)

# ---- NLMS scenarios: continuous far; doubletalk in bursts (windows) ----
far_c = speechish(N, seed=7, vol=0.5)
echo_c = make_echo(far_c)
sp_c = speechish(N, seed=11, vol=0.3)
sp_burst = np.zeros(N)
sp_burst[3 * SR:8 * SR] = speechish(5 * SR, seed=11, vol=0.3)
sp_burst[14 * SR:18 * SR] = speechish(4 * SR, seed=12, vol=0.3)
run_scenario("nlms_echo_only", NLMS, echo_c, far_c)
run_scenario("nlms_doubletalk", NLMS, echo_c + sp_burst, far_c)
run_scenario("nlms_quiet", NLMS, echo_c * 0.05, far_c)

# ---- NLMS drift (official test_aec_pc.py scenario-3 structure):
# far clock off by +0.1%/s; near speech only in [12,14]s; expect re-lock + track.
rate = 0.001
far_src = speechish(N + 8192, seed=55, vol=0.5)
far_d = far_src[:N]
d0 = int(ECHO_DELAY_S * SR)
xs = np.arange(-2048, len(far_src), dtype=np.float64)
vals = np.concatenate((np.zeros(2048), far_src))
kidx = np.arange(N, dtype=np.float64)
rng = np.random.default_rng(66)
taps = [(0, 1.0), (192, 0.45), (432, 0.30), (608, 0.18)]
for _ in range(24):
    taps.append((int(rng.uniform(80, 700)), float(rng.normal(0.0, 0.05))))
echo_d = np.zeros(N)
for pos, g in taps:
    echo_d += g * np.interp((kidx - d0 - pos) / (1.0 + rate), xs, vals)
sp_d = np.zeros(N)
sp_d[12 * SR:14 * SR] = speechish(2 * SR, seed=77, vol=0.3)
sp_d *= np.sqrt(np.mean(echo_d[8 * SR:11 * SR] ** 2)) / (np.sqrt(np.mean(sp_d[12 * SR:14 * SR] ** 2)) + 1e-12)
run_scenario("nlms_drift", NLMS, echo_d + sp_d, far_d)

# ---- Subband scenarios: sentence far, randomized envelopes ----
far_s = sentence_far(N)
echo_s = make_echo(far_s)
sp_s = speechish_rand(N, seed=11, vol=0.25)
run_scenario("sub_echo_only", SUB, echo_s, far_s)
run_scenario("sub_doubletalk", SUB, echo_s + sp_s, far_s)
run_scenario("sub_quiet", SUB, echo_s * 0.05, far_s)

print("fixtures written to", OUT)
