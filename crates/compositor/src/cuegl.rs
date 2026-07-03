// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! CUDA ↔ EGL interop bridge (M1 #1, Stage B).
//!
//! NVIDIA can't render into an imported dmabuf, so the render target is a
//! GL-native texture. This bridges that texture into CUDA *without a CPU
//! round-trip*: wrap the GL texture as an EGLImage (`eglCreateImage`), register
//! it with CUDA (`cuGraphicsEGLRegisterImage`), get the mapped CUDA array
//! (`cuGraphicsResourceGetMappedEglFrame`), then `cuMemcpy2D` it into a linear
//! device buffer that NVENC encodes.
//!
//! cudarc exposes no EGL interop, so the EGL-image + CUDA-EGL calls are loaded
//! by hand via `libloading` from libEGL/libcuda. The array→linear copy and all
//! plain CUDA types reuse cudarc's `sys`.
//!
//! API floor: `cuGraphicsEGLRegisterImage` exists since CUDA 9.0; the practical
//! floor for headless EGL+GBM on NVIDIA is driver r495+ (≈CUDA 11.5).

use std::ffi::c_void;

use cudarc::driver::sys as cu;

const EGL_GL_TEXTURE_2D: u32 = 0x30B1;

type EglDisplay = *mut c_void;
type EglContext = *mut c_void;
type EglImage = *mut c_void;
type EglClientBuffer = *mut c_void;

/// Mirrors `CUeglFrame_st` from cudaEGL.h (MAX_PLANES = 3). We only read
/// `frame[0]` as a `CUarray` for the single-plane ABGR case.
#[repr(C)]
struct CuEglFrame {
    frame: [cu::CUarray; 3],
    width: u32,
    height: u32,
    depth: u32,
    pitch: u32,
    plane_count: u32,
    num_channels: u32,
    frame_type: u32,
    egl_color_format: u32,
    cu_format: u32,
}

type PfnEglCreateImage =
    unsafe extern "C" fn(EglDisplay, EglContext, u32, EglClientBuffer, *const isize) -> EglImage;
type PfnEglDestroyImage = unsafe extern "C" fn(EglDisplay, EglImage) -> u32;
type PfnCuRegister =
    unsafe extern "C" fn(*mut cu::CUgraphicsResource, EglImage, u32) -> cu::CUresult;
type PfnCuGetEglFrame =
    unsafe extern "C" fn(*mut CuEglFrame, cu::CUgraphicsResource, u32, u32) -> cu::CUresult;
type PfnCuUnregister = unsafe extern "C" fn(cu::CUgraphicsResource) -> cu::CUresult;

/// Loaded EGL/CUDA-EGL entry points + their owning libraries.
pub struct CudaGlBridge {
    egl_create_image: PfnEglCreateImage,
    egl_destroy_image: PfnEglDestroyImage,
    cu_register: PfnCuRegister,
    cu_get_egl_frame: PfnCuGetEglFrame,
    cu_unregister: PfnCuUnregister,
    _libegl: libloading::Library,
    _libcuda: libloading::Library,
}

fn check(rc: cu::CUresult, what: &str) -> anyhow::Result<()> {
    if rc == cu::CUresult::CUDA_SUCCESS {
        Ok(())
    } else {
        Err(anyhow::anyhow!("{what} failed: {rc:?}"))
    }
}

impl CudaGlBridge {
    /// Load the EGL + CUDA-EGL symbols. The CUDA context must already exist.
    ///
    /// # Safety
    /// Loads and calls raw C entry points; the returned function pointers are
    /// only valid while this struct (and thus the libraries) is alive.
    pub unsafe fn new() -> anyhow::Result<Self> {
        let libegl = libloading::Library::new("libEGL.so.1")
            .map_err(|e| anyhow::anyhow!("dlopen libEGL.so.1: {e}"))?;
        let libcuda = libloading::Library::new("libcuda.so.1")
            .map_err(|e| anyhow::anyhow!("dlopen libcuda.so.1: {e}"))?;

        let egl_create_image = *libegl
            .get::<PfnEglCreateImage>(b"eglCreateImage\0")
            .map_err(|e| anyhow::anyhow!("eglCreateImage: {e}"))?;
        let egl_destroy_image = *libegl
            .get::<PfnEglDestroyImage>(b"eglDestroyImage\0")
            .map_err(|e| anyhow::anyhow!("eglDestroyImage: {e}"))?;
        let cu_register = *libcuda
            .get::<PfnCuRegister>(b"cuGraphicsEGLRegisterImage\0")
            .map_err(|e| anyhow::anyhow!("cuGraphicsEGLRegisterImage: {e}"))?;
        let cu_get_egl_frame = *libcuda
            .get::<PfnCuGetEglFrame>(b"cuGraphicsResourceGetMappedEglFrame\0")
            .map_err(|e| anyhow::anyhow!("cuGraphicsResourceGetMappedEglFrame: {e}"))?;
        let cu_unregister = *libcuda
            .get::<PfnCuUnregister>(b"cuGraphicsUnregisterResource\0")
            .map_err(|e| anyhow::anyhow!("cuGraphicsUnregisterResource: {e}"))?;

        Ok(Self {
            egl_create_image,
            egl_destroy_image,
            cu_register,
            cu_get_egl_frame,
            cu_unregister,
            _libegl: libegl,
            _libcuda: libcuda,
        })
    }

    /// Copy a GL texture's contents into a linear CUDA device buffer, zero-copy
    /// across GL→CUDA (one GPU-internal `cuMemcpy2D`, no CPU round-trip).
    ///
    /// `dpy`/`ctx` are the raw EGL handles of the GL context that owns `tex_id`
    /// (must be current). `dst` is a linear `cuMemAlloc`'d buffer with `dst_pitch`
    /// bytes per row. ABGR8888 (4 bytes/pixel) is assumed.
    ///
    /// # Safety
    /// `tex_id` must be a live GL texture in the current EGL context; `dst` must
    /// be a valid device allocation of at least `dst_pitch * height` bytes.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn copy_gl_texture_to_linear(
        &self,
        dpy: EglDisplay,
        ctx: EglContext,
        tex_id: u32,
        dst: cu::CUdeviceptr,
        dst_pitch: usize,
        width: u32,
        height: u32,
    ) -> anyhow::Result<()> {
        let image = (self.egl_create_image)(
            dpy,
            ctx,
            EGL_GL_TEXTURE_2D,
            tex_id as usize as EglClientBuffer,
            std::ptr::null(),
        );
        if image.is_null() {
            anyhow::bail!("eglCreateImage returned EGL_NO_IMAGE");
        }

        let mut res: cu::CUgraphicsResource = std::ptr::null_mut();
        let reg = (self.cu_register)(&mut res, image, 0);
        if let Err(e) = check(reg, "cuGraphicsEGLRegisterImage") {
            (self.egl_destroy_image)(dpy, image);
            return Err(e);
        }

        let result = (|| {
            let mut frame: CuEglFrame = std::mem::zeroed();
            check(
                (self.cu_get_egl_frame)(&mut frame, res, 0, 0),
                "cuGraphicsResourceGetMappedEglFrame",
            )?;
            // Log the mapped frame layout once — the ground truth for whether a
            // 10-bit (RGB10_A2) EGLImage round-trips as a clean single-plane CUDA
            // array. `planes=1 channels=4 pitch≈width*4` confirms the packed path.
            static FRAME_LOG: std::sync::Once = std::sync::Once::new();
            FRAME_LOG.call_once(|| {
                tracing::info!(
                    target: "compositor::cuegl",
                    "CUeglFrame {}x{} planes={} channels={} pitch={} egl_fmt={:#x} cu_fmt={} (copy WidthInBytes={})",
                    frame.width, frame.height, frame.plane_count, frame.num_channels,
                    frame.pitch, frame.egl_color_format, frame.cu_format, width * 4,
                );
            });

            // CUDA_MEMCPY2D has a CUmemorytype enum with no 0 variant, so it
            // can't be zero-initialized — construct it explicitly.
            let copy = cu::CUDA_MEMCPY2D_st {
                srcXInBytes: 0,
                srcY: 0,
                srcMemoryType: cu::CUmemorytype::CU_MEMORYTYPE_ARRAY,
                srcHost: std::ptr::null(),
                srcDevice: 0,
                srcArray: frame.frame[0],
                srcPitch: 0,
                dstXInBytes: 0,
                dstY: 0,
                dstMemoryType: cu::CUmemorytype::CU_MEMORYTYPE_DEVICE,
                dstHost: std::ptr::null_mut(),
                dstDevice: dst,
                dstArray: std::ptr::null_mut(),
                dstPitch: dst_pitch,
                WidthInBytes: (width * 4) as usize,
                Height: height as usize,
            };
            check(cu::cuMemcpy2D_v2(&copy), "cuMemcpy2D")?;
            Ok(())
        })();

        // Always unregister + destroy, even on error.
        (self.cu_unregister)(res);
        (self.egl_destroy_image)(dpy, image);
        result
    }
}
