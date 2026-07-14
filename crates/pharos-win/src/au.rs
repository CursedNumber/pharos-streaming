// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Annex-B access-unit splitter (HEVC / H.264 / VVC).
//!
//! ffmpeg emits a raw Annex-B elementary stream on its stdout pipe with no
//! framing. To carry one frame per `QuicStreamer::send` we cut the stream at
//! each access-unit-delimiter NAL, which the codec's `*_metadata=aud=insert`
//! bitstream filter guarantees precedes every access unit. The AUD's shape is
//! per-codec:
//!
//! * HEVC — type 35 in bits 6..1 of the header byte: `0x46`.
//! * H.264 — type 9 in the low 5 bits of the header byte: `0x09`.
//! * VVC — two-byte header; base layer `0x00`, then type 20 in the top 5 bits
//!   of the second byte: `0x00 0xA1`.
//!
//! The client concatenates the units back into a continuous stream before
//! decoding, so byte-exact alignment is not required for correctness — only for
//! one-frame-per-send pacing.
//!
//! Pure byte logic, so it builds and tests on the Linux dev box too.

/// Which codec's AUD the splitter cuts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuCodec {
    Hevc,
    H264,
    Vvc,
}

pub struct AnnexBSplitter {
    codec: AuCodec,
    buf: Vec<u8>,
}

impl AnnexBSplitter {
    pub fn new(codec: AuCodec) -> Self {
        Self {
            codec,
            buf: Vec::new(),
        }
    }

    /// Append freshly-read bytes and return any now-complete access units.
    pub fn push(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(data);
        let starts = aud_starts(self.codec, &self.buf);
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

/// Whether the bytes at `hdr..` (just past a start code) are `codec`'s AUD NAL.
fn is_aud(codec: AuCodec, buf: &[u8], hdr: usize) -> bool {
    match codec {
        AuCodec::Hevc => buf.get(hdr).is_some_and(|&b| (b >> 1) & 0x3f == 35),
        AuCodec::H264 => buf
            .get(hdr)
            .is_some_and(|&b| (b & 0x80) == 0 && (b & 0x1f) == 9),
        AuCodec::Vvc => {
            buf.get(hdr) == Some(&0x00) && buf.get(hdr + 1).is_some_and(|&b| (b >> 3) == 20)
        }
    }
}

/// Byte offsets at which an access-unit-delimiter NAL begins (pointing at the
/// first byte of its `00 00 01` start code, including a leading `00` of a 4-byte
/// start code when present).
fn aud_starts(codec: AuCodec, buf: &[u8]) -> Vec<usize> {
    let mut starts = Vec::new();
    let n = buf.len();
    if n < 4 {
        return starts;
    }
    let mut i = 0usize;
    while i + 3 < n {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            if is_aud(codec, buf, i + 3) {
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

    // A fake HEVC access unit: 4-byte AUD start code + AUD header + one payload byte.
    fn au(payload: u8) -> Vec<u8> {
        vec![0x00, 0x00, 0x00, 0x01, 0x46, 0x00, payload]
    }

    #[test]
    fn splits_complete_aus_and_keeps_tail() {
        let mut s = AnnexBSplitter::new(AuCodec::Hevc);
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
        let mut s = AnnexBSplitter::new(AuCodec::Hevc);
        // A non-AUD NAL (type 0x13 = IDR slice) alone is not a cut point.
        assert!(s.push(&[0, 0, 0, 1, 0x26, 1, 2, 3]).is_empty());
        assert!(s.flush().is_some());
    }

    #[test]
    fn splits_h264_on_its_aud() {
        // H.264 AUD header byte is 0x09; the HEVC AUD byte 0x46 must NOT cut.
        let mut s = AnnexBSplitter::new(AuCodec::H264);
        let one = vec![0x00, 0x00, 0x00, 0x01, 0x09, 0xF0, 0xaa];
        let mut stream = one.clone();
        stream.extend([0x00, 0x00, 0x01, 0x46, 0x00]); // HEVC AUD lookalike inside
        stream.extend(one.clone());
        let got = s.push(&stream);
        assert_eq!(got.len(), 1);
        assert!(got[0].starts_with(&[0, 0, 0, 1, 0x09]));
        assert!(got[0].len() > one.len(), "lookalike stayed inside the AU");
    }

    #[test]
    fn splits_vvc_on_its_two_byte_aud() {
        let mut s = AnnexBSplitter::new(AuCodec::Vvc);
        let one = vec![0x00, 0x00, 0x00, 0x01, 0x00, 0xA1, 0x50, 0xaa];
        let mut stream = one.clone();
        stream.extend(one.clone());
        let got = s.push(&stream);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], one);
        // Non-base-layer or wrong-type second byte is not a cut point.
        let mut s2 = AnnexBSplitter::new(AuCodec::Vvc);
        assert!(s2.push(&[0, 0, 0, 1, 0x00, 0x79, 0x02]).is_empty()); // SPS
        assert!(s2.push(&[0, 0, 0, 1, 0x01, 0xA1, 0x50]).is_empty()); // layer 1
    }
}
