//! Defines [`Session`] which represents an ongoing encoder session.
//!
//! You need to start a session using [`Encoder::start_session`] before
//! you can initialize input or output buffers, and before you can encode
//! frames. The [`Session`] also stores some information such as the encode
//! width and height so that you do not have to keep repeating it each time.

use std::fmt::Debug;

use super::{
    api::ENCODE_API,
    encoder::Encoder,
    result::{EncodeError, ErrorKind},
};
use crate::{
    sys::nvEncodeAPI::{
        GUID,
        NV_ENC_BUFFER_FORMAT,
        NV_ENC_CODEC_AV1_GUID,
        NV_ENC_CODEC_H264_GUID,
        NV_ENC_CODEC_HEVC_GUID,
        NV_ENC_CODEC_PIC_PARAMS,
        NV_ENC_CONFIG,
        NV_ENC_INITIALIZE_PARAMS,
        NV_ENC_PARAMS_RC_MODE,
        NV_ENC_PIC_FLAGS,
        NV_ENC_PIC_PARAMS,
        NV_ENC_PIC_PARAMS_AV1,
        NV_ENC_PIC_PARAMS_H264,
        NV_ENC_PIC_PARAMS_HEVC,
        NV_ENC_PIC_PARAMS_VER,
        NV_ENC_PIC_STRUCT,
        NV_ENC_PIC_TYPE,
        NV_ENC_RECONFIGURE_PARAMS,
        NV_ENC_RECONFIGURE_PARAMS_VER,
    },
    EncoderInput,
    EncoderOutput,
};

/// An encoding session to create input/output buffers and encode frames.
///
/// You need to call [`Encoder::start_session`] before you can
/// encode frames using the session. On drop, the session will automatically
/// send an empty EOS frame to flush the encoder.
pub struct Session {
    pub(crate) encoder: Encoder,
    // Interior-mutable so a live resolution change (`reconfigure_resolution`)
    // can update the dimensions that `encode_picture` submits, while output
    // buffers / registered resources still hold shared (`&self`) borrows of the
    // session. The session is single-render-thread (`!Sync`), so `Cell` is sound.
    pub(crate) width: std::cell::Cell<u32>,
    pub(crate) height: std::cell::Cell<u32>,
    pub(crate) buffer_format: NV_ENC_BUFFER_FORMAT,
    pub(crate) encode_guid: GUID,
    /// Init params + the encode config captured at [`Encoder::start_session`], so
    /// the running encoder can be LIVE-reconfigured (see
    /// [`Session::reconfigure_bitrate`] / [`Session::reconfigure_resolution`])
    /// without a teardown. Both are POD copies; the reconfigure path works on a
    /// stack copy so it needs only `&self` (which matters because output buffers
    /// borrow the session).
    ///
    /// `init_params` holds the NATIVE dimensions captured at create — never the
    /// live coded size; the current coded size is `width`/`height` (the Cells
    /// above), which both reconfigure paths read so dimensions always travel
    /// together. `config` is interior-mutable so `reconfigure_bitrate` can
    /// PERSIST its rate-control override: otherwise a later
    /// `reconfigure_resolution` (which reattaches the whole config) would revert
    /// the live bitrate back to the session-create cap. `None` if the session was
    /// started without an encode config.
    pub(crate) init_params: NV_ENC_INITIALIZE_PARAMS,
    pub(crate) config: std::cell::Cell<Option<NV_ENC_CONFIG>>,
}

impl Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // NV_ENC_CONFIG does not derive Debug, so elide it.
        f.debug_struct("Session")
            .field("encoder", &self.encoder)
            .field("width", &self.width.get())
            .field("height", &self.height.get())
            .field("buffer_format", &self.buffer_format)
            .field("encode_guid", &self.encode_guid)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Live rate-control reconfigure (average/max bitrate + VBV) on the RUNNING
    /// encoder via `NvEncReconfigureEncoder`.
    ///
    /// This changes the rate control IN PLACE — it does **not** tear down and
    /// recreate the session, so it avoids the driver crash a full re-init can
    /// trigger. It is RC-only (no resolution change), so no max-encode-dimension
    /// envelope is required. `force_idr` emits a keyframe so the new rate applies
    /// cleanly from a fresh GOP.
    ///
    /// Added on top of upstream 0.4.0: the published crate exposes the
    /// `reconfigure_encoder` entry point but no safe wrapper, and seals the
    /// encoder handle. Reuses the init params captured at
    /// [`Encoder::start_session`] so the reconfigure matches the original setup
    /// exactly. Errors if the session was started with no encode config.
    pub fn reconfigure_bitrate(
        &self,
        rate_control_mode: NV_ENC_PARAMS_RC_MODE,
        average_bitrate: u32,
        max_bitrate: u32,
        vbv_buffer_size: u32,
        force_idr: bool,
    ) -> Result<(), EncodeError> {
        // Work on a stack copy of the captured config: NVENC reads the params
        // synchronously during the reconfigure call. `&self` (not `&mut`) is
        // required because output buffers/registered resources hold shared borrows
        // of the session. RC-only override; the rest of the config is whatever the
        // session was started with.
        let mut config = self.config.get().ok_or_else(|| {
            EncodeError::new(
                ErrorKind::InvalidParam,
                Some("reconfigure_bitrate: session has no encode config".to_string()),
            )
        })?;
        config.rcParams.rateControlMode = rate_control_mode;
        config.rcParams.averageBitRate = average_bitrate;
        config.rcParams.maxBitRate = max_bitrate;
        config.rcParams.vbvBufferSize = vbv_buffer_size;
        config.rcParams.vbvInitialDelay = vbv_buffer_size;
        // PERSIST the RC override so a later `reconfigure_resolution` (which
        // reattaches the whole config) carries this live bitrate forward instead
        // of reverting to the session-create cap. NV_ENC_CONFIG is POD/Copy, so
        // the local `config` stays valid for the FFI call below after the set.
        self.config.set(Some(config));
        let mut init_params = self.init_params;
        // Submit the CURRENT coded size — a prior resolution reconfigure may have
        // changed it, and the captured init_params still holds the native size.
        // Sending stale native dims here (with resetEncoder=0) would either be
        // rejected or desync encodeWidth from the downscaled inputWidth.
        init_params.encodeWidth = self.width.get();
        init_params.encodeHeight = self.height.get();
        init_params.darWidth = self.width.get();
        init_params.darHeight = self.height.get();
        init_params.encodeConfig = &mut config as *mut NV_ENC_CONFIG;
        // SAFETY: `NV_ENC_RECONFIGURE_PARAMS` is plain-old-data; zero it, then set
        // the version + fields (the documented init pattern). `config`/`init_params`
        // live on this stack frame for the whole synchronous FFI call below.
        let mut params: NV_ENC_RECONFIGURE_PARAMS = unsafe { std::mem::zeroed() };
        params.version = NV_ENC_RECONFIGURE_PARAMS_VER;
        params.reInitEncodeParams = init_params;
        params.set_resetEncoder(0);
        params.set_forceIDR(u32::from(force_idr));
        unsafe { (ENCODE_API.reconfigure_encoder)(self.encoder.ptr, &mut params) }
            .result(&self.encoder)
    }

    /// Live RESOLUTION reconfigure on the RUNNING encoder via
    /// `NvEncReconfigureEncoder` — changes the encoded `encodeWidth`/
    /// `encodeHeight` IN PLACE, with **no** teardown/re-init (a full re-init
    /// crashes the driver). Like [`Session::reconfigure_bitrate`] it works on a
    /// stack copy of the captured init params/config and needs only `&self`
    /// (output buffers/registered resources hold shared borrows of the session).
    ///
    /// The new dimensions must be `<=` the `maxEncodeWidth`/`maxEncodeHeight`
    /// envelope set via [`EncoderInitParams::max_encode_resolution`] at session
    /// create; NVENC rejects the change if no envelope was set or if it exceeds
    /// it. The change resets the encoder (`resetEncoder = 1`) and forces a
    /// keyframe so the reference chain restarts cleanly at the new size. The
    /// caller is responsible for feeding the next frame at the new dimensions
    /// (the top-left `width × height` region of the input buffer; the input
    /// pitch is unchanged, so a buffer registered at the envelope size can be
    /// reused without re-registration). The captured encode config is reattached
    /// so the codec/GOP/RC setup is preserved across the change.
    ///
    /// Updates the cached `width`/`height` so the next
    /// [`Session::encode_picture`] submits the new `inputWidth`/`inputHeight`.
    ///
    /// Added on top of upstream 0.4.0 alongside
    /// [`Session::reconfigure_bitrate`]; the published crate exposes no
    /// reconfigure wrapper at all.
    pub fn reconfigure_resolution(&self, width: u32, height: u32) -> Result<(), EncodeError> {
        // Reattach a stack copy of the CURRENT encode config so the running setup
        // (codec params, GOP, rate control) carries across the change — only the
        // dimensions move. Reading `self.config.get()` (not the create-time field)
        // is what carries the live bitrate forward: `reconfigure_bitrate` persists
        // its RC override there, so a resolution step preserves the adapted rate
        // instead of snapping back to the session-create cap. `config` lives on
        // this frame for the whole synchronous FFI call below.
        let mut config = self.config.get();
        let mut init_params = self.init_params;
        if let Some(c) = config.as_mut() {
            init_params.encodeConfig = c as *mut NV_ENC_CONFIG;
        }
        init_params.encodeWidth = width;
        init_params.encodeHeight = height;
        // Keep the display aspect ratio square to the coded size so the client
        // renders the new resolution without stretch (it learns the size via the
        // stream-geometry datagram).
        init_params.darWidth = width;
        init_params.darHeight = height;
        // SAFETY: `NV_ENC_RECONFIGURE_PARAMS` is plain-old-data; zero it, then set
        // the version + fields (the documented init pattern).
        let mut params: NV_ENC_RECONFIGURE_PARAMS = unsafe { std::mem::zeroed() };
        params.version = NV_ENC_RECONFIGURE_PARAMS_VER;
        params.reInitEncodeParams = init_params;
        // A resolution change requires resetting the encoder; pair it with a
        // forced IDR so the new GOP starts clean (no refs at the old size).
        params.set_resetEncoder(1);
        params.set_forceIDR(1);
        unsafe { (ENCODE_API.reconfigure_encoder)(self.encoder.ptr, &mut params) }
            .result(&self.encoder)?;
        self.width.set(width);
        self.height.set(height);
        Ok(())
    }

    /// The current coded width the session encodes (tracks
    /// [`Session::reconfigure_resolution`]).
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width.get()
    }

    /// The current coded height the session encodes (tracks
    /// [`Session::reconfigure_resolution`]).
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height.get()
    }

    /// Get the encoder used for this session.
    ///
    /// This might be useful if you want to use some of
    /// the functions on [`Encoder`].
    ///
    /// # Examples
    ///
    /// ```
    /// # use cudarc::driver::CudaContext;
    /// # use nvidia_video_codec_sdk::{
    /// #     sys::nvEncodeAPI::{
    /// #         NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
    /// #         NV_ENC_CODEC_H264_GUID,
    /// #     },
    /// #     Encoder, EncoderInitParams
    /// # };
    /// //* Create encoder. *//
    /// # let cuda_ctx = CudaContext::new(0).unwrap();
    /// # let encoder = Encoder::initialize_with_cuda(cuda_ctx).unwrap();
    ///
    /// //* Set `encode_guid` and check that H.264 encoding is supported. *//
    /// # let encode_guid = NV_ENC_CODEC_H264_GUID;
    /// # let encode_guids = encoder.get_encode_guids().unwrap();
    /// # assert!(encode_guids.contains(&encode_guid));
    ///
    /// let session = encoder
    ///     .start_session(
    ///         NV_ENC_BUFFER_FORMAT_ARGB,
    ///         EncoderInitParams::new(encode_guid, 1920, 1080),
    ///     )
    ///     .unwrap();
    /// // We can still use the encoder like this:
    /// let _input_formats = session
    ///     .get_encoder()
    ///     .get_supported_input_formats(encode_guid);
    /// ```
    #[must_use]
    pub fn get_encoder(&self) -> &Encoder {
        &self.encoder
    }

    /// Encode a frame.
    ///
    /// See [NVIDIA docs](https://docs.nvidia.com/video-technologies/video-codec-sdk/12.0/nvenc-video-encoder-api-prog-guide/index.html#submitting-input-frame-for-encoding).
    ///
    /// # Errors
    ///
    /// Could error if the encode picture parameters were invalid or otherwise
    /// incorrect, or if we run out memory.
    ///
    /// There are two recoverable errors:
    /// - If this returns an error with
    ///   [`ErrorKind::EncoderBusy`](super::ErrorKind::EncoderBusy) then you
    ///   should retry after a few milliseconds.
    /// - If this returns an error with
    ///   [`ErrorKind::NeedMoreInput`](super::ErrorKind::NeedMoreInput), the
    ///   client should not lock the output bitstream yet. They should continue
    ///   encoding until this function returns `Ok`, and then lock the
    ///   bitstreams in the order in which they were originally used.
    ///
    /// # Panics
    ///
    /// Panics if codec specific parameters are provided for a different codec
    /// than the one used in the session.
    ///
    /// # Examples
    ///
    /// ```
    /// # use cudarc::driver::CudaContext;
    /// # use nvidia_video_codec_sdk::{
    /// #     sys::nvEncodeAPI::{
    /// #         NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
    /// #         NV_ENC_CODEC_H264_GUID,
    /// #         NV_ENC_PIC_PARAMS,
    /// #         NV_ENC_PIC_STRUCT,
    /// #     },
    /// #     Encoder, EncoderInitParams,
    /// #     EncodePictureParams
    /// # };
    /// # const WIDTH: u32 = 1920;
    /// # const HEIGHT: u32 = 1080;
    /// # const DATA_LEN: usize = (WIDTH * HEIGHT * 4) as usize;
    /// //* Create encoder. *//
    /// # let cuda_ctx = CudaContext::new(0).unwrap();
    /// # let encoder = Encoder::initialize_with_cuda(cuda_ctx).unwrap();
    ///
    /// //* Set `encode_guid` and `buffer_format`, and check that H.264 encoding and the ARGB format are supported. *//
    /// # let encode_guid = NV_ENC_CODEC_H264_GUID;
    /// # let encode_guids = encoder.get_encode_guids().unwrap();
    /// # assert!(encode_guids.contains(&encode_guid));
    /// # let buffer_format = NV_ENC_BUFFER_FORMAT_ARGB;
    /// # let input_formats = encoder.get_supported_input_formats(encode_guid).unwrap();
    /// # assert!(input_formats.contains(&buffer_format));
    ///
    /// // Begin encoder session.
    /// let mut initialize_params = EncoderInitParams::new(encode_guid, WIDTH, HEIGHT);
    /// initialize_params.display_aspect_ratio(16, 9)
    ///     .framerate(30, 1)
    ///     .enable_picture_type_decision();
    /// let session = encoder.start_session(
    ///     buffer_format,
    ///     initialize_params,
    /// ).unwrap();
    ///
    /// //* Create input and output buffers. *//
    /// # let mut input_buffer = session
    /// #     .create_input_buffer()
    /// #     .unwrap();
    /// # let mut output_bitstream = session.create_output_bitstream().unwrap();
    ///
    /// // Encode frame.
    /// unsafe { input_buffer.lock().unwrap().write(&[0; DATA_LEN]) };
    /// session
    ///     .encode_picture(
    ///         &mut input_buffer,
    ///         &mut output_bitstream,
    ///         // Optional picture parameters
    ///         EncodePictureParams {
    ///             input_timestamp: 42,
    ///             ..Default::default()
    ///         }
    ///     )
    ///     .unwrap();
    /// # // TODO: check that output is correct.
    /// let _data = output_bitstream.lock().unwrap().data();
    /// ```
    pub fn encode_picture<I: EncoderInput, O: EncoderOutput>(
        &self,
        input_buffer: &mut I,
        output_bitstream: &mut O,
        params: EncodePictureParams,
    ) -> Result<(), EncodeError> {
        if let Some(codec_params) = &params.codec_params {
            assert_eq!(
                codec_params.get_codec_guid(),
                self.encode_guid,
                "The provided codec specific params must match the codec used"
            );
        }
        let mut encode_pic_params = NV_ENC_PIC_PARAMS {
            version: NV_ENC_PIC_PARAMS_VER,
            inputWidth: self.width.get(),
            inputHeight: self.height.get(),
            inputPitch: input_buffer.pitch(),
            inputBuffer: input_buffer.handle(),
            outputBitstream: output_bitstream.handle(),
            bufferFmt: self.buffer_format,
            pictureStruct: NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME,
            inputTimeStamp: params.input_timestamp,
            codecPicParams: params.codec_params.map(Into::into).unwrap_or_default(),
            pictureType: params.picture_type,
            // On-demand keyframe: FORCEIDR makes THIS frame an IDR regardless of
            // picture-type-decision, resetting the reference chain so a client
            // whose reference was lost (a dropped P-frame) resyncs immediately
            // instead of smearing until the next periodic IDR. OUTPUT_SPSPPS
            // re-emits the sequence header with it so a mid-stream joiner/resyncer
            // can decode it standalone.
            encodePicFlags: if params.force_idr {
                NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32
                    | NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS as u32
            } else {
                0
            },
            ..Default::default()
        };
        unsafe { (ENCODE_API.encode_picture)(self.encoder.ptr, &mut encode_pic_params) }
            .result(&self.encoder)
    }

    /// Send an EOS notifications to flush the encoder.
    ///
    /// This function is called automatically on drop, but if you wish to
    /// get the data after flushing, you should call this function yourself.
    ///
    /// # Errors
    ///
    /// Could error if we run out of memory.
    ///
    /// If this returns an error with
    /// [`ErrorKind::EncoderBusy`](super::ErrorKind::EncoderBusy) then you
    /// should retry after a few milliseconds.
    pub fn end_of_stream(&self) -> Result<(), EncodeError> {
        let mut encode_pic_params = NV_ENC_PIC_PARAMS::end_of_stream();
        unsafe { (ENCODE_API.encode_picture)(self.encoder.ptr, &mut encode_pic_params) }
            .result(&self.encoder)
    }
}

/// Send an EOS notifications on drop to flush the encoder.
impl Drop for Session {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            self.end_of_stream()
                .expect("The encoder should not be busy.");
        }
    }
}

/// Optional parameters for [`Session::encode_picture`].
#[allow(missing_debug_implementations)] // CodecPictureParams doesn't implement Debug
pub struct EncodePictureParams {
    /// Opaque data used for identifying the corresponding encoded frame
    pub input_timestamp: u64,
    /// The picture type to use, if picture type decision is disabled in the
    /// encoder
    pub picture_type: NV_ENC_PIC_TYPE,
    /// Force this frame to be an IDR (`NV_ENC_PIC_FLAG_FORCEIDR` +
    /// `OUTPUT_SPSPPS`). Works even with picture-type decision enabled — used for
    /// on-demand keyframes that resync a client after a lost reference frame.
    pub force_idr: bool,
    /// Codec-specific parameters
    pub codec_params: Option<CodecPictureParams>,
}

impl Default for EncodePictureParams {
    fn default() -> Self {
        Self {
            input_timestamp: 0,
            picture_type: NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_UNKNOWN,
            force_idr: false,
            codec_params: None,
        }
    }
}

/// Codec specific picture parameters
#[allow(missing_debug_implementations)] // NV_ENC_PIC_PARAMS_H264 contains a union, thus doesn't derive Debug
pub enum CodecPictureParams {
    /// Parameters for H.264
    H264(NV_ENC_PIC_PARAMS_H264),
    /// Parameters for HEVC or H.265
    Hevc(NV_ENC_PIC_PARAMS_HEVC),
    /// Parameters for AV1
    Av1(NV_ENC_PIC_PARAMS_AV1),
}

impl CodecPictureParams {
    /// Returns the GUID representing the codec for which the parameters are
    /// specified.
    #[must_use]
    pub fn get_codec_guid(&self) -> GUID {
        match self {
            Self::H264(_) => NV_ENC_CODEC_H264_GUID,
            Self::Hevc(_) => NV_ENC_CODEC_HEVC_GUID,
            Self::Av1(_) => NV_ENC_CODEC_AV1_GUID,
        }
    }
}

impl From<CodecPictureParams> for NV_ENC_CODEC_PIC_PARAMS {
    fn from(value: CodecPictureParams) -> Self {
        match value {
            CodecPictureParams::H264(params) => Self {
                h264PicParams: params,
            },
            CodecPictureParams::Hevc(params) => Self {
                hevcPicParams: params,
            },
            CodecPictureParams::Av1(params) => Self {
                av1PicParams: params,
            },
        }
    }
}
