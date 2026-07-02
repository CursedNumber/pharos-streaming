//! Native **VNC (RFB)** source for split-screen cells — e.g. a Proxmox VM
//! console. Connects to a VNC server, maintains the framebuffer from rectangle
//! updates, and emits a full top-down RGBA frame on each refresh. The player
//! bridges those frames into its existing NV12 render path via ffmpeg, so the
//! grid renderer is unchanged.
//!
//! **Input** is forwarded too: the same `InputEvent` channel the player feeds to
//! Pharos sources is drained inside the RFB loop and translated to RFB pointer /
//! key messages (evdev scancode → X11 keysym). So a VNC cell is interactive,
//! gated by the player's normal capture rules (focus + cursor-over-cell).

use std::net::{SocketAddr, TcpStream};

use anyhow::{Context, Result};
use core_proto::InputEvent;
use tokio::sync::mpsc::UnboundedReceiver;
use vnc::client::{AuthChoice, AuthMethod, Event};
use vnc::{Client, Encoding, PixelFormat, Rect};

/// Connect to `addr` and pump full RGBA frames (`w*h*4`, top-down) to `on_frame`
/// on every end-of-frame, until the connection drops (Err) or the desktop is
/// resized (Ok — the caller reconnects and picks up the new size). Blocking, so
/// it runs on its own thread. `password` is used only if the server requires
/// VNC auth. `input_rx`, when present, is drained each loop iteration and its
/// `InputEvent`s are translated to RFB pointer/key messages (view+control); the
/// player only sends through it when capture is active for this cell.
pub fn run(
    addr: SocketAddr,
    password: Option<&str>,
    mut input_rx: Option<UnboundedReceiver<InputEvent>>,
    mut on_frame: impl FnMut(u16, u16, &[u8]),
) -> Result<()> {
    let stream = TcpStream::connect(addr).with_context(|| format!("VNC connect {addr}"))?;
    let pw = password.map(str::to_string);
    let client = Client::from_tcp_stream(stream, false, |methods| {
        for m in methods {
            match m {
                AuthMethod::None => return Some(AuthChoice::None),
                AuthMethod::Password if pw.is_some() => {
                    let mut key = [0u8; 8];
                    for (i, b) in pw.as_deref().unwrap().bytes().take(8).enumerate() {
                        key[i] = b;
                    }
                    return Some(AuthChoice::Password(key));
                }
                _ => {}
            }
        }
        None
    });
    let mut client = client.map_err(|e| anyhow::anyhow!("VNC handshake failed: {e:?}"))?;

    let (w, h) = client.size();
    let fmt = client.format();
    anyhow::ensure!(w > 0 && h > 0, "VNC server reported a 0-sized desktop");
    // Ask for the encodings we can decode; the server falls back to Raw.
    let _ = client.set_encodings(&[Encoding::Raw, Encoding::CopyRect, Encoding::DesktopSize]);

    let mut fb = vec![0u8; w as usize * h as usize * 4];
    let full = Rect {
        left: 0,
        top: 0,
        width: w,
        height: h,
    };
    client
        .request_update(full, false)
        .map_err(|e| anyhow::anyhow!("VNC request_update: {e:?}"))?;

    // Pointer state carried across iterations: RFB sends absolute position +
    // button mask on every pointer message, so we remember the last position to
    // re-send on a button/scroll change, and the mask to re-send on a move.
    let mut pointer = PointerState::default();
    loop {
        let mut end_of_frame = false;
        for event in client.poll_iter() {
            match event {
                Event::Disconnected(e) => {
                    return match e {
                        Some(err) => Err(anyhow::anyhow!("VNC disconnected: {err:?}")),
                        None => Ok(()),
                    };
                }
                // A resize changes the framebuffer geometry; bail so the caller
                // reconnects and re-reads the size (rare; keeps the bridge simple).
                Event::Resize(_, _) => return Ok(()),
                Event::PutPixels(rect, pixels) => blit(&mut fb, w, &rect, &pixels, &fmt),
                Event::CopyPixels { src, dst } => copy_rect(&mut fb, w, &src, &dst),
                Event::EndOfFrame => end_of_frame = true,
                _ => {}
            }
        }
        if end_of_frame {
            on_frame(w, h, &fb);
        }
        // Drain any queued input and translate to RFB. A send error means the
        // link is gone — propagate so the caller reconnects.
        if let Some(rx) = input_rx.as_mut() {
            while let Ok(ev) = rx.try_recv() {
                apply_input(&mut client, ev, w, h, &mut pointer)?;
            }
        }
        client
            .request_update(full, true)
            .map_err(|e| anyhow::anyhow!("VNC request_update: {e:?}"))?;
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
}

/// Last-known pointer position (framebuffer pixels) + RFB button mask. RFB
/// `PointerEvent` is stateless on the wire — each message carries the full mask
/// and absolute position — so we track both here and re-send the pair on any
/// change.
#[derive(Default)]
struct PointerState {
    x: u16,
    y: u16,
    buttons: u8,
}

/// Translate one player `InputEvent` into RFB message(s) and send them. Pointer
/// coordinates arrive normalised 0..=1 over the source rect (the player already
/// removed letterbox/margins), so we scale to framebuffer pixels here.
fn apply_input(
    client: &mut Client,
    ev: InputEvent,
    w: u16,
    h: u16,
    p: &mut PointerState,
) -> Result<()> {
    let send_ptr =
        |c: &mut Client, p: &PointerState| c.send_pointer_event(p.buttons, p.x, p.y).map_err(rfb);
    match ev {
        InputEvent::Key {
            scancode, pressed, ..
        } => {
            if let Some(keysym) = evdev_to_keysym(scancode) {
                client.send_key_event(pressed, keysym).map_err(rfb)?;
            }
        }
        InputEvent::PointerAbs { x, y } => {
            p.x = (x.clamp(0.0, 1.0) * (w.saturating_sub(1)) as f32).round() as u16;
            p.y = (y.clamp(0.0, 1.0) * (h.saturating_sub(1)) as f32).round() as u16;
            send_ptr(client, p)?;
        }
        InputEvent::PointerRel { dx, dy } => {
            p.x = (p.x as f32 + dx)
                .clamp(0.0, w.saturating_sub(1) as f32)
                .round() as u16;
            p.y = (p.y as f32 + dy)
                .clamp(0.0, h.saturating_sub(1) as f32)
                .round() as u16;
            send_ptr(client, p)?;
        }
        InputEvent::PointerButton { button, pressed } => {
            if let Some(bit) = button_bit(button) {
                if pressed {
                    p.buttons |= bit;
                } else {
                    p.buttons &= !bit;
                }
                send_ptr(client, p)?;
            }
        }
        InputEvent::Scroll { dx, dy } => {
            // RFB models the wheel as button presses: 4=up, 5=down, 6=left,
            // 7=right (mask bits 3..=6). Pulse press+release per notch.
            for (delta, bit) in [(dy, 0x08u8), (-dy, 0x10), (-dx, 0x20), (dx, 0x40)] {
                for _ in 0..(delta.max(0.0).round() as u32).min(10) {
                    client
                        .send_pointer_event(p.buttons | bit, p.x, p.y)
                        .map_err(rfb)?;
                    send_ptr(client, p)?;
                }
            }
        }
        // Gamepad input has no RFB equivalent — the host's virtual pad is local.
        InputEvent::GamepadButton { .. }
        | InputEvent::GamepadAxis { .. }
        | InputEvent::GamepadConnect { .. }
        | InputEvent::GamepadDisconnect { .. }
        | InputEvent::GamepadMotion { .. }
        | InputEvent::ControllerTuning { .. } => {}
    }
    Ok(())
}

#[inline]
fn rfb(e: vnc::Error) -> anyhow::Error {
    anyhow::anyhow!("VNC send: {e:?}")
}

/// core_proto pointer button index (0=Left,1=Right,2=Middle,3=Back,4=Forward) →
/// RFB button-mask bit (bit0=Left, bit1=Middle, bit2=Right). Back/Forward have
/// no standard RFB bit, so they're dropped.
fn button_bit(button: u8) -> Option<u8> {
    match button {
        0 => Some(0x01), // Left
        1 => Some(0x04), // Right
        2 => Some(0x02), // Middle
        _ => None,
    }
}

/// Linux evdev key code → X11 keysym (what RFB `KeyEvent` expects). The inverse
/// of `input::keycode_to_evdev`, covering exactly the set that path emits. Base
/// (unshifted) keysyms — the server applies Shift itself from the separately
/// forwarded Shift key events.
fn evdev_to_keysym(scancode: u32) -> Option<u32> {
    Some(match scancode {
        1 => 0xff1b,                         // Escape
        2..=10 => 0x31 + (scancode - 2),     // 1..9
        11 => 0x30,                          // 0
        12 => 0x2d,                          // minus
        13 => 0x3d,                          // equal
        14 => 0xff08,                        // BackSpace
        15 => 0xff09,                        // Tab
        16 => 0x71,                          // q
        17 => 0x77,                          // w
        18 => 0x65,                          // e
        19 => 0x72,                          // r
        20 => 0x74,                          // t
        21 => 0x79,                          // y
        22 => 0x75,                          // u
        23 => 0x69,                          // i
        24 => 0x6f,                          // o
        25 => 0x70,                          // p
        26 => 0x5b,                          // bracketleft
        27 => 0x5d,                          // bracketright
        28 => 0xff0d,                        // Return
        29 => 0xffe3,                        // Control_L
        30 => 0x61,                          // a
        31 => 0x73,                          // s
        32 => 0x64,                          // d
        33 => 0x66,                          // f
        34 => 0x67,                          // g
        35 => 0x68,                          // h
        36 => 0x6a,                          // j
        37 => 0x6b,                          // k
        38 => 0x6c,                          // l
        39 => 0x3b,                          // semicolon
        40 => 0x27,                          // apostrophe
        41 => 0x60,                          // grave
        42 => 0xffe1,                        // Shift_L
        43 => 0x5c,                          // backslash
        44 => 0x7a,                          // z
        45 => 0x78,                          // x
        46 => 0x63,                          // c
        47 => 0x76,                          // v
        48 => 0x62,                          // b
        49 => 0x6e,                          // n
        50 => 0x6d,                          // m
        51 => 0x2c,                          // comma
        52 => 0x2e,                          // period
        53 => 0x2f,                          // slash
        54 => 0xffe2,                        // Shift_R
        55 => 0xffaa,                        // KP_Multiply
        56 => 0xffe9,                        // Alt_L
        57 => 0x20,                          // space
        58 => 0xffe5,                        // Caps_Lock
        59..=68 => 0xffbe + (scancode - 59), // F1..F10
        69 => 0xff7f,                        // Num_Lock
        70 => 0xff14,                        // Scroll_Lock
        71 => 0xffb7,                        // KP_7
        72 => 0xffb8,                        // KP_8
        73 => 0xffb9,                        // KP_9
        74 => 0xffad,                        // KP_Subtract
        75 => 0xffb4,                        // KP_4
        76 => 0xffb5,                        // KP_5
        77 => 0xffb6,                        // KP_6
        78 => 0xffab,                        // KP_Add
        79 => 0xffb1,                        // KP_1
        80 => 0xffb2,                        // KP_2
        81 => 0xffb3,                        // KP_3
        82 => 0xffb0,                        // KP_0
        83 => 0xffae,                        // KP_Decimal
        87 => 0xffc8,                        // F11
        88 => 0xffc9,                        // F12
        96 => 0xff8d,                        // KP_Enter
        97 => 0xffe4,                        // Control_R
        98 => 0xffaf,                        // KP_Divide
        99 => 0xff61,                        // Print
        100 => 0xffea,                       // Alt_R
        102 => 0xff50,                       // Home
        103 => 0xff52,                       // Up
        104 => 0xff55,                       // Page_Up
        105 => 0xff51,                       // Left
        106 => 0xff53,                       // Right
        107 => 0xff57,                       // End
        108 => 0xff54,                       // Down
        109 => 0xff56,                       // Page_Down
        110 => 0xff63,                       // Insert
        111 => 0xffff,                       // Delete
        119 => 0xff13,                       // Pause
        125 => 0xffeb,                       // Super_L
        126 => 0xffec,                       // Super_R
        127 => 0xff67,                       // Menu
        _ => return None,
    })
}

/// Blit a server rectangle (pixels in `fmt`) into the RGBA framebuffer. Handles
/// any true-colour `PixelFormat` via its masks/shifts and endianness.
fn blit(fb: &mut [u8], fb_w: u16, rect: &Rect, pixels: &[u8], fmt: &PixelFormat) {
    let bpp = (fmt.bits_per_pixel / 8) as usize;
    if bpp == 0 {
        return;
    }
    let (rw, rh, fbw) = (rect.width as usize, rect.height as usize, fb_w as usize);
    for ry in 0..rh {
        for rx in 0..rw {
            let i = (ry * rw + rx) * bpp;
            if i + bpp > pixels.len() {
                return;
            }
            let mut val: u32 = 0;
            if fmt.big_endian {
                for b in 0..bpp {
                    val = (val << 8) | pixels[i + b] as u32;
                }
            } else {
                for b in (0..bpp).rev() {
                    val = (val << 8) | pixels[i + b] as u32;
                }
            }
            let r = scale((val >> fmt.red_shift) & fmt.red_max as u32, fmt.red_max);
            let g = scale(
                (val >> fmt.green_shift) & fmt.green_max as u32,
                fmt.green_max,
            );
            let b = scale((val >> fmt.blue_shift) & fmt.blue_max as u32, fmt.blue_max);
            let o = ((rect.top as usize + ry) * fbw + rect.left as usize + rx) * 4;
            if o + 4 <= fb.len() {
                fb[o] = r;
                fb[o + 1] = g;
                fb[o + 2] = b;
                fb[o + 3] = 255;
            }
        }
    }
}

#[inline]
fn scale(v: u32, max: u16) -> u8 {
    if max == 0 {
        0
    } else {
        ((v * 255) / max as u32) as u8
    }
}

/// CopyRect: relocate a rectangle within the framebuffer (server-side scroll).
/// Copies via a temp buffer so overlapping src/dst is correct.
fn copy_rect(fb: &mut [u8], fb_w: u16, src: &Rect, dst: &Rect) {
    let fbw = fb_w as usize;
    let (w, h) = (
        src.width.min(dst.width) as usize,
        src.height.min(dst.height) as usize,
    );
    let mut tmp = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let so = ((src.top as usize + y) * fbw + src.left as usize + x) * 4;
            let to = (y * w + x) * 4;
            if so + 4 <= fb.len() {
                tmp[to..to + 4].copy_from_slice(&fb[so..so + 4]);
            }
        }
    }
    for y in 0..h {
        for x in 0..w {
            let to = ((dst.top as usize + y) * fbw + dst.left as usize + x) * 4;
            let so = (y * w + x) * 4;
            if to + 4 <= fb.len() {
                fb[to..to + 4].copy_from_slice(&tmp[so..so + 4]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The format almost every VNC server actually sends: 32 bpp, little-endian,
    /// 8 bits each, R at shift 16 / G at 8 / B at 0 (i.e. BGRA byte order on the
    /// wire). One known pixel must land as the right RGBA.
    fn bgra8888() -> PixelFormat {
        PixelFormat {
            bits_per_pixel: 32,
            depth: 24,
            big_endian: false,
            true_colour: true,
            red_max: 255,
            green_max: 255,
            blue_max: 255,
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
        }
    }

    fn rect(left: u16, top: u16, width: u16, height: u16) -> Rect {
        Rect {
            left,
            top,
            width,
            height,
        }
    }

    #[test]
    fn blit_bgra8888_full_frame() {
        let fmt = bgra8888();
        let mut fb = vec![0u8; 2 * 2 * 4];
        // 2×2 of one colour R=0x12 G=0x34 B=0x56, little-endian u32 BGRX bytes.
        let px = [0x56u8, 0x34, 0x12, 0x00];
        let pixels: Vec<u8> = px.iter().cycle().take(2 * 2 * 4).copied().collect();
        blit(&mut fb, 2, &rect(0, 0, 2, 2), &pixels, &fmt);
        for chunk in fb.chunks_exact(4) {
            assert_eq!(chunk, [0x12, 0x34, 0x56, 255], "RGBA from BGRA wire pixel");
        }
    }

    #[test]
    fn blit_writes_at_rect_offset() {
        // 4×4 black framebuffer; one white pixel at (col=2, row=1).
        let fmt = bgra8888();
        let mut fb = vec![0u8; 4 * 4 * 4];
        let white = [0xFFu8, 0xFF, 0xFF, 0x00]; // FF FF FF on the wire
        blit(&mut fb, 4, &rect(2, 1, 1, 1), &white, &fmt);
        let (row, col) = (1usize, 2usize);
        let o = (row * 4 + col) * 4;
        assert_eq!(&fb[o..o + 4], [0xFF, 0xFF, 0xFF, 255]);
        // A neighbour stays black (offset math didn't smear).
        let n = (row * 4 + 1) * 4;
        assert_eq!(&fb[n..n + 4], [0, 0, 0, 0]);
    }

    #[test]
    fn blit_rgb565_scales_channels() {
        // 16 bpp RGB565, little-endian. Pure red = 0xF800, pure blue = 0x001F.
        let fmt = PixelFormat {
            bits_per_pixel: 16,
            depth: 16,
            big_endian: false,
            true_colour: true,
            red_max: 31,
            green_max: 63,
            blue_max: 31,
            red_shift: 11,
            green_shift: 5,
            blue_shift: 0,
        };
        let mut fb = vec![0u8; 2 * 4];
        // pixel 0 = red (0xF800 → LE bytes 00 F8), pixel 1 = blue (0x001F → 1F 00)
        let pixels = [0x00u8, 0xF8, 0x1F, 0x00];
        blit(&mut fb, 2, &rect(0, 0, 2, 1), &pixels, &fmt);
        assert_eq!(&fb[0..4], [255, 0, 0, 255], "5-bit max red scales to 255");
        assert_eq!(&fb[4..8], [0, 0, 255, 255], "5-bit max blue scales to 255");
    }

    #[test]
    fn blit_ignores_short_pixel_buffer() {
        // A truncated rectangle payload must not panic (bounds-checked).
        let fmt = bgra8888();
        let mut fb = vec![0u8; 2 * 2 * 4];
        blit(&mut fb, 2, &rect(0, 0, 2, 2), &[0xFF, 0xFF], &fmt);
    }

    #[test]
    fn scale_handles_zero_max() {
        assert_eq!(scale(0, 0), 0);
        assert_eq!(scale(255, 255), 255);
        assert_eq!(scale(31, 31), 255);
        assert_eq!(scale(0, 255), 0);
    }

    #[test]
    fn copy_rect_relocates_block() {
        // 4×1 framebuffer (RGBA); colour the leftmost pixel, copy it right by 2.
        let mut fb = vec![0u8; 4 * 4];
        fb[0..4].copy_from_slice(&[10, 20, 30, 255]);
        copy_rect(&mut fb, 4, &rect(0, 0, 1, 1), &rect(2, 0, 1, 1));
        assert_eq!(&fb[8..12], [10, 20, 30, 255], "pixel copied to x=2");
        assert_eq!(&fb[0..4], [10, 20, 30, 255], "source still intact");
    }

    /// Exactly the evdev codes `input::keycode_to_evdev` can emit. Kept in sync
    /// so the coverage test below fails loudly if that table grows a key the VNC
    /// path can't translate.
    fn emitted_evdev_codes() -> Vec<u32> {
        let mut v: Vec<u32> = (1..=83).collect();
        v.extend([87, 88]);
        v.extend(96..=100);
        v.extend(102..=111);
        v.extend([119, 125, 126, 127]);
        v
    }

    #[test]
    fn every_emitted_evdev_code_maps_to_a_keysym() {
        for code in emitted_evdev_codes() {
            assert!(
                evdev_to_keysym(code).is_some(),
                "evdev {code} (emitted by the player) has no keysym"
            );
        }
        // A code the player never emits has no mapping.
        assert_eq!(evdev_to_keysym(200), None);
    }

    #[test]
    fn keysym_spot_checks() {
        assert_eq!(evdev_to_keysym(30), Some(0x61), "A → XK_a");
        assert_eq!(evdev_to_keysym(2), Some(0x31), "1 → XK_1");
        assert_eq!(evdev_to_keysym(11), Some(0x30), "0 → XK_0");
        assert_eq!(evdev_to_keysym(28), Some(0xff0d), "Enter → XK_Return");
        assert_eq!(evdev_to_keysym(57), Some(0x20), "Space → XK_space");
        assert_eq!(evdev_to_keysym(42), Some(0xffe1), "LShift → XK_Shift_L");
        assert_eq!(evdev_to_keysym(103), Some(0xff52), "Up → XK_Up");
        assert_eq!(evdev_to_keysym(59), Some(0xffbe), "F1 → XK_F1");
        assert_eq!(evdev_to_keysym(88), Some(0xffc9), "F12 → XK_F12");
    }

    #[test]
    fn pointer_button_bits() {
        assert_eq!(button_bit(0), Some(0x01), "Left → bit0");
        assert_eq!(button_bit(1), Some(0x04), "Right → bit2");
        assert_eq!(button_bit(2), Some(0x02), "Middle → bit1");
        assert_eq!(button_bit(3), None, "Back has no RFB bit");
        assert_eq!(button_bit(4), None, "Forward has no RFB bit");
    }
}
