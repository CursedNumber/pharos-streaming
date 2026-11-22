// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! SPICE primary-surface pixels → tightly-packed top-down RGBA.
//!
//! A SPICE `SPICE_SURFACE_FMT_32_xRGB` (and `_32_ARGB`) primary is 32 bpp,
//! little-endian, origin top-left, with in-memory byte order `[B, G, R, X]`.
//! The player's render path wants `[R, G, B, 255]` (alpha forced opaque — the
//! `x`/`a` byte is meaningless for a console). Rows may be padded: `stride` is
//! the byte step between rows and can exceed `width * 4`, so we copy row by row
//! and skip the padding, producing a tight `width * height * 4` buffer.

/// Convert a 32-bit BGRX/BGRA source surface to packed top-down RGBA in `dst`.
///
/// `src` must hold at least `stride * height` bytes (the timer reads the live
/// surface, whose length we don't independently know, so this is bounds-checked
/// against `src.len()` defensively). `dst` is resized to `width * height * 4`.
pub fn bgrx_to_rgba(src: &[u8], width: usize, height: usize, stride: usize, dst: &mut Vec<u8>) {
    dst.clear();
    dst.resize(width * height * 4, 0);
    let row_bytes = width * 4;
    for y in 0..height {
        let row_off = y * stride;
        if row_off + row_bytes > src.len() {
            break; // truncated surface — leave the rest zeroed rather than panic
        }
        let src_row = &src[row_off..row_off + row_bytes];
        let dst_row = &mut dst[y * row_bytes..(y + 1) * row_bytes];
        for (s, d) in src_row
            .as_chunks::<4>()
            .0
            .iter()
            .zip(dst_row.as_chunks_mut::<4>().0)
        {
            d[0] = s[2]; // R ← src.R (byte 2)
            d[1] = s[1]; // G ← src.G (byte 1)
            d[2] = s[0]; // B ← src.B (byte 0)
            d[3] = 255; // force opaque; src byte 3 is x/alpha, ignored
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_pixel_channel_swap() {
        // BGRX [0x10,0x20,0x30,0xFF] → RGBA [0x30,0x20,0x10,255].
        let src = [0x10u8, 0x20, 0x30, 0xFF];
        let mut dst = Vec::new();
        bgrx_to_rgba(&src, 1, 1, 4, &mut dst);
        assert_eq!(dst, [0x30, 0x20, 0x10, 255]);
    }

    #[test]
    fn alpha_is_forced_opaque() {
        // Source x/alpha byte = 0 must still produce 255.
        let src = [0x11u8, 0x22, 0x33, 0x00];
        let mut dst = Vec::new();
        bgrx_to_rgba(&src, 1, 1, 4, &mut dst);
        assert_eq!(dst[3], 255);
    }

    #[test]
    fn output_is_tightly_packed() {
        let src = vec![0u8; 4 * 3 * 2]; // 3x2, no padding
        let mut dst = Vec::new();
        bgrx_to_rgba(&src, 3, 2, 12, &mut dst);
        assert_eq!(dst.len(), 3 * 2 * 4);
    }

    #[test]
    fn stride_padding_is_skipped() {
        // 2x2 image, stride 12 (4 bytes / 1 px of padding per row). Distinct
        // colours per pixel so we can prove rows aren't smeared by the padding.
        let stride = 12;
        let mut src = vec![0u8; stride * 2];
        // row 0: px(0,0)=B,G,R 1,2,3 ; px(1,0)=4,5,6 ; then 4 padding bytes
        src[0..4].copy_from_slice(&[1, 2, 3, 0]);
        src[4..8].copy_from_slice(&[4, 5, 6, 0]);
        // row 1 starts at byte 12: px(0,1)=7,8,9 ; px(1,1)=10,11,12
        src[12..16].copy_from_slice(&[7, 8, 9, 0]);
        src[16..20].copy_from_slice(&[10, 11, 12, 0]);
        let mut dst = Vec::new();
        bgrx_to_rgba(&src, 2, 2, stride, &mut dst);
        assert_eq!(dst.len(), 16);
        assert_eq!(&dst[0..4], [3, 2, 1, 255], "px(0,0) R,G,B swapped");
        assert_eq!(&dst[4..8], [6, 5, 4, 255], "px(1,0)");
        assert_eq!(&dst[8..12], [9, 8, 7, 255], "px(0,1) — padding skipped");
        assert_eq!(&dst[12..16], [12, 11, 10, 255], "px(1,1)");
    }

    #[test]
    fn top_down_ordering_no_flip() {
        // First row red, last row blue → must land at dst[0] and dst[end].
        let (w, h) = (1usize, 2usize);
        let mut src = vec![0u8; w * 4 * h];
        src[0..4].copy_from_slice(&[0, 0, 255, 0]); // row 0 = R=255 (BGRX)
        src[4..8].copy_from_slice(&[255, 0, 0, 0]); // row 1 = B=255
        let mut dst = Vec::new();
        bgrx_to_rgba(&src, w, h, w * 4, &mut dst);
        assert_eq!(&dst[0..4], [255, 0, 0, 255], "first row stays first");
        assert_eq!(&dst[4..8], [0, 0, 255, 255], "last row stays last");
    }

    #[test]
    fn truncated_source_does_not_panic() {
        let src = [0u8; 4]; // claims 2x2 but only 1 px present
        let mut dst = Vec::new();
        bgrx_to_rgba(&src, 2, 2, 8, &mut dst);
        assert_eq!(dst.len(), 16); // still sized; tail left zeroed
    }
}
