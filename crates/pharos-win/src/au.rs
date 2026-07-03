// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Annex-B HEVC access-unit splitter.
//!
//! ffmpeg emits a raw Annex-B HEVC elementary stream on its stdout pipe with no
//! framing. To carry one frame per `QuicStreamer::send` we cut the stream at
//! each access-unit-delimiter NAL (type 35), which `hevc_metadata=aud=insert`
//! guarantees precedes every access unit. An AUD's NAL header byte is `0x46`
//! (`(35 << 1)`), so we scan for start codes (`00 00 01`, with an optional
//! leading `00` for a 4-byte start code) followed by that header byte.
//!
//! The client concatenates the units back into a continuous stream before
//! decoding, so byte-exact alignment is not required for correctness — only for
//! one-frame-per-send pacing.
//!
//! Pure byte logic, so it builds and tests on the Linux dev box too.

pub struct AnnexBSplitter {
    buf: Vec<u8>,
}

impl AnnexBSplitter {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Append freshly-read bytes and return any now-complete access units.
    pub fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(data);
        let starts = aud_starts(&self.buf);
        let mut out = Vec::new();
        if starts.len() < 2 {
            // 0 or 1 AUD seen → nothing is provably complete yet.
            return out;
        }
        // Emit [starts[i], starts[i+1]) for each complete interval; retain the
        // tail from the last AUD (the in-progress frame).
        for w in starts.windows(2) {
            out.push(self.buf[w[0]..w[1]].to_vec());
        }
        let keep_from = *starts.last().unwrap();
        self.buf.drain(..keep_from);
        out
    }

    /// Final partial access unit at EOF, if any. Unused on the P6 respawn path (a
    /// truncated tail is undecodable — the fresh ffmpeg re-sends VPS/SPS/PPS+IDR);
    /// retained for the splitter tests + a future clean-shutdown flush.
    #[allow(dead_code)]
    pub fn flush(&mut self) -> Option<Vec<u8>> {
        if self.buf.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.buf))
        }
    }
}

impl Default for AnnexBSplitter {
    fn default() -> Self {
        Self::new()
    }
}

/// Byte offsets at which an access-unit-delimiter NAL begins (pointing at the
/// first byte of its `00 00 01` start code, including a leading `00` of a 4-byte
/// start code when present).
fn aud_starts(buf: &[u8]) -> Vec<usize> {
    let mut starts = Vec::new();
    let n = buf.len();
    if n < 4 {
        return starts;
    }
    let mut i = 0usize;
    while i + 3 < n {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            let nal_type = (buf[i + 3] >> 1) & 0x3f;
            if nal_type == 35 {
                let start = if i > 0 && buf[i - 1] == 0 { i - 1 } else { i };
                starts.push(start);
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    starts
}

#[cfg(test)]
mod tests {
    use super::*;

    // A fake access unit: 4-byte AUD start code + AUD header + one payload byte.
    fn au(payload: u8) -> Vec<u8> {
        vec![0x00, 0x00, 0x00, 0x01, 0x46, 0x00, payload]
    }

    #[test]
    fn splits_complete_aus_and_keeps_tail() {
        let mut s = AnnexBSplitter::new();
        let mut stream = Vec::new();
        stream.extend(au(0xaa));
        stream.extend(au(0xbb));
        stream.extend(au(0xcc));
        // Feed two arbitrary slices to exercise cross-chunk scanning.
        let mut got = s.push(&stream[..5]);
        got.extend(s.push(&stream[5..]));
        // Two AUs are provably complete; the third is the retained tail.
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], au(0xaa));
        assert_eq!(got[1], au(0xbb));
        assert_eq!(s.flush(), Some(au(0xcc)));
    }

    #[test]
    fn no_aud_yields_nothing() {
        let mut s = AnnexBSplitter::new();
        // A non-AUD NAL (type 0x13 = IDR slice) alone is not a cut point.
        assert!(s.push(&[0, 0, 0, 1, 0x26, 1, 2, 3]).is_empty());
        assert!(s.flush().is_some());
    }
}
