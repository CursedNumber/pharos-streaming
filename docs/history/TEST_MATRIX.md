# Pharos/Trireme Hardware Test Matrix

**Goal:** basic functionality across nearly every processor + GPU configuration, host
(encode) and client (decode). Produced 2026-06-24 by a multi-agent pass grounded in the
repo's actual codec paths + adversarially-verified hardware facts.

**Pipeline fact that drives everything:** Pharos is **HEVC (default) + AV1 (opt-in), with
NO H.264 path anywhere** — verified: the `VideoCodec` enum is `Hevc`/`Av1` only, the panel's
H.264 card is display-only, and there is zero `h264_*` encode/decode wiring. So a GPU's
value is decided purely by its **HEVC/AV1 hardware** support.

## Coverage matrix

| Config (device · role) | Encode | Decode | Fits pipeline? | Coverage value | Verdict |
|---|---|---|---|---|---|
| **4070 · devbox .100 · Linux host** | NVENC HEVC+AV1 (direct CUDA) | — | ✅ both | reference (gold standard) | **Keep untouched.** Everything else validates against it. |
| **4070 · Windows host** (dual-boot) | ffmpeg `hevc_nvenc` (AV1 needs a code-add) | — | ✅ HEVC | **NEW — never live-validated** | **#1 — highest-value gap.** Dual-boot, don't swap. |
| **Topton Intel · Linux** (bare-metal) | `hevc_qsv` host = new code | `hevc_qsv`/`av1_qsv` client (plumbed) | ✅ | client decode NEW (free); host encode = new code | **#2 client now; host encode is P-later.** |
| **T14 Intel · Windows host** | `hevc_qsv` | — | ✅ HEVC | covered | done |
| **RX 570 .213 · Windows host** | `hevc_amf` | `amf` decode untested | ✅ HEVC | encode covered; **decode is the open half** | **#3 — close AMD decode leg.** |
| **HD 7900 · anywhere** | H.264 only (VCE 1.0) | H.264 only (UVD 3.x) | ❌ **none** | **zero** | **Leave out.** Can't HW-encode or HW-decode the codecs. |
| **Pi 5 (BCM2712) · client** | — (no encoder) | **HW HEVC 4Kp60** via `drm` (V4L2-stateless); AV1 = software only | ✅ HEVC | **NEW** | **#4 — bring up as HEVC client.** |
| **Pi 4 couchpi .209 · client** | — | HW HEVC (`v4l2m2m`/`drm`) | ✅ HEVC | covered | done |
| **Android (X90J, 14T Pro, Mi Box) · client** | — | HEVC universal; AV1 on flagships | ✅ | broad, mostly covered | done; one gap = Qualcomm AV1 |
| **Ubuntu LTS stick · client** | — | stock apt ffmpeg = **no** bundled nvenc/nvdec → probe fallback | ✅ (degrade test) | packaging/driver smoke test | **#5 — one stock pass.** |

## The swap verdict (4070 → Windows rig, HD 7900 → CachyOS)

**Don't do the physical swap.** The HD 7900 (Tahiti, 2012, VCE 1.0) is **H.264-only
silicon** and the pipeline has no H.264 path, so in the CachyOS box it can neither HW-encode
nor HW-decode anything the stack streams — you'd be downgrading your working reference host
to a paperweight, *and* dismantling the live-validated `.100` gold-standard rig to obtain a
Windows-NVENC test you can get for free.

**Instead: dual-boot the CachyOS/4070 machine into Windows** (separate disk/partition, keep
the NVIDIA Windows driver off GRUB). Identical `hevc_nvenc`-on-Windows validation, lower
risk (no reseating the 4070 or the 12-year-old card), zero risk to the reference install.
The RX 570 (.213, Polaris/VCE 3.4) already covers every AMD hardware-codec path worth
testing. The HD 7900 has **no role** in this fleet (at most a throwaway software-decode-client
curiosity).

## Prioritized test plan

1. **(low) Windows NVENC host** — dual-boot the 4070 box into Windows, `pharos-host stream
   <bind> <idx> 60 nvenc` (ddagrab), pair a known-good client, confirm `hevc_nvenc` streams.
   *The single highest-value gap — never live-validated.*
2. **(low) Linux QSV client** — boot the Topton bare-metal (kernel 6.8+, intel-media-driver
   iHD, oneVPL; `vainfo`), run trireme against the 4070 host, let the probe self-confirm
   `hevc_qsv`, then `TRIREME_CODEC=av1` for `av1_qsv`. *Zero new code.*
3. **(low) AMD AMF decode** — on the RX 570 .213 box run trireme as a **client** against the
   4070 host; confirm the probe picks `amf` and HW-decodes HEVC. *Closes the open AMD leg.*
4. **(med) Pi 5 HEVC client** — recent Raspberry Pi OS (V4L2-stateless HEVC driver present),
   add user to `render`, 5GHz/Ethernet, run trireme-kms against the 4070 host; confirm `drm`
   tier decodes in hardware. *Should meet/beat the Pi 4.*
5. **(low) Ubuntu LTS degrade test** — boot the stock stick (do NOT add jellyfin-ffmpeg/CUDA
   PPA), run trireme once against the 4070 host; confirm the probe falls through to
   vaapi/software with the WARNING branch rather than mis-selecting. *Real-world packaging path.*
6. **(high) Linux QSV host encode** — add an ffmpeg `hevc_qsv` pipeline to the compositor
   (`capture.rs` is hardcoded `hevc_nvenc`); validate on the Topton. *Genuine new code — after
   the free wins.*
7. **(low, optional) Qualcomm AV1** — one Snapdragon 8 Gen 2+ phone as an AV1 client. *Vendor
   breadth, not capability-critical.*

## Gotchas / traps (verified)

- **Pi 5 myth (INVERTED):** the common "Pi 5 lost HEVC decode" claim is backwards — BCM2712
  **keeps** a HW HEVC 4Kp60 decoder and dropped **H.264** HW decode (irrelevant; we never
  emit H.264). The HEVC V4L2-stateless driver landed *after* launch, so update the
  kernel/firmware before concluding "no HW decode." Watch the decoder's **tiled output
  format** in zero-copy scanout (may force an extra copy).
- **Never break the reference rig** to run a basic-functionality test — dual-boot instead.
- **QSV-in-a-VM is a minefield** on Alder-Lake-N Toptons: GVT-g deprecated/absent, SR-IOV
  out-of-tree-experimental; only GVT-d full passthrough works and it steals the iGPU from the
  host. Go **bare-metal**.
- **Topton AV1-encode SKU trap:** N100/N150 (and std 11–13th gen) have **no AV1 encoder** —
  only Meteor Lake/Arc or N305/N355-class. Confirm the SKU before treating AV1 QSV encode as
  a target; an N100 validates HEVC QSV encode + AV1 *decode* only.
- **Windows NVENC is HEVC-only out of the box:** `pharos-win` emits only `hevc_*` — no
  `av1_nvenc` variant. AV1 on the Windows 4070 host needs a code-add. Ensure ffmpeg is on
  PATH / in the WinGet Links shim dir or the encoder spawn silently fails.
- **Android armv7 build gap:** the current build ships arm64-v8a + x86_64 only. 32-bit-only
  TVs (X90J, armeabi-v7a) **can't install at all** without `cargo ndk -t armeabi-v7a` — a
  "device can't install" failure that looks like a decode bug but isn't.
- **Linux QSV decode probesize trap:** do NOT add ffmpeg `-probesize`/`-analyzeduration`
  limits on the QSV path — in-tree lore records they starve the QSV SPS parser ("Picture size
  0x0"). Use the standalone `hevc_qsv` decoder.
- **Ubuntu test validity:** the packaging smoke test only holds if ffmpeg stays **stock apt**
  and the OS isn't fully updated — pulling a CUDA PPA or backports erases the variance the
  test measures.
- **HD 7900 driver split is a time-sink with no payoff:** Tahiti binds legacy `radeon` by
  default (amdgpu only with `radeon.si_support=0 amdgpu.si_support=1`, flips to amdgpu-default
  at kernel 6.19+); either way the codec floor is unchanged (no HEVC/AV1). Don't sink effort.
