#!/usr/bin/env python3
"""Offline quality check of libavcodec's AAC coders (F-06, output-ports spec).

Encodes two fixed synthetic stereo 48 kHz signals (speech-like and music-like, 10 s
each) with the `aac` encoder at the given bitrates and each `aac_coder`, decodes them
back, and prints SNR, segmental SNR, log-spectral distance and the decoded bandwidth
per coder. Same encoder settings as the adapter: one thread, the `ffmpeg` tool's
libavcodec. Needs `ffmpeg` on PATH and numpy.

    python3 -I crates/rbs-codec-ffmpeg/tools/coder_quality.py [128 160]
"""

import subprocess
import sys
import tempfile
from pathlib import Path

import numpy as np

RATE = 48_000
SECONDS = 10
CODERS = ["twoloop", "fast"]
PRIMING = 1024  # the encoder's initial_padding; ADTS carries no edit list


def speech_like(rng):
    """Voiced segments (pulse train through three formants, gliding pitch) with
    syllable-rate envelope, unvoiced noise bursts and pauses."""
    n = RATE * SECONDS
    t = np.arange(n) / RATE
    f0 = 140 + 40 * np.sin(2 * np.pi * 0.7 * t) + 15 * np.sin(2 * np.pi * 3.1 * t)
    phase = np.cumsum(f0 / RATE)
    pulses = (np.diff(np.floor(phase), prepend=0) > 0).astype(float)
    voiced = np.zeros(n)
    for fc, bw, gain in [(700, 110, 1.0), (1200, 130, 0.6), (2600, 200, 0.3)]:
        voiced += gain * resonator(pulses, fc, bw)
    syllable = np.clip(np.sin(2 * np.pi * 4.0 * t), 0, None) ** 0.7
    pause = (np.sin(2 * np.pi * 0.23 * t) > -0.6).astype(float)
    noise = rng.standard_normal(n)
    hiss = resonator(noise, 5500, 2500) * (np.sin(2 * np.pi * 4.0 * t + 2.2) > 0.85)
    mono = (voiced * syllable + 0.15 * hiss) * pause
    mono = smooth_gate(mono)
    mono *= 0.3 / np.max(np.abs(mono))
    right = 0.9 * mono + 0.02 * rng.standard_normal(n) * pause
    return np.stack([mono, right])


def music_like(rng):
    """Four-voice chords with rich harmonics and decaying notes, a bass line, and a
    noise hi-hat, panned across the stereo field."""
    n = RATE * SECONDS
    t = np.arange(n) / RATE
    left = np.zeros(n)
    right = np.zeros(n)
    chords = [[261.6, 329.6, 392.0, 493.9], [220.0, 261.6, 329.6, 392.0],
              [174.6, 220.0, 261.6, 329.6], [196.0, 246.9, 293.7, 392.0]]
    beat = RATE // 2
    for start in range(0, n, beat * 4):
        chord = chords[(start // (beat * 4)) % len(chords)]
        seg = slice(start, min(n, start + beat * 4))
        ts = t[seg] - t[start]
        env = np.exp(-ts * 1.2) * (1 - np.exp(-ts * 200))
        for k, f in enumerate(chord):
            tone = sum((0.6 ** h) * np.sin(2 * np.pi * f * (h + 1) * ts + h)
                       for h in range(8))
            pan = 0.2 + 0.2 * k
            left[seg] += (1 - pan) * env * tone
            right[seg] += pan * env * tone
        bass = np.sin(2 * np.pi * chord[0] / 2 * ts) * np.exp(-ts * 0.8)
        left[seg] += 0.8 * bass
        right[seg] += 0.8 * bass
    hat = resonator(rng.standard_normal(n), 9000, 4000)
    hat_env = np.exp(-((np.arange(n) % (beat // 2)) / RATE) * 40)
    left += 0.25 * hat * hat_env
    right += 0.35 * hat * hat_env
    peak = max(np.max(np.abs(left)), np.max(np.abs(right)))
    return np.stack([left, right]) * (0.5 / peak)


def resonator(x, fc, bw):
    """Two-pole band-pass filter (direct form; a plain loop, as numpy has no IIR)."""
    r = np.exp(-np.pi * bw / RATE)
    a1 = -2 * r * np.cos(2 * np.pi * fc / RATE)
    a2 = r * r
    y = np.zeros_like(x)
    y1 = y2 = 0.0
    g = 1 - r
    for i, v in enumerate(x):
        y0 = g * v - a1 * y1 - a2 * y2
        y[i] = y0
        y2, y1 = y1, y0
    return y


def smooth_gate(x):
    k = np.hanning(481)
    k /= k.sum()
    env = np.convolve(np.abs(x), k, mode="same")
    return x * (env > 1e-4 * np.max(env))


def encode_decode(signal, coder, kbps, workdir):
    raw = workdir / "in.f32"
    out = workdir / f"{coder}-{kbps}.aac"
    signal.T.astype("<f4").tofile(raw)
    enc = subprocess.run(
        ["ffmpeg", "-hide_banner", "-nostats", "-benchmark", "-y", "-f", "f32le", "-ar", str(RATE), "-ac", "2",
         "-i", str(raw), "-c:a", "aac", "-aac_coder", coder, "-b:a", f"{kbps}k",
         "-threads", "1", "-f", "adts", str(out)],
        check=True, capture_output=True, text=True)
    # `bench: utime=1.234s ...`: CPU time of the whole run (reading raw input is cheap).
    utime = float(enc.stderr.split("utime=")[1].split("s")[0])
    decoded = subprocess.run(
        ["ffmpeg", "-v", "error", "-i", str(out), "-f", "f32le", "-"],
        check=True, capture_output=True).stdout
    pcm = np.frombuffer(decoded, dtype="<f4").reshape(-1, 2).T
    return pcm, out.stat().st_size, utime


def align(ref, dec):
    """Drops the priming, confirmed by cross-correlation around it."""
    probe = ref[0, RATE:RATE * 2]
    best = max(range(PRIMING - 64, PRIMING + 65),
               key=lambda lag: float(np.dot(probe, dec[0, RATE + lag:RATE * 2 + lag])))
    assert best == PRIMING, f"unexpected delay {best}"
    dec = dec[:, best:best + ref.shape[1]]
    return dec


def metrics(ref, dec):
    margin = RATE // 10
    r = ref[:, margin:-margin]
    d = dec[:, margin:-margin]
    err = r - d
    snr = 10 * np.log10(np.sum(r ** 2) / np.sum(err ** 2))
    frame = 1024
    segs = []
    for i in range(0, r.shape[1] - frame, frame):
        p = np.sum(r[:, i:i + frame] ** 2)
        e = np.sum(err[:, i:i + frame] ** 2)
        if p > 1e-6 * frame:
            segs.append(np.clip(10 * np.log10(p / max(e, 1e-20)), -10, 35))
    seg_snr = float(np.mean(segs))
    win = np.hanning(2048)
    lsd = []
    hf_ref = hf_dec = 0.0
    freqs = np.fft.rfftfreq(2048, 1 / RATE)
    band = (freqs >= 50) & (freqs <= 16_000)
    for ch in range(2):
        for i in range(0, r.shape[1] - 2048, 1024):
            a = np.abs(np.fft.rfft(win * r[ch, i:i + 2048])) ** 2
            b = np.abs(np.fft.rfft(win * d[ch, i:i + 2048])) ** 2
            hf_ref += np.sum(a[freqs > 14_000])
            hf_dec += np.sum(b[freqs > 14_000])
            floor = np.max(a) * 1e-6
            if np.max(a) < 1e-8:
                continue
            la = 10 * np.log10(np.maximum(a[band], floor))
            lb = 10 * np.log10(np.maximum(b[band], floor))
            lsd.append(np.sqrt(np.mean((la - lb) ** 2)))
    hf = 10 * np.log10(max(hf_dec, 1e-20) / max(hf_ref, 1e-20))
    return snr, seg_snr, float(np.mean(lsd)), hf


def main():
    rates = [int(v) for v in sys.argv[1:]] or [128, 160]
    rng = np.random.default_rng(20261120)
    signals = {"speech": speech_like(rng), "music": music_like(rng)}
    print("signal  kbps  coder    SNR dB  segSNR dB  LSD dB  >14 kHz dB    bytes"
          "  us/10ms")
    with tempfile.TemporaryDirectory() as tmp:
        workdir = Path(tmp)
        for name, sig in signals.items():
            for kbps in rates:
                for coder in CODERS:
                    pcm, size, utime = encode_decode(sig, coder, kbps, workdir)
                    per_block = utime / (SECONDS * 100) * 1e6
                    snr, seg, lsd, hf = metrics(sig, align(sig, pcm))
                    print(f"{name:7} {kbps:4}  {coder:7} {snr:7.2f} {seg:9.2f}"
                          f" {lsd:7.2f} {hf:10.1f} {size:8} {per_block:8.0f}")


if __name__ == "__main__":
    main()
