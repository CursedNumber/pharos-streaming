#!/usr/bin/env python3
"""A/B the trireme GPU HDR tonemap shader math against the reference CPU chain.

Verification harness for issue #15 (GPU tonemap in the wgpu present path). Run
it whenever HDR_SHADER's color math changes; it needs only ffmpeg (with libx265
+ zimg) and numpy, no GPU. Passing bar: worst block-center diff <= 3/255.
Verified 2026-07-08 on ffmpeg n8.1.2 / RTX 4070 box: worst 2.33/255.

Builds a synthetic 10-bit BT.2020/PQ test pattern (flat blocks), encodes it
to HEVC with the same 1000-nit MDCV / 1000 MaxCLL SEI the Pharos hosts attach,
runs the real client CPU chain
  zscale=t=linear:npl=203,tonemap=hable,zscale=p=bt709:t=bt709:m=bt709:r=tv,format=nv12
on it, and compares the display-reaching R'G'B' of that path (via the SDR
shader's BT.709 math) against a NumPy port of the HDR_SHADER math fed the
decoded 10-bit codes. Flat block centers only, so chroma-resampler differences
don't pollute the comparison.

Two non-obvious facts this harness pinned down (mirrored in HDR_SHADER,
crates/trireme/src/play.rs):
  - vf_tonemap's desat "luma" runs with UNITY coefficients (r+g+b) because
    zscale hands it an RGB-colorspace frame; a quirk, but the shipped look.
  - zimg's t=bt709 encode is display-referred inverse BT.1886 (pure gamma
    1/2.4), not the scene OETF.
"""
import subprocess, sys, os
import numpy as np

D = os.path.dirname(os.path.abspath(__file__))
W, H = 640, 360
BW, BH = 80, 72  # 8x5 grid of flat blocks
PEAK = 10.0      # hable normalization: MaxCLL 1000 / REFERENCE_WHITE 100

# --- build a 10-bit YCbCr (limited range) test pattern ----------------------
# Blocks defined directly as (Y, Cb, Cr) 10-bit codes: grayscale PQ ramp
# (black -> 203 nits -> 1000 nits -> beyond) + saturated/skin-ish colors.
blocks = []
# PQ grayscale ramp: choose Y codes across the range, neutral chroma.
for yc in [64, 150, 250, 350, 450, 520, 593, 650]:   # 593 ~= 203 nits, 769 ~= 1000
    blocks.append((yc, 512, 512))
for yc in [700, 769, 850, 940]:
    blocks.append((yc, 512, 512))
# colors: (Y, Cb, Cr) offsets around mid luma
for y0 in (300, 500, 700):
    for cb, cr in [(700, 400), (400, 700), (650, 650), (350, 350),
                   (512, 700), (700, 512), (512, 350), (350, 512)]:
        blocks.append((y0, cb, cr))
blocks = blocks[:40]
while len(blocks) < 40:
    blocks.append((512, 512, 512))

Yp = np.zeros((H, W), np.uint16)
Cbp = np.zeros((H // 2, W // 2), np.uint16)
Crp = np.zeros((H // 2, W // 2), np.uint16)
for i, (y, cb, cr) in enumerate(blocks):
    bx, by = (i % 8) * BW, (i // 8) * BH
    Yp[by:by + BH, bx:bx + BW] = y
    Cbp[by // 2:(by + BH) // 2, bx // 2:(bx + BW) // 2] = cb
    Crp[by // 2:(by + BH) // 2, bx // 2:(bx + BW) // 2] = cr

with open(f"{D}/in.yuv", "wb") as f:  # yuv420p10le, planar
    f.write(Yp.astype('<u2').tobytes())
    f.write(Cbp.astype('<u2').tobytes())
    f.write(Crp.astype('<u2').tobytes())

def run(cmd):
    r = subprocess.run(cmd, capture_output=True, text=True)
    if r.returncode != 0:
        print(r.stderr[-3000:]); sys.exit(f"FAILED: {' '.join(cmd)}")

# --- encode to HEVC with the host's HDR10 signalling ------------------------
x265 = ("lossless=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:"
        "range=limited:master-display=G(13250,34500)B(7500,3000)R(34000,16000)"
        "WP(15635,16450)L(10000000,1):max-cll=1000,400")
run(["ffmpeg", "-y", "-hide_banner", "-loglevel", "error",
     "-f", "rawvideo", "-pix_fmt", "yuv420p10le", "-s", f"{W}x{H}", "-r", "30",
     "-i", f"{D}/in.yuv",
     "-vf", "loop=loop=5:size=1",  # a few identical frames
     "-c:v", "libx265", "-x265-params", x265,
     "-color_primaries", "bt2020", "-color_trc", "smpte2084",
     "-colorspace", "bt2020nc", "-color_range", "tv",
     f"{D}/in.hevc"])

# --- reference: the real CPU tonemap chain -----------------------------------
VF = "zscale=t=linear:npl=203,tonemap=hable,zscale=p=bt709:t=bt709:m=bt709:r=tv,format=nv12"
run(["ffmpeg", "-y", "-hide_banner", "-loglevel", "error",
     "-i", f"{D}/in.hevc", "-vf", VF, "-frames:v", "1",
     "-f", "rawvideo", "-pix_fmt", "nv12", f"{D}/ref.nv12"])

# what zimg actually saw (the encode is not bit-exact): decoded 10-bit codes
run(["ffmpeg", "-y", "-hide_banner", "-loglevel", "error",
     "-i", f"{D}/in.hevc", "-frames:v", "1",
     "-f", "rawvideo", "-pix_fmt", "yuv420p10le", f"{D}/dec.yuv"])
dec = np.fromfile(f"{D}/dec.yuv", '<u2')
decY = dec[:W * H].reshape(H, W)
decCb = dec[W * H:W * H * 5 // 4].reshape(H // 2, W // 2)
decCr = dec[W * H * 5 // 4:].reshape(H // 2, W // 2)

raw = np.fromfile(f"{D}/ref.nv12", np.uint8)
refY = raw[:W * H].reshape(H, W).astype(np.float64) / 255.0
refUV = raw[W * H:W * H * 3 // 2].reshape(H // 2, W // 2, 2).astype(np.float64) / 255.0

def sdr_shader_rgb(y, cb, cr):
    """The SDR blit shader's NV12 BT.709 limited -> R'G'B' (what hits the screen)."""
    yv = (y - 0.0627451) * 1.1643836
    cbv, crv = cb - 0.5019608, cr - 0.5019608
    rgb = np.stack([yv + 1.7927411 * crv,
                    yv - 0.2132486 * cbv - 0.5329093 * crv,
                    yv + 2.1124018 * cbv], -1)
    return np.clip(rgb, 0.0, 1.0)

# --- candidate: NumPy port of HDR_SHADER -------------------------------------
M1, M2, C1, C2, C3 = 0.1593017578125, 78.84375, 0.8359375, 18.8515625, 18.6875
BT2020_TO_BT709 = np.array([[1.660491002, -0.587641138, -0.072849864],
                            [-0.124550474, 1.132899897, -0.008349423],
                            [-0.018150764, -0.100578898, 1.118729662]])

def hdr_shader_rgb(y10, cb10, cr10):
    y = (y10 - 64.0) / 876.0
    cb = (cb10 - 512.0) / 896.0
    cr = (cr10 - 512.0) / 896.0
    rgb = np.stack([y + 1.4746 * cr,
                    y - 0.16455313 * cb - 0.57135313 * cr,
                    y + 1.8814 * cb], -1)
    rgb = np.clip(rgb, 0.0, 1.0)
    em = np.power(rgb, 1.0 / M2)
    lin = 10000.0 * np.power(np.maximum(em - C1, 0) / np.maximum(C2 - C3 * em, 1e-6),
                             1.0 / M1) / 203.0
    luma = lin.sum(-1)  # ffmpeg quirk: tonemap sees csp=gbr -> unity luma coeffs
    overbright = np.maximum(luma - 2.0, 1e-6) / np.maximum(luma, 1e-6)
    lin = lin * (1 - overbright[..., None]) + luma[..., None] * overbright[..., None]
    sig = np.maximum(lin.max(-1), 1e-6)
    hable = lambda x: (x * (x * 0.15 + 0.05) + 0.004) / (x * (x * 0.15 + 0.5) + 0.06) - 1.0 / 15.0
    lin = lin * (hable(sig) / hable(PEAK) / sig)[..., None]
    lin709 = np.clip(lin @ BT2020_TO_BT709.T, 0.0, 1.0)
    # zimg encodes display-referred: inverse BT.1886 EOTF (pure gamma 2.4)
    return np.power(lin709, 1.0 / 2.4)

# --- compare block centers ----------------------------------------------------
print(f"{'blk':>3} {'Y/Cb/Cr in':>14} | {'ref RGB':>20} | {'gpu RGB':>20} | maxdiff(8bit)")
worst = 0.0
for i, (y, cb, cr) in enumerate(blocks):
    cx, cy = (i % 8) * BW + BW // 2, (i // 8) * BH + BH // 2
    ref = sdr_shader_rgb(refY[cy, cx], refUV[cy // 2, cx // 2, 0], refUV[cy // 2, cx // 2, 1])
    got = hdr_shader_rgb(float(decY[cy, cx]), float(decCb[cy // 2, cx // 2]),
                         float(decCr[cy // 2, cx // 2]))
    d = np.abs(ref - got).max() * 255.0
    worst = max(worst, d)
    flag = "  <-- " if d > 6 else ""
    print(f"{i:>3} {y:>4}/{cb:>4}/{cr:>4} | "
          f"{ref[0]:.3f} {ref[1]:.3f} {ref[2]:.3f}    | "
          f"{got[0]:.3f} {got[1]:.3f} {got[2]:.3f}    | {d:6.2f}{flag}")
print(f"\nworst block-center diff: {worst:.2f}/255")
sys.exit(0 if worst <= 3 else 1)
