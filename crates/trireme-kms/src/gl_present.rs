//! GLES → GBM-surface → atomic scanout: the full-rate (120 Hz) presenter.
//!
//! The dumb-buffer path caps at ~60 Hz because nvidia-drm does a per-flip
//! linear→tiled blit of the CPU-mapped buffer. Here we instead render each frame
//! with GLES into a **GBM surface**, which nvidia backs with a native
//! block-linear (tiled) buffer — so scanout needs no per-flip conversion and the
//! flips keep up with the mode's vblank. EGL is set up on the GBM platform; the
//! decoded frame is uploaded as a texture and drawn as a full-screen quad.

use std::collections::HashMap;
use std::ffi::c_void;
use std::fs::File;
use std::ptr;
use std::time::Instant;

use anyhow::{anyhow, Context as _, Result};
use drm::control::{framebuffer, AtomicCommitFlags, Device as ControlDevice, FbCmd2Flags};
use gbm::{
    AsRaw, BufferObject, BufferObjectFlags, Device as GbmDevice, Format, Surface as GbmSurface,
};
use glow::HasContext;
use khronos_egl as egl;

use crate::kms::{self, Card, ModePref, Target};

const PLATFORM_GBM_KHR: egl::Enum = 0x31D7;
const GBM_FORMAT_XRGB8888: i32 = 0x3432_5258; // 'XR24', matches the plane's XR24

type Egl = egl::DynamicInstance<egl::EGL1_5>;

/// GLES rendering state (the bits a frame-render closure touches).
///
/// The decoded frame arrives as **NV12** (Y plane + interleaved CbCr at half
/// resolution) — ffmpeg/cuvid's native output, so no CPU colour conversion. Y
/// goes into a `LUMINANCE` texture, CbCr into a `LUMINANCE_ALPHA` texture, and
/// the fragment shader does YUV→RGB (BT.709 limited) on the GPU during the draw.
pub struct Renderer {
    gl: glow::Context,
    program: glow::Program,
    idle_program: glow::Program,
    tex_y: glow::Texture,
    tex_uv: glow::Texture,
    u_y: Option<glow::UniformLocation>,
    u_uv: Option<glow::UniformLocation>,
    badge_tex: glow::Texture,
    badge_aspect: f32,
    u_badge: Option<glow::UniformLocation>,
    u_rect: Option<glow::UniformLocation>,
    u_alpha: Option<glow::UniformLocation>,
    dim: (i32, i32),
}

impl Renderer {
    /// Clear the whole frame to a solid colour (used by the rate test).
    pub fn clear(&self, r: f32, g: f32, b: f32) {
        unsafe {
            self.gl.clear_color(r, g, b, 1.0);
            self.gl.clear(glow::COLOR_BUFFER_BIT);
        }
    }

    /// Badge aspect ratio (width/height in pixels).
    pub fn badge_aspect(&self) -> f32 {
        self.badge_aspect
    }

    /// Draw the idle badge within screen-uv rect `[x0,y0,x1,y1]`, faded by `alpha`.
    pub fn draw_idle(&self, rect: [f32; 4], alpha: f32) {
        unsafe {
            self.gl.use_program(Some(self.idle_program));
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(self.badge_tex));
            self.gl.uniform_1_i32(self.u_badge.as_ref(), 0);
            self.gl
                .uniform_4_f32(self.u_rect.as_ref(), rect[0], rect[1], rect[2], rect[3]);
            self.gl.uniform_1_f32(self.u_alpha.as_ref(), alpha);
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
    }

    /// Upload an NV12 frame: `data` = Y plane (`w*h`) then interleaved CbCr
    /// (`w*h/2`). No draw.
    pub fn upload_nv12(&mut self, w: i32, h: i32, data: &[u8]) {
        let ysz = (w * h) as usize;
        if data.len() < ysz + ysz / 2 {
            return; // partial/garbage frame
        }
        let (cw, ch) = (w / 2, h / 2);
        let y = &data[..ysz];
        let uv = &data[ysz..ysz + (cw * ch * 2) as usize];
        let realloc = self.dim != (w, h);
        unsafe {
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(self.tex_y));
            upload_plane(&self.gl, glow::LUMINANCE, w, h, y, realloc);
            self.gl.active_texture(glow::TEXTURE1);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(self.tex_uv));
            upload_plane(&self.gl, glow::LUMINANCE_ALPHA, cw, ch, uv, realloc);
        }
        self.dim = (w, h);
    }

    /// Draw the current NV12 textures as a full-screen quad.
    pub fn draw(&self) {
        unsafe {
            self.gl.use_program(Some(self.program));
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(self.tex_y));
            self.gl.uniform_1_i32(self.u_y.as_ref(), 0);
            self.gl.active_texture(glow::TEXTURE1);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(self.tex_uv));
            self.gl.uniform_1_i32(self.u_uv.as_ref(), 1);
            self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
    }
}

/// (Re)upload one texture plane (`format` = LUMINANCE or LUMINANCE_ALPHA).
unsafe fn upload_plane(
    gl: &glow::Context,
    format: u32,
    w: i32,
    h: i32,
    data: &[u8],
    realloc: bool,
) {
    if realloc {
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            format as i32,
            w,
            h,
            0,
            format,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(data)),
        );
    } else {
        gl.tex_sub_image_2d(
            glow::TEXTURE_2D,
            0,
            0,
            0,
            w,
            h,
            format,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(data)),
        );
    }
}

/// Owns the DRM device, the GBM surface, the EGL context, and the renderer.
pub struct GlPresenter {
    card: Card,
    target: Target,
    // GBM device must outlive the surface; keep it alive.
    _gbm: GbmDevice<File>,
    surface: GbmSurface<()>,
    egl: Egl,
    egl_display: egl::Display,
    egl_surface: egl::Surface,
    renderer: Renderer,
    fbs: HashMap<drm::buffer::Handle, framebuffer::Handle>,
    // The bo currently on screen (kept locked until the next flip lands; dropping
    // it releases it back to the surface pool).
    front: Option<BufferObject<()>>,
    modeset_done: bool,
    // Clock for the idle logo's slow drift.
    start: Instant,
    // Show the badge when idle (config `idle_badge`); else plain black.
    idle_badge: bool,
}

impl GlPresenter {
    pub fn open(card_path: &str, pref: Option<ModePref>, idle_badge: bool) -> Result<Self> {
        let card = Card::open(card_path)?;
        let target = kms::select_target(&card, pref)?;

        // GBM on a dup of the DRM fd (shares master).
        let gbm = GbmDevice::new(card.dup_file()?).context("create GBM device")?;
        let surface = gbm
            .create_surface::<()>(
                target.width,
                target.height,
                Format::Xrgb8888,
                BufferObjectFlags::SCANOUT | BufferObjectFlags::RENDERING,
            )
            .context("create GBM surface")?;

        // EGL on the GBM platform.
        let egl = unsafe { Egl::load_required_from_filename("libEGL.so.1") }
            .map_err(|e| anyhow!("load libEGL: {e}"))?;
        let egl_display = unsafe {
            egl.get_platform_display(
                PLATFORM_GBM_KHR,
                gbm.as_raw() as *mut c_void,
                &[egl::ATTRIB_NONE],
            )
        }
        .context("eglGetPlatformDisplay(GBM)")?;
        let (major, minor) = egl.initialize(egl_display).context("eglInitialize")?;
        tracing::info!("EGL {major}.{minor} on the GBM platform");
        egl.bind_api(egl::OPENGL_ES_API).context("eglBindAPI")?;

        // A config whose native visual is XRGB8888 (matches the GBM surface and
        // the plane's XR24 format).
        let config = pick_config(&egl, egl_display)?;
        let egl_context = egl
            .create_context(
                egl_display,
                config,
                None,
                &[egl::CONTEXT_MAJOR_VERSION, 2, egl::NONE],
            )
            .context("eglCreateContext (GLES2)")?;
        let egl_surface = unsafe {
            egl.create_window_surface(
                egl_display,
                config,
                surface.as_raw() as egl::NativeWindowType,
                None,
            )
        }
        .context("eglCreateWindowSurface(GBM surface)")?;
        egl.make_current(
            egl_display,
            Some(egl_surface),
            Some(egl_surface),
            Some(egl_context),
        )
        .context("eglMakeCurrent")?;

        // GLES via glow, loading through eglGetProcAddress.
        let gl = unsafe {
            glow::Context::from_loader_function(|s| {
                egl.get_proc_address(s)
                    .map(|f| f as *const c_void)
                    .unwrap_or(ptr::null())
            })
        };
        unsafe {
            gl.viewport(0, 0, target.width as i32, target.height as i32);
        }
        let renderer = build_renderer(gl)?;

        let mut s = Self {
            card,
            target,
            _gbm: gbm,
            surface,
            egl,
            egl_display,
            egl_surface,
            renderer,
            fbs: HashMap::new(),
            front: None,
            modeset_done: false,
            start: Instant::now(),
            idle_badge,
        };
        // Modeset immediately to the idle screen so the output is live and clean
        // (not the leftover console fb) before any stream connects.
        s.show_idle()?;
        Ok(s)
    }

    pub fn width(&self) -> u32 {
        self.target.width
    }
    pub fn height(&self) -> u32 {
        self.target.height
    }
    pub fn refresh(&self) -> u32 {
        self.target.refresh
    }

    /// Vblank-locked present loop. `render(renderer, frame)` issues the GLES draw
    /// calls for each frame; return `false` to stop. Returns frames presented.
    pub fn present_loop<F>(&mut self, mut render: F) -> Result<u64>
    where
        F: FnMut(&mut Renderer, u64) -> bool,
    {
        let mut frame = 0u64;
        loop {
            if !render(&mut self.renderer, frame) {
                break;
            }
            self.present()?;
            frame += 1;
        }
        Ok(frame)
    }

    /// Present whatever the renderer last drew: swap, lock the front bo, and
    /// either modeset (first time) or page-flip to it on the next vblank.
    fn present(&mut self) -> Result<()> {
        self.swap()?;
        let bo = self.lock_bo()?;
        let fb = self.fb_for_bo(&bo)?;
        if self.modeset_done {
            self.card
                .atomic_commit(
                    AtomicCommitFlags::PAGE_FLIP_EVENT | AtomicCommitFlags::NONBLOCK,
                    kms::flip_req(&self.target, fb),
                )
                .context("atomic page-flip commit")?;
            kms::wait_flip(&self.card)?;
        } else {
            let req = kms::modeset_req(&self.card, &self.target, fb)?;
            self.card
                .atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)
                .context("atomic modeset commit")?;
            self.modeset_done = true;
            if let Ok(Some(m)) = self.card.get_crtc(self.target.crtc).map(|i| i.mode()) {
                tracing::info!(
                    "CRTC programmed: {}x{}@{}",
                    m.size().0,
                    m.size().1,
                    m.vrefresh()
                );
            }
        }
        // Replacing the front bo drops (releases) the previously-displayed one,
        // now that the new buffer is on screen.
        self.front = Some(bo);
        Ok(())
    }

    /// Present the idle screen (black + the idle badge) — the state shown
    /// before/between streams, so the screen is never a stale decoded frame or
    /// the leftover console fb.
    pub fn show_idle(&mut self) -> Result<()> {
        if !self.idle_badge {
            // Plain black idle (config `idle_badge = false`).
            self.renderer.clear(0.0, 0.0, 0.0);
            return self.present();
        }
        let screen_aspect = self.target.width as f32 / self.target.height as f32;
        let t = self.start.elapsed().as_secs_f32();

        // Badge size: ~5.5% of screen height, width from the badge's pixel aspect
        // (kept un-stretched on this panel).
        let bh = 0.055f32;
        let bw = self.renderer.badge_aspect() * bh / screen_aspect;

        // Every 15 s it fades out, jumps to a new spot on the right side, fades in.
        const PERIOD: f32 = 15.0;
        let cycle = (t / PERIOD).floor();
        let phase = t - cycle * PERIOD;
        let alpha = smoothstep(0.0, 0.6, phase) * (1.0 - smoothstep(PERIOD - 0.6, PERIOD, phase));

        // Position varies per cycle: the badge's right edge stays near the right
        // edge; its vertical position roams (spreads burn-in).
        let x1 = 0.96 - 0.04 * hash(cycle * 1.7);
        let cy = 0.20 + 0.60 * hash(cycle * 3.1 + 0.5);
        let x0 = x1 - bw;
        let rect = [x0, cy - bh * 0.5, x1, cy + bh * 0.5];

        self.renderer.draw_idle(rect, alpha);
        self.present()
    }

    fn swap(&self) -> Result<()> {
        self.egl
            .swap_buffers(self.egl_display, self.egl_surface)
            .context("eglSwapBuffers")
    }

    fn lock_bo(&self) -> Result<BufferObject<()>> {
        unsafe { self.surface.lock_front_buffer() }.map_err(|_| anyhow!("gbm lock_front_buffer"))
    }

    /// Framebuffer for a GBM bo, created on first sight (the surface cycles a
    /// small pool of bos, so this caches ~2-3 fbs). Uses addfb2 with the bo's
    /// modifier — the buffer is tiled, so legacy addfb would be rejected.
    fn fb_for_bo(&mut self, bo: &BufferObject<()>) -> Result<framebuffer::Handle> {
        let key = drm::buffer::Buffer::handle(bo);
        if let Some(&fb) = self.fbs.get(&key) {
            return Ok(fb);
        }
        let fb = self
            .card
            .add_planar_framebuffer(bo, FbCmd2Flags::MODIFIERS)
            .context("add_planar_framebuffer(gbm bo)")?;
        self.fbs.insert(key, fb);
        Ok(fb)
    }
}

/// Choose an EGL config whose native visual id is XRGB8888.
fn pick_config(egl: &Egl, display: egl::Display) -> Result<egl::Config> {
    let attribs = [
        egl::SURFACE_TYPE,
        egl::WINDOW_BIT,
        egl::RENDERABLE_TYPE,
        egl::OPENGL_ES2_BIT,
        egl::RED_SIZE,
        8,
        egl::GREEN_SIZE,
        8,
        egl::BLUE_SIZE,
        8,
        egl::ALPHA_SIZE,
        0,
        egl::NONE,
    ];
    let mut configs = Vec::with_capacity(32);
    egl.choose_config(display, &attribs, &mut configs)
        .context("eglChooseConfig")?;
    for cfg in &configs {
        if let Ok(id) = egl.get_config_attrib(display, *cfg, egl::NATIVE_VISUAL_ID) {
            if id == GBM_FORMAT_XRGB8888 {
                return Ok(*cfg);
            }
        }
    }
    // Fall back to the first config if none advertised the visual id.
    configs
        .first()
        .copied()
        .ok_or_else(|| anyhow!("no suitable EGL config (XRGB8888)"))
}

const VERT_SRC: &str = r#"#version 100
attribute vec2 a_pos;
attribute vec2 a_uv;
varying vec2 v_uv;
void main() {
    v_uv = a_uv;
    gl_Position = vec4(a_pos, 0.0, 1.0);
}
"#;

// NV12 → RGB on the GPU. Y from a LUMINANCE texture (.r), CbCr from a
// LUMINANCE_ALPHA texture (.r = Cb, .a = Cr). BT.709 limited range — matches
// what NVENC tags for HD and what swscale produced (which looked correct).
const FRAG_SRC: &str = r#"#version 100
precision highp float;
varying vec2 v_uv;
uniform sampler2D u_y;
uniform sampler2D u_uv;
void main() {
    float y = texture2D(u_y, v_uv).r;
    float cb = texture2D(u_uv, v_uv).r - 0.5;
    float cr = texture2D(u_uv, v_uv).a - 0.5;
    float yy = 1.1643 * (y - 0.0627);
    float r = yy + 1.7927 * cr;
    float g = yy - 0.2132 * cb - 0.5329 * cr;
    float b = yy + 2.1124 * cb;
    gl_FragColor = vec4(r, g, b, 1.0);
}
"#;

// Idle screen: the prebuilt badge texture (idle mark + "Ready to connect")
// drawn within `u_rect` of the black screen, faded by `u_alpha`. The badge sits
// on black, so fading = multiplying by alpha (no blending needed).
const IDLE_FRAG_SRC: &str = r#"#version 100
precision mediump float;
varying vec2 v_uv;
uniform sampler2D u_badge;
uniform vec4 u_rect;   // x0, y0, x1, y1 in screen uv
uniform float u_alpha;
void main() {
    vec2 r = (v_uv - u_rect.xy) / (u_rect.zw - u_rect.xy);
    vec3 c = vec3(0.0);
    if (r.x >= 0.0 && r.x <= 1.0 && r.y >= 0.0 && r.y <= 1.0) {
        c = texture2D(u_badge, r).rgb;
    }
    gl_FragColor = vec4(c * u_alpha, 1.0);
}
"#;

fn build_renderer(gl: glow::Context) -> Result<Renderer> {
    unsafe {
        // Full-screen quad (TRIANGLE_STRIP): x, y, u, v. Texture row 0 (top of the
        // decoded frame) maps to the top of the screen.
        #[rustfmt::skip]
        let verts: [f32; 16] = [
            -1.0, -1.0, 0.0, 1.0,
             1.0, -1.0, 1.0, 1.0,
            -1.0,  1.0, 0.0, 0.0,
             1.0,  1.0, 1.0, 0.0,
        ];

        let program = make_program(&gl, VERT_SRC, FRAG_SRC)?;
        let idle_program = make_program(&gl, VERT_SRC, IDLE_FRAG_SRC)?;

        let vbo = gl
            .create_buffer()
            .map_err(|e| anyhow!("create_buffer: {e}"))?;
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytemuck_cast(&verts), glow::STATIC_DRAW);
        let stride = 4 * 4; // 4 floats per vertex
        gl.enable_vertex_attrib_array(0);
        gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, stride, 0);
        gl.enable_vertex_attrib_array(1);
        gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, stride, 8);

        // Planes are byte-packed (Y stride = w, CbCr stride = w); avoid the
        // default 4-byte row alignment biting odd widths.
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
        let tex_y = make_tex(&gl)?;
        let tex_uv = make_tex(&gl)?;

        // Build + upload the idle badge (idle mark + "Ready to connect").
        let badge = crate::badge::build(280);
        let badge_aspect = badge.w as f32 / badge.h as f32;
        let badge_tex = make_tex(&gl)?;
        gl.bind_texture(glow::TEXTURE_2D, Some(badge_tex));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA as i32,
            badge.w as i32,
            badge.h as i32,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(&badge.rgba)),
        );

        let u_y = gl.get_uniform_location(program, "u_y");
        let u_uv = gl.get_uniform_location(program, "u_uv");
        let u_badge = gl.get_uniform_location(idle_program, "u_badge");
        let u_rect = gl.get_uniform_location(idle_program, "u_rect");
        let u_alpha = gl.get_uniform_location(idle_program, "u_alpha");

        Ok(Renderer {
            gl,
            program,
            idle_program,
            tex_y,
            tex_uv,
            u_y,
            u_uv,
            badge_tex,
            badge_aspect,
            u_badge,
            u_rect,
            u_alpha,
            dim: (0, 0),
        })
    }
}

/// Compile + link a program from vertex/fragment sources, binding the standard
/// `a_pos`/`a_uv` attribute locations.
unsafe fn make_program(gl: &glow::Context, vs_src: &str, fs_src: &str) -> Result<glow::Program> {
    let program = gl
        .create_program()
        .map_err(|e| anyhow!("create_program: {e}"))?;
    let vs = compile(gl, glow::VERTEX_SHADER, vs_src)?;
    let fs = compile(gl, glow::FRAGMENT_SHADER, fs_src)?;
    gl.attach_shader(program, vs);
    gl.attach_shader(program, fs);
    gl.bind_attrib_location(program, 0, "a_pos");
    gl.bind_attrib_location(program, 1, "a_uv");
    gl.link_program(program);
    if !gl.get_program_link_status(program) {
        return Err(anyhow!(
            "link program: {}",
            gl.get_program_info_log(program)
        ));
    }
    gl.delete_shader(vs);
    gl.delete_shader(fs);
    Ok(program)
}

/// Create a clamped, linearly-filtered 2D texture.
unsafe fn make_tex(gl: &glow::Context) -> Result<glow::Texture> {
    let tex = gl
        .create_texture()
        .map_err(|e| anyhow!("create_texture: {e}"))?;
    gl.bind_texture(glow::TEXTURE_2D, Some(tex));
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_MIN_FILTER,
        glow::LINEAR as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_MAG_FILTER,
        glow::LINEAR as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_WRAP_S,
        glow::CLAMP_TO_EDGE as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_WRAP_T,
        glow::CLAMP_TO_EDGE as i32,
    );
    Ok(tex)
}

unsafe fn compile(gl: &glow::Context, kind: u32, src: &str) -> Result<glow::Shader> {
    let sh = gl
        .create_shader(kind)
        .map_err(|e| anyhow!("create_shader: {e}"))?;
    gl.shader_source(sh, src);
    gl.compile_shader(sh);
    if !gl.get_shader_compile_status(sh) {
        return Err(anyhow!("shader compile: {}", gl.get_shader_info_log(sh)));
    }
    Ok(sh)
}

/// Reinterpret a `&[f32]` as bytes for `buffer_data_u8_slice` (no extra dep).
fn bytemuck_cast(v: &[f32]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Cheap deterministic hash in [0,1) for per-cycle idle positions.
fn hash(n: f32) -> f32 {
    let s = (n * 12.9898).sin() * 43758.5;
    s - s.floor()
}
