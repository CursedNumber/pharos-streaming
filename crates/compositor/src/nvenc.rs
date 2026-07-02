//! Direct NVENC (nvEncodeAPI) HEVC encoding from a CUDA device buffer.
//!
//! M1 #1, Stage C: proves the NVENC-from-CUDA path end to end (no ffmpeg) by
//! encoding an animated test pattern uploaded to a CUDA buffer. Stage B then
//! replaces the host upload with a zero-copy GL→CUDA EGL-image import.

use std::ffi::c_void;
use std::fs::File;
use std::io::Write;

use cudarc::driver::{result, CudaContext};
use nvidia_video_codec_sdk::sys::nvEncodeAPI::{
    NV_ENC_BUFFER_FORMAT::{NV_ENC_BUFFER_FORMAT_ABGR, NV_ENC_BUFFER_FORMAT_ABGR10},
    NV_ENC_CODEC_HEVC_GUID, NV_ENC_HEVC_PROFILE_MAIN10_GUID, NV_ENC_INPUT_RESOURCE_TYPE,
    NV_ENC_PIC_PARAMS_HEVC, NV_ENC_PRESET_P4_GUID, NV_ENC_SEI_PAYLOAD, NV_ENC_TUNING_INFO,
    NV_ENC_VUI_COLOR_PRIMARIES, NV_ENC_VUI_MATRIX_COEFFS, NV_ENC_VUI_TRANSFER_CHARACTERISTIC,
};
use nvidia_video_codec_sdk::{CodecPictureParams, EncodePictureParams, Encoder, EncoderInitParams};
use tracing::info;

/// Animated RGBA test pattern (ABGR byte order: R,G,B,A).
fn fill_test_pattern(buf: &mut [u8], width: u32, height: u32, i: u32, frames: u32) {
    let t = i as f32 / frames.max(1) as f32;
    for y in 0..height {
        for x in 0..width {
            let idx = ((y * width + x) * 4) as usize;
            buf[idx] = (255.0 * x as f32 / width as f32) as u8; // R
            buf[idx + 1] = (255.0 * y as f32 / height as f32) as u8; // G
            buf[idx + 2] = (255.0 * t) as u8; // B
            buf[idx + 3] = 255; // A
        }
    }
}

/// Encode `frames` of an animated test pattern to a raw HEVC stream at `out_path`
/// using NVENC driven directly from CUDA device memory.
pub fn encode_test_pattern(
    width: u32,
    height: u32,
    fps: u32,
    frames: u32,
    out_path: &str,
) -> anyhow::Result<()> {
    let ctx = CudaContext::new(0).map_err(|e| anyhow::anyhow!("cuda init: {e:?}"))?;
    ctx.bind_to_thread()
        .map_err(|e| anyhow::anyhow!("bind ctx: {e:?}"))?;
    let encoder = Encoder::initialize_with_cuda(ctx.clone())
        .map_err(|e| anyhow::anyhow!("nvenc init: {e:?}"))?;

    let encode_guid = NV_ENC_CODEC_HEVC_GUID;
    let preset_guid = NV_ENC_PRESET_P4_GUID;
    let tuning = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    let buffer_format = NV_ENC_BUFFER_FORMAT_ABGR;

    let mut preset_config = encoder
        .get_preset_config(encode_guid, preset_guid, tuning)
        .map_err(|e| anyhow::anyhow!("preset config: {e:?}"))?;

    let mut init = EncoderInitParams::new(encode_guid, width, height);
    init.preset_guid(preset_guid)
        .tuning_info(tuning)
        .framerate(fps, 1)
        .enable_picture_type_decision()
        .encode_config(&mut preset_config.presetCfg);

    let session = encoder
        .start_session(buffer_format, init)
        .map_err(|e| anyhow::anyhow!("start session: {e:?}"))?;

    let num_bufs = (preset_config.presetCfg.frameIntervalP.max(0)
        + preset_config.presetCfg.rcParams.lookaheadDepth as i32)
        .max(1) as usize;
    let mut outputs: Vec<_> = (0..num_bufs)
        .map(|_| session.create_output_bitstream())
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("create bitstream: {e:?}"))?;

    let pitch = width * 4;
    let size = (pitch * height) as usize;
    let mut host = vec![0u8; size];
    let mut out_file = File::create(out_path)?;
    info!("nvenc(stage C): {frames} frames {width}x{height} HEVC → {out_path}");

    // Synchronous cuMemAlloc — NVENC won't register async-mempool pointers.
    let dptr =
        unsafe { result::malloc_sync(size) }.map_err(|e| anyhow::anyhow!("cuMemAlloc: {e:?}"))?;
    let mut reg = session
        .register_generic_resource(
            (),
            NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
            dptr as *mut c_void,
            pitch,
        )
        .map_err(|e| anyhow::anyhow!("register resource: {e:?}"))?;

    for i in 0..frames {
        fill_test_pattern(&mut host, width, height, i, frames);
        unsafe { result::memcpy_htod_sync(dptr, &host) }
            .map_err(|e| anyhow::anyhow!("htod copy: {e:?}"))?;

        let ob = &mut outputs[i as usize % num_bufs];
        session
            .encode_picture(&mut reg, ob, Default::default())
            .map_err(|e| anyhow::anyhow!("encode_picture: {e:?}"))?;

        let lock = ob
            .lock()
            .map_err(|e| anyhow::anyhow!("bitstream lock: {e:?}"))?;
        out_file.write_all(lock.data())?;
    }

    drop(reg);
    unsafe { result::free_sync(dptr) }.map_err(|e| anyhow::anyhow!("free: {e:?}"))?;

    info!("nvenc(stage C) complete: {out_path}");
    Ok(())
}

/// Fill an A2B10G10R10 (NVENC `ABGR10`) buffer with an animated 10-bit grayscale
/// luminance ramp. A genuine 10-bit ramp renders smooth; if any stage truncates
/// to 8-bit the ramp bands visibly — the diagnostic value of the spike. Word
/// layout: R in bits [9:0], G [19:10], B [29:20], A [31:30], stored little-endian.
fn fill_pq_gradient_abgr10(buf: &mut [u8], width: u32, height: u32, i: u32, frames: u32) {
    let phase = ((i as f32 / frames.max(1) as f32) * 1024.0) as u32;
    for y in 0..height {
        for x in 0..width {
            let base = (x * 1024 / width.max(1)).min(1023);
            let v = (base + phase) & 0x3FF; // animated wrap so successive frames differ
            let word: u32 = (0b11 << 30) | (v << 20) | (v << 10) | v; // A=3, B=G=R=v
            let idx = ((y * width + x) * 4) as usize;
            buf[idx..idx + 4].copy_from_slice(&word.to_le_bytes());
        }
    }
}

/// Static HDR10 metadata SEI payloads (BT.2020 / 1000-nit mastering display).
/// Returns `(mastering_display_colour_volume [type 137], content_light_level
/// [type 144])` as raw SEI payload bytes per H.265 Annex D — NVENC wraps each in
/// a PREFIX_SEI NAL. Placeholder defaults; the live path will source these from
/// the game surface / config.
pub(crate) fn hdr10_sei_payloads() -> (Vec<u8>, Vec<u8>) {
    // Mastering Display Colour Volume (ST 2086): primaries in display order
    // (G, B, R), chromaticity in units of 0.00002, luminance in units of
    // 0.0001 cd/m². BT.2020 primaries + D65 white point.
    let mut mdcv = Vec::with_capacity(24);
    for &(x, y) in &[
        (8500u16, 39850u16), // G (0.170, 0.797)
        (6550, 2300),        // B (0.131, 0.046)
        (35400, 14600),      // R (0.708, 0.292)
        (15635, 16450),      // white D65 (0.3127, 0.3290)
    ] {
        mdcv.extend_from_slice(&x.to_be_bytes());
        mdcv.extend_from_slice(&y.to_be_bytes());
    }
    mdcv.extend_from_slice(&10_000_000u32.to_be_bytes()); // max 1000 cd/m²
    mdcv.extend_from_slice(&1u32.to_be_bytes()); // min 0.0001 cd/m²
    debug_assert_eq!(mdcv.len(), 24);

    // Content Light Level Info: MaxCLL, MaxFALL (cd/m²).
    let mut cll = Vec::with_capacity(4);
    cll.extend_from_slice(&1000u16.to_be_bytes());
    cll.extend_from_slice(&400u16.to_be_bytes());
    (mdcv, cll)
}

/// HDR10 sibling of [`encode_test_pattern`]: encodes a 10-bit HEVC **Main10**
/// stream with BT.2020 primaries + PQ (ST-2084) transfer signalled in the VUI
/// and static Mastering-Display (SEI 137) + Content-Light-Level (SEI 144)
/// metadata on the IDR. Proves the full HDR10 encoder config against the in-tree
/// NVENC SDK *before* the capture pipeline is widened to 10-bit — the source
/// here is a CPU-filled A2B10G10R10 buffer, isolating the encoder from the GL
/// interop.
pub fn encode_test_pattern_hdr10(
    width: u32,
    height: u32,
    fps: u32,
    frames: u32,
    out_path: &str,
) -> anyhow::Result<()> {
    let ctx = CudaContext::new(0).map_err(|e| anyhow::anyhow!("cuda init: {e:?}"))?;
    ctx.bind_to_thread()
        .map_err(|e| anyhow::anyhow!("bind ctx: {e:?}"))?;
    let encoder = Encoder::initialize_with_cuda(ctx.clone())
        .map_err(|e| anyhow::anyhow!("nvenc init: {e:?}"))?;

    let encode_guid = NV_ENC_CODEC_HEVC_GUID;
    let preset_guid = NV_ENC_PRESET_P4_GUID;
    let tuning = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    let buffer_format = NV_ENC_BUFFER_FORMAT_ABGR10;

    let mut preset_config = encoder
        .get_preset_config(encode_guid, preset_guid, tuning)
        .map_err(|e| anyhow::anyhow!("preset config: {e:?}"))?;

    // Declare HEVC Main10 + 10-bit depth + BT.2020/PQ VUI. `profileGUID` is a
    // plain NV_ENC_CONFIG field; the codec config + VUI live in a union, hence
    // the unsafe access (mirrors `apply_codec_lowlatency` in multi.rs).
    preset_config.presetCfg.profileGUID = NV_ENC_HEVC_PROFILE_MAIN10_GUID;
    // SAFETY: get_preset_config(HEVC, ..) initialised the HEVC arm of the union.
    unsafe {
        let hevc = &mut preset_config.presetCfg.encodeCodecConfig.hevcConfig;
        hevc.set_pixelBitDepthMinus8(2); // 10-bit encoded output
        let vui = &mut hevc.hevcVUIParameters;
        vui.videoSignalTypePresentFlag = 1;
        vui.videoFullRangeFlag = 0; // limited (narrow) range — standard HDR10 signal
        vui.colourDescriptionPresentFlag = 1;
        vui.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT2020;
        vui.transferCharacteristics =
            NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SMPTE2084;
        vui.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL;
    }

    let mut init = EncoderInitParams::new(encode_guid, width, height);
    init.preset_guid(preset_guid)
        .tuning_info(tuning)
        .framerate(fps, 1)
        .enable_picture_type_decision()
        .encode_config(&mut preset_config.presetCfg);

    let session = encoder
        .start_session(buffer_format, init)
        .map_err(|e| anyhow::anyhow!("start session (ABGR10/Main10): {e:?}"))?;

    let num_bufs = (preset_config.presetCfg.frameIntervalP.max(0)
        + preset_config.presetCfg.rcParams.lookaheadDepth as i32)
        .max(1) as usize;
    let mut outputs: Vec<_> = (0..num_bufs)
        .map(|_| session.create_output_bitstream())
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("create bitstream: {e:?}"))?;

    let pitch = width * 4; // A2B10G10R10 is 32bpp packed — same stride as ABGR8888
    let size = (pitch * height) as usize;
    let mut host = vec![0u8; size];
    let mut out_file = File::create(out_path)?;
    info!("nvenc(HDR10): {frames} frames {width}x{height} HEVC Main10 BT.2020/PQ → {out_path}");

    let dptr =
        unsafe { result::malloc_sync(size) }.map_err(|e| anyhow::anyhow!("cuMemAlloc: {e:?}"))?;
    let mut reg = session
        .register_generic_resource(
            (),
            NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
            dptr as *mut c_void,
            pitch,
        )
        .map_err(|e| anyhow::anyhow!("register resource: {e:?}"))?;

    // Static HDR10 metadata, attached on the IDR (here: the first frame; the live
    // path re-emits on every IDR to match repeatSPSPPS so joiners resync ≤1s).
    let (mdcv, cll) = hdr10_sei_payloads();

    for i in 0..frames {
        fill_pq_gradient_abgr10(&mut host, width, height, i, frames);
        unsafe { result::memcpy_htod_sync(dptr, &host) }
            .map_err(|e| anyhow::anyhow!("htod copy: {e:?}"))?;

        let ob = &mut outputs[i as usize % num_bufs];

        if i == 0 {
            let mut sei = [
                NV_ENC_SEI_PAYLOAD {
                    payloadSize: mdcv.len() as u32,
                    payloadType: 137, // mastering_display_colour_volume
                    payload: mdcv.as_ptr() as *mut u8,
                },
                NV_ENC_SEI_PAYLOAD {
                    payloadSize: cll.len() as u32,
                    payloadType: 144, // content_light_level_info
                    payload: cll.as_ptr() as *mut u8,
                },
            ];
            // SAFETY: NV_ENC_PIC_PARAMS_HEVC is POD (all scalar/pointer fields);
            // NVENC copies the SEI during the synchronous encode call, so `sei`
            // and the payload Vecs outlive the pointers.
            let mut hevc_pic: NV_ENC_PIC_PARAMS_HEVC = unsafe { std::mem::zeroed() };
            hevc_pic.seiPayloadArrayCnt = sei.len() as u32;
            hevc_pic.seiPayloadArray = sei.as_mut_ptr();
            let params = EncodePictureParams {
                force_idr: true,
                codec_params: Some(CodecPictureParams::Hevc(hevc_pic)),
                ..Default::default()
            };
            session
                .encode_picture(&mut reg, ob, params)
                .map_err(|e| anyhow::anyhow!("encode_picture(idr+sei): {e:?}"))?;
        } else {
            session
                .encode_picture(&mut reg, ob, Default::default())
                .map_err(|e| anyhow::anyhow!("encode_picture: {e:?}"))?;
        }

        let lock = ob
            .lock()
            .map_err(|e| anyhow::anyhow!("bitstream lock: {e:?}"))?;
        out_file.write_all(lock.data())?;
    }

    drop(reg);
    unsafe { result::free_sync(dptr) }.map_err(|e| anyhow::anyhow!("free: {e:?}"))?;
    info!("nvenc(HDR10) complete: {out_path}");
    Ok(())
}
