// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Clipboard sync between the host and a Trireme client.
//!
//! Carried on a dedicated bidirectional QUIC stream — the **host opens** it as the
//! second server-initiated bidi, right after the app relay (a direction the client
//! only accepts, so it never collides with the input stream's ID space). The client
//! demuxes by accept order: 1st server-bidi = app relay, 2nd = clipboard. The host
//! nudges the stream open with a zero-length frame so the client accepts it even
//! with an idle clipboard. Both ends speak [`ClipboardMsg`], framed as
//! `u32 BE length || postcard bytes` — the same framing as input events and the
//! app relay (a `len == 0` frame is the nudge and is skipped).
//!
//! - host → client: [`ClipboardMsg::Text`] — a host Wayland app set the CLIPBOARD
//!   selection; the client mirrors it into its OS clipboard.
//! - client → host: [`ClipboardMsg::Text`] — the client's OS clipboard changed;
//!   the host becomes the Wayland selection owner so host apps can paste it.
//!
//! v1 is UTF-8 text only. The enum is left open for a future `Mime { mime, data }`
//! flavor (images/files); postcard is positional, so append new variants at the
//! END and rebuild both ends in lockstep.

use serde::{Deserialize, Serialize};

use crate::WireError;

/// Defensive cap on the clipboard **text** payload: the host truncates a selection
/// read to this, and senders cap their outbound text to it. Text clipboards are
/// tiny; this bounds a hostile or accidental huge paste (1 MiB ≈ a large code file)
/// without a real-world limit. Longer text is truncated, not dropped.
pub const MAX_CLIPBOARD_BYTES: usize = 1 << 20; // 1 MiB

/// Cap on a whole relay **frame** (`len` prefix value). A `ClipboardMsg::Text` of
/// `MAX_CLIPBOARD_BYTES` postcard-encodes to slightly MORE than that (enum
/// discriminant + a varint length prefix), so the frame cap must exceed the text
/// cap or a max-size clipboard would trip the reader and drop the whole relay
/// stream. 64 bytes is ample headroom for the postcard envelope while still
/// bounding the receiver's per-frame allocation. Readers reject frames above this.
pub const MAX_CLIPBOARD_FRAME: usize = MAX_CLIPBOARD_BYTES + 64;

/// A message on the clipboard relay stream. Flows both directions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardMsg {
    /// UTF-8 clipboard text. Empty string = the owner cleared its clipboard.
    Text(String),
}

/// Encode a [`ClipboardMsg`] (postcard).
pub fn encode_clipboard(msg: &ClipboardMsg) -> Result<Vec<u8>, WireError> {
    postcard::to_stdvec(msg).map_err(|e| WireError::Encode(e.to_string()))
}

/// Decode a [`ClipboardMsg`] (postcard).
pub fn decode_clipboard(bytes: &[u8]) -> Result<ClipboardMsg, WireError> {
    postcard::from_bytes(bytes).map_err(|e| WireError::Decode(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_roundtrip() {
        let msg = ClipboardMsg::Text("hello, clipboard 📋".into());
        assert_eq!(
            decode_clipboard(&encode_clipboard(&msg).unwrap()).unwrap(),
            msg
        );
    }

    #[test]
    fn empty_text_roundtrip() {
        let msg = ClipboardMsg::Text(String::new());
        assert_eq!(
            decode_clipboard(&encode_clipboard(&msg).unwrap()).unwrap(),
            msg
        );
    }

    #[test]
    fn max_text_frame_fits_frame_cap_and_roundtrips() {
        // A max-size text must encode to a frame within MAX_CLIPBOARD_FRAME — else a
        // 1 MiB clipboard would trip the relay reader and drop the whole stream.
        let msg = ClipboardMsg::Text("x".repeat(MAX_CLIPBOARD_BYTES));
        let frame = encode_clipboard(&msg).unwrap();
        assert!(
            frame.len() > MAX_CLIPBOARD_BYTES && frame.len() <= MAX_CLIPBOARD_FRAME,
            "frame {} must exceed the text cap (envelope) but fit the frame cap {}",
            frame.len(),
            MAX_CLIPBOARD_FRAME
        );
        assert_eq!(decode_clipboard(&frame).unwrap(), msg);
    }
}
