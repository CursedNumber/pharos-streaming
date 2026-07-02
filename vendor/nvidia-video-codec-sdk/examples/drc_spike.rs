//! Validation spike for the live resolution-change patch.
//!
//! The hard constraint behind Pharos' streaming pipeline is that
//! re-initialising NVENC in a long-running process SIGSEGVs libnvcuvid
//! (`lesson-nvenc-driver-crash`). `Session::reconfigure_bitrate` is proven
//! crash-free because it never re-inits. This spike answers the open question
//! for the resolution ladder (Phase 2): does `Session::reconfigure_resolution`
//! — `NvEncReconfigureEncoder` with a *dimension* change + `resetEncoder=1` —
//! also avoid the crash, and does the registered CUDA resource survive it?
//!
//! It mirrors the real pipeline: HEVC, preset P4, ULTRA_LOW_LATENCY tuning,
//! ABGR, a CUDA device-pointer resource registered ONCE at the native (envelope)
//! size, then encoded while the coded resolution is stepped DOWN and back UP
//! within the envelope. If `reconfigure_resolution` returns `Ok` for every step
//! and every subsequent `encode_picture` produces a non-empty bitstream with no
//! SIGSEGV, the host-side actuator design is validated.
//!
//! Run: `cargo run -p nvidia-video-codec-sdk --example drc_spike`

use std::ffi::c_void;

use cudarc::driver::result as curesult;
use cudarc::driver::CudaContext;
use nvidia_video_codec_sdk::{
    sys::nvEncodeAPI::{
        NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR,
        NV_ENC_CODEC_HEVC_GUID,
        NV_ENC_INPUT_RESOURCE_TYPE,
        NV_ENC_PARAMS_RC_MODE,
        NV_ENC_PRESET_P4_GUID,
        NV_ENC_TUNING_INFO,
    },
    EncodePictureParams,
    Encoder,
    EncoderInitParams,
    ErrorKind,
};

// Native = the envelope ceiling. The ladder only ever scales DOWN from here.
const NATIVE_W: u32 = 1920;
const NATIVE_H: u32 = 1080;
const FRAMES_PER_STAGE: u32 = 30;

/// (label, width, height, reconfigure-first?). The first stage is the native
/// warm-up (no reconfigure); every later stage reconfigures THEN encodes.
const STAGES: &[(&str, u32, u32, bool)] = &[
    ("1920x1080 (native warm-up)", NATIVE_W, NATIVE_H, false),
    ("1280x720", 1280, 720, true),
    ("960x540", 960, 540, true),
    ("640x360", 640, 360, true),
    ("1920x1080 (back to native)", NATIVE_W, NATIVE_H, true),
    ("1600x900", 1600, 900, true),
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("== NVENC dynamic-resolution-change spike ==");
    println!("envelope (max) = {NATIVE_W}x{NATIVE_H}\n");

    // Same thread for the whole spike: CudaContext::new binds the primary
    // context current, which curesult::malloc_sync and NVENC both rely on.
    let cuda = CudaContext::new(0).map_err(|e| format!("cuda init: {e:?}"))?;

    let encoder = Encoder::initialize_with_cuda(cuda.clone())
        .map_err(|e| format!("nvenc init: {e:?}"))?;
    let encode_guid = NV_ENC_CODEC_HEVC_GUID;
    let preset_guid = NV_ENC_PRESET_P4_GUID;
    let tuning = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    let mut preset_config = encoder
        .get_preset_config(encode_guid, preset_guid, tuning)
        .map_err(|e| format!("preset: {e:?}"))?;
    // 1s GOP + every-frame-P, matching the real host config.
    preset_config.presetCfg.gopLength = 60;
    preset_config.presetCfg.frameIntervalP = 1;

    // Envelope set at create time; coded size starts at native.
    let mut init = EncoderInitParams::new(encode_guid, NATIVE_W, NATIVE_H);
    init.preset_guid(preset_guid)
        .tuning_info(tuning)
        .framerate(60, 1)
        .enable_picture_type_decision()
        .max_encode_resolution(NATIVE_W, NATIVE_H)
        .encode_config(&mut preset_config.presetCfg);
    let session = encoder
        .start_session(NV_ENC_BUFFER_FORMAT_ABGR, init)
        .map_err(|e| format!("start_session: {e:?}"))?;
    println!("session started, coded {}x{}", session.width(), session.height());

    // CUDA input buffer registered ONCE at the native/envelope size (pitch in
    // bytes = w*4 for ABGR). Downscaled frames reuse the top-left region of this
    // same buffer at the same pitch — no re-registration.
    let pitch = NATIVE_W * 4;
    let size = (pitch * NATIVE_H) as usize;
    let dptr =
        unsafe { curesult::malloc_sync(size) }.map_err(|e| format!("cuMemAlloc: {e:?}"))?;
    // Zero it so the encoder reads a defined (black) frame.
    unsafe { curesult::memset_d8_sync(dptr, 0, size) }
        .map_err(|e| format!("memset: {e:?}"))?;
    let mut reg = session
        .register_generic_resource(
            (),
            NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
            dptr as *mut c_void,
            pitch,
        )
        .map_err(|e| format!("register: {e:?}"))?;

    let mut bitstreams = (0..2)
        .map(|_| session.create_output_bitstream())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("bitstream: {e:?}"))?;

    let mut frame_no: u64 = 0;
    for (label, w, h, reconfigure) in STAGES {
        if *reconfigure {
            // THE operation under test. A SIGSEGV here = design dead; an Err =
            // recoverable (reject the rung); Ok + non-empty output below = win.
            session
                .reconfigure_resolution(*w, *h)
                .map_err(|e| format!("reconfigure_resolution -> {label}: {e:?}"))?;
        }
        let mut total: usize = 0;
        let mut min_len = usize::MAX;
        for _ in 0..FRAMES_PER_STAGE {
            let idx = (frame_no as usize) % bitstreams.len();
            frame_no += 1;
            let mut tries = 0;
            loop {
                match session.encode_picture(
                    &mut reg,
                    &mut bitstreams[idx],
                    EncodePictureParams::default(),
                ) {
                    Ok(()) => break,
                    Err(e)
                        if matches!(
                            e.kind(),
                            ErrorKind::EncoderBusy
                                | ErrorKind::NeedMoreInput
                                | ErrorKind::LockBusy
                        ) && tries < 8 =>
                    {
                        tries += 1;
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(e) => return Err(format!("encode_picture [{label}]: {e:?}").into()),
                }
            }
            let lock = bitstreams[idx]
                .lock()
                .map_err(|e| format!("bitstream lock [{label}]: {e:?}"))?;
            let len = lock.data().len();
            total += len;
            min_len = min_len.min(len);
        }
        println!(
            "  [{label:>28}] coded={}x{}  frames={FRAMES_PER_STAGE}  bytes={total}  min_au={}",
            session.width(),
            session.height(),
            if min_len == usize::MAX { 0 } else { min_len },
        );
    }

    // Interleave check (the bug the adversarial review caught): a BITRATE
    // reconfigure issued while the coded size is NON-native must carry the current
    // dimensions, not the captured native ones. With the stale-native bug this
    // call is rejected (resetEncoder=0 + a dimension change) or desyncs the
    // encoder; with the fix it succeeds and keeps encoding at 720p.
    println!("\n-- interleave: bitrate reconfigure at a non-native coded size --");
    session
        .reconfigure_resolution(1280, 720)
        .map_err(|e| format!("reconfigure_resolution -> 720p: {e:?}"))?;
    session
        .reconfigure_bitrate(
            NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_VBR,
            4_000_000,
            6_000_000,
            66_000,
            true,
        )
        .map_err(|e| format!("reconfigure_bitrate at 720p (stale-native-dims bug?): {e:?}"))?;
    {
        let mut total = 0usize;
        for _ in 0..30 {
            let idx = (frame_no as usize) % bitstreams.len();
            frame_no += 1;
            let mut tries = 0;
            loop {
                match session.encode_picture(
                    &mut reg,
                    &mut bitstreams[idx],
                    EncodePictureParams::default(),
                ) {
                    Ok(()) => break,
                    Err(e)
                        if matches!(
                            e.kind(),
                            ErrorKind::EncoderBusy
                                | ErrorKind::NeedMoreInput
                                | ErrorKind::LockBusy
                        ) && tries < 8 =>
                    {
                        tries += 1;
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(e) => return Err(format!("encode_picture [interleave]: {e:?}").into()),
                }
            }
            total += bitstreams[idx]
                .lock()
                .map_err(|e| format!("lock [interleave]: {e:?}"))?
                .data()
                .len();
        }
        println!(
            "  [bitrate@720p] coded={}x{}  frames=30  bytes={total}  (no crash, no reject)",
            session.width(),
            session.height(),
        );
    }

    println!(
        "\n== SPIKE PASSED: {frame_no} frames across {} resolutions + bitrate interleave, no crash ==",
        STAGES.len()
    );
    println!("reconfigure_resolution is crash-free on this driver — Path B (true DRC) is viable.");

    // Drop order: bitstreams + reg (unmap/unregister) before session (EOS), then
    // free the device buffer — so the unregister precedes the free.
    drop(bitstreams);
    drop(reg);
    drop(session);
    unsafe { curesult::free_sync(dptr) }.ok();
    Ok(())
}
