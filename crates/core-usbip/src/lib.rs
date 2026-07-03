// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! USB/IP framing + device-class policy (BATTLE_PLAN §8).
//!
//! Direction is reversed vs textbook USB/IP: the CLIENT exports its local
//! device and the HOST imports it into a virtual hub. v1 classes below.
//!
//! [`proto`] is the USB/IP URB wire protocol that Linux `vhci_hcd` speaks once a
//! device is attached: the host (import side) issues `CMD_SUBMIT`, the Trireme
//! exporter services it against the real device and replies `RET_SUBMIT`. All
//! multi-byte integers are big-endian (network order); every header is 48 bytes.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbClass {
    Hid,
    Audio,
    MassStorage,
    /// Anything else — architected for, but gated off until post-v1.
    Other,
}

impl UsbClass {
    /// v1 ships HID + audio + mass storage; isochronous/webcams deferred.
    pub fn supported_v1(self) -> bool {
        matches!(
            self,
            UsbClass::Hid | UsbClass::Audio | UsbClass::MassStorage
        )
    }
}

pub mod proto {
    //! USB/IP URB protocol (kernel `Documentation/usb/usbip_protocol.rst`).

    /// Every URB header is exactly 48 bytes (basic 20 + command-specific 28).
    pub const HEADER_LEN: usize = 48;

    /// One `usbip_iso_packet_descriptor`: {offset, length, actual_length,
    /// status}, four big-endian u32 = 16 bytes. Iso URBs append
    /// `number_of_packets` of these after the data payload on both CMD_SUBMIT
    /// and RET_SUBMIT (and vhci always reads them back), so they must be carried
    /// or the byte stream desyncs.
    pub const ISO_PACKET_DESCRIPTOR_LEN: usize = 16;

    pub const USBIP_CMD_SUBMIT: u32 = 1;
    pub const USBIP_CMD_UNLINK: u32 = 2;
    pub const USBIP_RET_SUBMIT: u32 = 3;
    pub const USBIP_RET_UNLINK: u32 = 4;

    pub const USBIP_DIR_OUT: u32 = 0;
    pub const USBIP_DIR_IN: u32 = 1;

    #[inline]
    fn rd_u32(b: &[u8], o: usize) -> u32 {
        u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
    }
    #[inline]
    fn wr_u32(b: &mut [u8], o: usize, v: u32) {
        b[o..o + 4].copy_from_slice(&v.to_be_bytes());
    }

    /// One isochronous packet descriptor (`usbip_iso_packet_descriptor`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct IsoPacketDescriptor {
        pub offset: u32,
        pub length: u32,
        pub actual_length: u32,
        pub status: i32,
    }

    impl IsoPacketDescriptor {
        /// Read the 16 big-endian bytes of one descriptor.
        pub fn read(b: &[u8]) -> Self {
            Self {
                offset: rd_u32(b, 0),
                length: rd_u32(b, 4),
                actual_length: rd_u32(b, 8),
                status: rd_u32(b, 12) as i32,
            }
        }

        /// The 16 big-endian bytes of this descriptor.
        pub fn to_bytes(&self) -> [u8; ISO_PACKET_DESCRIPTOR_LEN] {
            let mut b = [0u8; ISO_PACKET_DESCRIPTOR_LEN];
            wr_u32(&mut b, 0, self.offset);
            wr_u32(&mut b, 4, self.length);
            wr_u32(&mut b, 8, self.actual_length);
            wr_u32(&mut b, 12, self.status as u32);
            b
        }
    }

    /// The 20-byte header shared by every command.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Basic {
        pub command: u32,
        pub seqnum: u32,
        pub devid: u32,
        pub direction: u32,
        pub ep: u32,
    }

    impl Basic {
        pub fn read(b: &[u8]) -> Self {
            Basic {
                command: rd_u32(b, 0),
                seqnum: rd_u32(b, 4),
                devid: rd_u32(b, 8),
                direction: rd_u32(b, 12),
                ep: rd_u32(b, 16),
            }
        }
        pub fn write(&self, b: &mut [u8]) {
            wr_u32(b, 0, self.command);
            wr_u32(b, 4, self.seqnum);
            wr_u32(b, 8, self.devid);
            wr_u32(b, 12, self.direction);
            wr_u32(b, 16, self.ep);
        }
        pub fn is_in(&self) -> bool {
            self.direction == USBIP_DIR_IN
        }
    }

    /// A parsed `CMD_SUBMIT` header. For an OUT transfer, [`out_data_len`] bytes
    /// of payload follow the header on the wire and must be read separately.
    ///
    /// [`out_data_len`]: CmdSubmit::out_data_len
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CmdSubmit {
        pub basic: Basic,
        pub transfer_flags: u32,
        pub transfer_buffer_length: i32,
        pub start_frame: i32,
        pub number_of_packets: i32,
        pub interval: i32,
        pub setup: [u8; 8],
    }

    impl CmdSubmit {
        /// Parse a 48-byte `CMD_SUBMIT` header. Returns `None` if it isn't one.
        pub fn parse(b: &[u8; HEADER_LEN]) -> Option<Self> {
            let basic = Basic::read(b);
            if basic.command != USBIP_CMD_SUBMIT {
                return None;
            }
            let mut setup = [0u8; 8];
            setup.copy_from_slice(&b[40..48]);
            Some(CmdSubmit {
                basic,
                transfer_flags: rd_u32(b, 20),
                transfer_buffer_length: rd_u32(b, 24) as i32,
                start_frame: rd_u32(b, 28) as i32,
                number_of_packets: rd_u32(b, 32) as i32,
                interval: rd_u32(b, 36) as i32,
                setup,
            })
        }

        pub fn write(&self, b: &mut [u8; HEADER_LEN]) {
            self.basic.write(b);
            wr_u32(b, 20, self.transfer_flags);
            wr_u32(b, 24, self.transfer_buffer_length as u32);
            wr_u32(b, 28, self.start_frame as u32);
            wr_u32(b, 32, self.number_of_packets as u32);
            wr_u32(b, 36, self.interval as u32);
            b[40..48].copy_from_slice(&self.setup);
        }

        /// Bytes of OUT payload that follow this header (0 for an IN transfer).
        pub fn out_data_len(&self) -> usize {
            if self.basic.direction == USBIP_DIR_OUT {
                self.transfer_buffer_length.max(0) as usize
            } else {
                0
            }
        }

        /// Whether the device is expected to return up to `transfer_buffer_length`
        /// bytes (an IN transfer).
        pub fn is_in(&self) -> bool {
            self.basic.is_in()
        }

        /// True for isochronous transfers (USB audio / UVC video).
        pub fn is_iso(&self) -> bool {
            self.number_of_packets > 0
        }

        /// Bytes of `iso_packet_descriptor` array trailing this CMD_SUBMIT
        /// (after any OUT data) — `number_of_packets * 16` for an iso URB, else
        /// 0. The exporter must consume these even when rejecting iso, or the
        /// byte stream desyncs.
        pub fn iso_descriptor_len(&self) -> usize {
            if self.is_iso() {
                self.number_of_packets.max(0) as usize * ISO_PACKET_DESCRIPTOR_LEN
            } else {
                0
            }
        }
    }

    /// A `RET_SUBMIT` reply. `data` carries the IN payload (empty for OUT).
    #[derive(Debug, Clone, Default)]
    pub struct RetSubmit {
        pub seqnum: u32,
        pub devid: u32,
        pub direction: u32,
        pub ep: u32,
        pub status: i32,
        pub actual_length: i32,
        pub start_frame: i32,
        pub number_of_packets: i32,
        pub error_count: i32,
        pub data: Vec<u8>,
        /// Per-packet results for an iso reply (empty otherwise). vhci always
        /// reads `number_of_packets` of these back, so an iso reply MUST carry
        /// them or the import stream desyncs.
        pub iso_packets: Vec<IsoPacketDescriptor>,
    }

    impl RetSubmit {
        /// Build a reply mirroring the request's routing fields.
        pub fn for_request(cmd: &CmdSubmit) -> Self {
            RetSubmit {
                seqnum: cmd.basic.seqnum,
                devid: cmd.basic.devid,
                direction: cmd.basic.direction,
                ep: cmd.basic.ep,
                ..Default::default()
            }
        }

        /// Build an iso RET_SUBMIT that fails every packet — cleanly rejects an
        /// isochronous URB without desyncing vhci (which reads
        /// `number_of_packets` descriptors back regardless of the header). Used
        /// until the libusb async/FFI iso servicing path exists.
        pub fn iso_reject(cmd: &CmdSubmit, status: i32) -> Self {
            let n = cmd.number_of_packets.max(0);
            let mut r = Self::for_request(cmd);
            r.status = status;
            r.number_of_packets = n;
            r.error_count = n;
            r.iso_packets = (0..n)
                .map(|_| IsoPacketDescriptor {
                    status,
                    ..Default::default()
                })
                .collect();
            r
        }

        /// Serialize the 48-byte header, the IN payload, then any iso descriptors.
        pub fn to_bytes(&self) -> Vec<u8> {
            let iso_len = self.iso_packets.len() * ISO_PACKET_DESCRIPTOR_LEN;
            let mut b = vec![0u8; HEADER_LEN + self.data.len() + iso_len];
            wr_u32(&mut b, 0, USBIP_RET_SUBMIT);
            wr_u32(&mut b, 4, self.seqnum);
            wr_u32(&mut b, 8, self.devid);
            wr_u32(&mut b, 12, self.direction);
            wr_u32(&mut b, 16, self.ep);
            wr_u32(&mut b, 20, self.status as u32);
            wr_u32(&mut b, 24, self.actual_length as u32);
            wr_u32(&mut b, 28, self.start_frame as u32);
            wr_u32(&mut b, 32, self.number_of_packets as u32);
            wr_u32(&mut b, 36, self.error_count as u32);
            // setup[8] at 40..48 stays zero in a reply.
            b[HEADER_LEN..HEADER_LEN + self.data.len()].copy_from_slice(&self.data);
            let mut off = HEADER_LEN + self.data.len();
            for d in &self.iso_packets {
                b[off..off + ISO_PACKET_DESCRIPTOR_LEN].copy_from_slice(&d.to_bytes());
                off += ISO_PACKET_DESCRIPTOR_LEN;
            }
            b
        }
    }

    /// A parsed `CMD_UNLINK` (host aborting an in-flight URB).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CmdUnlink {
        pub basic: Basic,
        /// The seqnum of the `CMD_SUBMIT` being cancelled.
        pub unlink_seqnum: u32,
    }

    impl CmdUnlink {
        pub fn parse(b: &[u8; HEADER_LEN]) -> Option<Self> {
            let basic = Basic::read(b);
            if basic.command != USBIP_CMD_UNLINK {
                return None;
            }
            Some(CmdUnlink {
                basic,
                unlink_seqnum: rd_u32(b, 20),
            })
        }
    }

    /// Serialize a `RET_UNLINK` (header only; remaining 24 bytes are zero).
    pub fn ret_unlink(unlink: &CmdUnlink, status: i32) -> Vec<u8> {
        let mut b = vec![0u8; HEADER_LEN];
        let basic = Basic {
            command: USBIP_RET_UNLINK,
            seqnum: unlink.basic.seqnum,
            devid: unlink.basic.devid,
            direction: unlink.basic.direction,
            ep: unlink.basic.ep,
        };
        basic.write(&mut b);
        wr_u32(&mut b, 20, status as u32);
        b
    }

    /// Peek the command field of a header without fully parsing it.
    pub fn command_of(b: &[u8; HEADER_LEN]) -> u32 {
        rd_u32(b, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::proto::*;

    #[test]
    fn header_is_big_endian() {
        // A control IN GET_DESCRIPTOR(device) submit, ep0, seqnum 1.
        let cmd = CmdSubmit {
            basic: Basic {
                command: USBIP_CMD_SUBMIT,
                seqnum: 1,
                devid: 0x0001_0002,
                direction: USBIP_DIR_IN,
                ep: 0,
            },
            transfer_flags: 0,
            transfer_buffer_length: 18,
            start_frame: 0,
            number_of_packets: 0,
            interval: 0,
            setup: [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x12, 0x00],
        };
        let mut buf = [0u8; HEADER_LEN];
        cmd.write(&mut buf);
        // command=1 big-endian.
        assert_eq!(&buf[0..4], &[0, 0, 0, 1]);
        // devid 0x00010002 big-endian.
        assert_eq!(&buf[8..12], &[0x00, 0x01, 0x00, 0x02]);
        // direction IN=1.
        assert_eq!(&buf[12..16], &[0, 0, 0, 1]);
        // transfer_buffer_length 18.
        assert_eq!(&buf[24..28], &[0, 0, 0, 18]);
        // setup packet preserved at 40..48.
        assert_eq!(&buf[40..48], &cmd.setup);
    }

    #[test]
    fn cmd_submit_roundtrip() {
        let cmd = CmdSubmit {
            basic: Basic {
                command: USBIP_CMD_SUBMIT,
                seqnum: 42,
                devid: 7,
                direction: USBIP_DIR_OUT,
                ep: 2,
            },
            transfer_flags: 0,
            transfer_buffer_length: 512,
            start_frame: 0,
            number_of_packets: 0,
            interval: 0,
            setup: [0; 8],
        };
        let mut buf = [0u8; HEADER_LEN];
        cmd.write(&mut buf);
        let back = CmdSubmit::parse(&buf).expect("parse");
        assert_eq!(back, cmd);
        assert_eq!(back.out_data_len(), 512); // OUT → payload follows
        assert!(!back.is_in());
        assert!(!back.is_iso());
    }

    #[test]
    fn ret_submit_carries_in_data() {
        let cmd = CmdSubmit {
            basic: Basic {
                command: USBIP_CMD_SUBMIT,
                seqnum: 9,
                devid: 3,
                direction: USBIP_DIR_IN,
                ep: 0,
            },
            transfer_flags: 0,
            transfer_buffer_length: 4,
            start_frame: 0,
            number_of_packets: 0,
            interval: 0,
            setup: [0; 8],
        };
        let mut ret = RetSubmit::for_request(&cmd);
        ret.actual_length = 4;
        ret.data = vec![0xde, 0xad, 0xbe, 0xef];
        let bytes = ret.to_bytes();
        assert_eq!(bytes.len(), HEADER_LEN + 4);
        assert_eq!(
            command_of(bytes[..HEADER_LEN].try_into().unwrap()),
            USBIP_RET_SUBMIT
        );
        assert_eq!(&bytes[4..8], &[0, 0, 0, 9]); // seqnum echoed
        assert_eq!(&bytes[24..28], &[0, 0, 0, 4]); // actual_length
        assert_eq!(&bytes[HEADER_LEN..], &[0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn iso_descriptor_len_accounts_for_packets() {
        let mut cmd = CmdSubmit {
            basic: Basic {
                command: USBIP_CMD_SUBMIT,
                seqnum: 1,
                devid: 1,
                direction: USBIP_DIR_IN,
                ep: 1,
            },
            transfer_flags: 0,
            transfer_buffer_length: 192,
            start_frame: 0,
            number_of_packets: 4,
            interval: 1,
            setup: [0; 8],
        };
        assert!(cmd.is_iso());
        assert_eq!(cmd.iso_descriptor_len(), 4 * ISO_PACKET_DESCRIPTOR_LEN);
        cmd.number_of_packets = 0;
        assert!(!cmd.is_iso());
        assert_eq!(cmd.iso_descriptor_len(), 0);
    }

    #[test]
    fn iso_descriptor_roundtrips() {
        let d = IsoPacketDescriptor {
            offset: 10,
            length: 20,
            actual_length: 18,
            status: -71,
        };
        assert_eq!(IsoPacketDescriptor::read(&d.to_bytes()), d);
    }

    #[test]
    fn iso_reject_emits_descriptor_array() {
        let cmd = CmdSubmit {
            basic: Basic {
                command: USBIP_CMD_SUBMIT,
                seqnum: 5,
                devid: 1,
                direction: USBIP_DIR_IN,
                ep: 1,
            },
            transfer_flags: 0,
            transfer_buffer_length: 96,
            start_frame: 0,
            number_of_packets: 3,
            interval: 1,
            setup: [0; 8],
        };
        let ret = RetSubmit::iso_reject(&cmd, -22);
        assert_eq!(ret.iso_packets.len(), 3);
        let bytes = ret.to_bytes();
        // header + no IN data + 3 descriptors — what vhci expects to read back.
        assert_eq!(bytes.len(), HEADER_LEN + 3 * ISO_PACKET_DESCRIPTOR_LEN);
        assert_eq!(&bytes[32..36], &[0, 0, 0, 3]); // number_of_packets
        assert_eq!(&bytes[36..40], &[0, 0, 0, 3]); // error_count
                                                   // First descriptor's status field = -22 big-endian.
        assert_eq!(
            &bytes[HEADER_LEN + 12..HEADER_LEN + 16],
            &(-22i32 as u32).to_be_bytes()
        );
    }

    #[test]
    fn unlink_parse_and_reply() {
        let mut buf = [0u8; HEADER_LEN];
        Basic {
            command: USBIP_CMD_UNLINK,
            seqnum: 100,
            devid: 1,
            direction: 0,
            ep: 0,
        }
        .write(&mut buf);
        buf[20..24].copy_from_slice(&55u32.to_be_bytes()); // unlink seqnum 55
        let u = CmdUnlink::parse(&buf).expect("unlink");
        assert_eq!(u.unlink_seqnum, 55);
        let reply = ret_unlink(&u, 0);
        assert_eq!(command_of(reply[..].try_into().unwrap()), USBIP_RET_UNLINK);
        assert_eq!(&reply[4..8], &[0, 0, 0, 100]); // seqnum echoed
    }
}
