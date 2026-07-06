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
//!
//! [`op`] is the USB/IP *setup* protocol (OP_REQ/REP_DEVLIST, OP_REQ/REP_IMPORT)
//! that precedes the URB stream on a standard TCP transport. Linux Pharos never
//! speaks it (vhci_hcd gets a pre-connected fd via sysfs), but the Windows
//! bridge must: the usbip-win2 vhci driver dials TCP itself and performs the
//! full v1.1.1 import handshake before URBs flow (issue #3).
//!
//! [`hello`] is Pharos's own exporter→importer greeting on the QUIC relay. v2
//! adds the descriptor fields OP_REP_IMPORT needs, which v1 never carried.

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

pub mod op {
    //! USB/IP setup operations (`OP_*`), protocol v1.1.1 — the request/reply
    //! exchange a standard USB/IP client performs over TCP *before* the URB
    //! stream starts (`usbip_protocol.rst` "USBIP_OP_*"). The Windows bridge
    //! implements the **server** side of this on loopback so the usbip-win2
    //! vhci driver (which dials TCP itself) can import a Pharos-relayed device.
    //!
    //! Every field is big-endian. Strings are NUL-padded fixed arrays.

    /// Protocol version tag carried in every OP header (v1.1.1).
    pub const USBIP_VERSION: u16 = 0x0111;

    pub const OP_REQ_DEVLIST: u16 = 0x8005;
    pub const OP_REP_DEVLIST: u16 = 0x0005;
    pub const OP_REQ_IMPORT: u16 = 0x8003;
    pub const OP_REP_IMPORT: u16 = 0x0003;

    /// Status codes (`op_common.status`): 0 = OK, 1 = unavailable/error.
    pub const ST_OK: u32 = 0;
    pub const ST_NA: u32 = 1;

    /// `op_common`: version u16, code u16, status u32.
    pub const OP_HEADER_LEN: usize = 8;
    /// `usbip_usb_device`: path[256] + busid[32] + 3×u32 + 3×u16 + 6×u8.
    pub const DEVICE_LEN: usize = 312;
    /// `usbip_usb_interface`: class, subclass, protocol + 1 pad byte.
    pub const INTERFACE_LEN: usize = 4;
    /// Fixed width of the `busid` (and the OP_REQ_IMPORT body).
    pub const BUSID_LEN: usize = 32;

    /// The 8-byte `op_common` header framing every OP request and reply.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct OpHeader {
        pub version: u16,
        pub code: u16,
        pub status: u32,
    }

    impl OpHeader {
        pub fn read(b: &[u8; OP_HEADER_LEN]) -> Self {
            OpHeader {
                version: u16::from_be_bytes([b[0], b[1]]),
                code: u16::from_be_bytes([b[2], b[3]]),
                status: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
            }
        }

        pub fn to_bytes(&self) -> [u8; OP_HEADER_LEN] {
            let mut b = [0u8; OP_HEADER_LEN];
            b[0..2].copy_from_slice(&self.version.to_be_bytes());
            b[2..4].copy_from_slice(&self.code.to_be_bytes());
            b[4..8].copy_from_slice(&self.status.to_be_bytes());
            b
        }
    }

    /// One interface summary as OP_REP_DEVLIST carries it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct Interface {
        pub class: u8,
        pub subclass: u8,
        pub protocol: u8,
    }

    impl Interface {
        fn write(&self, out: &mut Vec<u8>) {
            out.extend_from_slice(&[self.class, self.subclass, self.protocol, 0]);
        }
    }

    /// A device as the server side announces it: `usbip_usb_device` plus its
    /// interface list (the latter only rides OP_REP_DEVLIST; OP_REP_IMPORT
    /// carries the 312-byte device alone).
    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct ExportedDevice {
        /// Display-only sysfs-style path; clients don't dereference it.
        pub path: String,
        /// The identifier the client imports by. Must round-trip through
        /// OP_REQ_IMPORT's fixed 32-byte field, so ≤ 31 bytes.
        pub busid: String,
        pub busnum: u32,
        pub devnum: u32,
        /// USB/IP speed code (1=low 2=full 3=high 5=super 6=super+).
        pub speed: u32,
        pub id_vendor: u16,
        pub id_product: u16,
        pub bcd_device: u16,
        pub device_class: u8,
        pub device_subclass: u8,
        pub device_protocol: u8,
        pub configuration_value: u8,
        pub num_configurations: u8,
        pub interfaces: Vec<Interface>,
    }

    /// Copy `s` into a NUL-padded fixed field, truncating oversize input at a
    /// UTF-8 boundary rather than corrupting the frame.
    fn put_padded(out: &mut Vec<u8>, s: &str, len: usize) {
        let mut bytes = s.as_bytes();
        if bytes.len() >= len {
            let mut cut = len - 1; // leave room for at least one NUL
            while cut > 0 && !s.is_char_boundary(cut) {
                cut -= 1;
            }
            bytes = &bytes[..cut];
        }
        out.extend_from_slice(bytes);
        out.resize(out.len() + (len - bytes.len()), 0);
    }

    impl ExportedDevice {
        /// Append the 312-byte `usbip_usb_device` block.
        fn write_device(&self, out: &mut Vec<u8>) {
            let start = out.len();
            put_padded(out, &self.path, 256);
            put_padded(out, &self.busid, BUSID_LEN);
            out.extend_from_slice(&self.busnum.to_be_bytes());
            out.extend_from_slice(&self.devnum.to_be_bytes());
            out.extend_from_slice(&self.speed.to_be_bytes());
            out.extend_from_slice(&self.id_vendor.to_be_bytes());
            out.extend_from_slice(&self.id_product.to_be_bytes());
            out.extend_from_slice(&self.bcd_device.to_be_bytes());
            out.push(self.device_class);
            out.push(self.device_subclass);
            out.push(self.device_protocol);
            out.push(self.configuration_value);
            out.push(self.num_configurations);
            out.push(self.interfaces.len() as u8);
            debug_assert_eq!(out.len() - start, DEVICE_LEN);
        }
    }

    /// Build a success OP_REP_IMPORT: header + the 312-byte device.
    pub fn rep_import(dev: &ExportedDevice) -> Vec<u8> {
        let mut out = Vec::with_capacity(OP_HEADER_LEN + DEVICE_LEN);
        out.extend_from_slice(
            &OpHeader {
                version: USBIP_VERSION,
                code: OP_REP_IMPORT,
                status: ST_OK,
            }
            .to_bytes(),
        );
        dev.write_device(&mut out);
        out
    }

    /// Build a failure OP_REP_IMPORT — header only, non-zero status.
    pub fn rep_import_err(status: u32) -> Vec<u8> {
        OpHeader {
            version: USBIP_VERSION,
            code: OP_REP_IMPORT,
            status,
        }
        .to_bytes()
        .to_vec()
    }

    /// Build an OP_REP_DEVLIST: header, device count, then each device with its
    /// trailing interface descriptors.
    pub fn rep_devlist(devs: &[ExportedDevice]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(
            &OpHeader {
                version: USBIP_VERSION,
                code: OP_REP_DEVLIST,
                status: ST_OK,
            }
            .to_bytes(),
        );
        out.extend_from_slice(&(devs.len() as u32).to_be_bytes());
        for dev in devs {
            dev.write_device(&mut out);
            for iface in &dev.interfaces {
                iface.write(&mut out);
            }
        }
        out
    }

    /// Extract the busid string from OP_REQ_IMPORT's fixed 32-byte body.
    pub fn parse_busid(b: &[u8; BUSID_LEN]) -> String {
        let end = b.iter().position(|&c| c == 0).unwrap_or(BUSID_LEN);
        String::from_utf8_lossy(&b[..end]).into_owned()
    }

    /// Build an OP_REQ_IMPORT (client side — used in tests and loopback checks).
    pub fn req_import(busid: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(OP_HEADER_LEN + BUSID_LEN);
        out.extend_from_slice(
            &OpHeader {
                version: USBIP_VERSION,
                code: OP_REQ_IMPORT,
                status: 0,
            }
            .to_bytes(),
        );
        put_padded(&mut out, busid, BUSID_LEN);
        out
    }
}

pub mod hello {
    //! The Pharos exporter→importer greeting on the QUIC relay.
    //!
    //! v1 (legacy): `name` (u16 len + UTF-8), `devid` (u32), `speed` (u32) —
    //! exactly what a Linux vhci attach needs, nothing more.
    //!
    //! v2 prefixes a [`V2_SENTINEL`] where v1's name length lives, then appends
    //! the descriptor block the Windows bridge must echo into OP_REP_IMPORT
    //! (usbip-win2's vhci builds its PDO from those fields, so fabricating them
    //! would break enumeration).
    //!
    //! An exporter may only send v2 to an importer that advertised v2 support
    //! (via the extra SAN on the USB-relay TLS cert — see `transport`): a
    //! legacy importer reading the sentinel as a 65535-byte name length would
    //! hang consuming the descriptor block as "name". The sentinel is
    //! unambiguous to a v2 importer because real names are length-capped far
    //! below it.

    use super::op::Interface;

    /// First u16 of a v2 hello — deliberately an impossible v1 name length.
    pub const V2_SENTINEL: u16 = 0xFFFF;

    /// Longest name a well-formed hello may carry (v1 or v2). Anything larger
    /// is garbage — and keeps real name lengths unmistakable from the sentinel.
    pub const MAX_NAME_LEN: usize = 1024;

    /// The v2 descriptor block: everything OP_REP_IMPORT needs that v1 lacked.
    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct DeviceDesc {
        pub busnum: u32,
        pub devnum: u32,
        pub id_vendor: u16,
        pub id_product: u16,
        pub bcd_device: u16,
        pub device_class: u8,
        pub device_subclass: u8,
        pub device_protocol: u8,
        pub configuration_value: u8,
        pub num_configurations: u8,
        pub interfaces: Vec<Interface>,
    }

    impl DeviceDesc {
        /// Width of the fixed part; `interfaces.len() * 3` bytes follow it.
        pub const FIXED_LEN: usize = 20;

        /// Serialize: fixed block, then 3 bytes per interface.
        pub fn to_bytes(&self) -> Vec<u8> {
            let mut b = Vec::with_capacity(Self::FIXED_LEN + self.interfaces.len() * 3);
            b.extend_from_slice(&self.busnum.to_be_bytes());
            b.extend_from_slice(&self.devnum.to_be_bytes());
            b.extend_from_slice(&self.id_vendor.to_be_bytes());
            b.extend_from_slice(&self.id_product.to_be_bytes());
            b.extend_from_slice(&self.bcd_device.to_be_bytes());
            b.push(self.device_class);
            b.push(self.device_subclass);
            b.push(self.device_protocol);
            b.push(self.configuration_value);
            b.push(self.num_configurations);
            b.push(self.interfaces.len() as u8);
            for i in &self.interfaces {
                b.extend_from_slice(&[i.class, i.subclass, i.protocol]);
            }
            b
        }

        /// Parse the fixed block; returns the desc (interfaces still empty) and
        /// how many interface triples the caller must read next. Split in two
        /// because the reader can't know the trailing length until the fixed
        /// block reveals the count.
        pub fn parse_fixed(b: &[u8; Self::FIXED_LEN]) -> (Self, usize) {
            let rd32 = |o: usize| u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
            let rd16 = |o: usize| u16::from_be_bytes([b[o], b[o + 1]]);
            let n = b[19] as usize;
            (
                DeviceDesc {
                    busnum: rd32(0),
                    devnum: rd32(4),
                    id_vendor: rd16(8),
                    id_product: rd16(10),
                    bcd_device: rd16(12),
                    device_class: b[14],
                    device_subclass: b[15],
                    device_protocol: b[16],
                    configuration_value: b[17],
                    num_configurations: b[18],
                    interfaces: Vec::new(),
                },
                n,
            )
        }

        /// Fill `interfaces` from `n` triples read after the fixed block.
        pub fn parse_interfaces(&mut self, b: &[u8]) {
            self.interfaces = b
                .chunks_exact(3)
                .map(|c| Interface {
                    class: c[0],
                    subclass: c[1],
                    protocol: c[2],
                })
                .collect();
        }
    }

    /// The full greeting. `desc` is `None` on a v1 wire.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Hello {
        pub name: String,
        pub devid: u32,
        pub speed: u32,
        pub desc: Option<DeviceDesc>,
    }

    impl Hello {
        /// The legacy v1 bytes: name (u16 len + UTF-8), devid, speed. Oversize
        /// names are truncated at a char boundary to [`MAX_NAME_LEN`].
        pub fn to_bytes_v1(&self) -> Vec<u8> {
            let mut cut = self.name.len().min(MAX_NAME_LEN);
            while cut > 0 && !self.name.is_char_boundary(cut) {
                cut -= 1;
            }
            let nb = &self.name.as_bytes()[..cut];
            let mut b = Vec::with_capacity(2 + nb.len() + 8);
            b.extend_from_slice(&(nb.len() as u16).to_be_bytes());
            b.extend_from_slice(nb);
            b.extend_from_slice(&self.devid.to_be_bytes());
            b.extend_from_slice(&self.speed.to_be_bytes());
            b
        }

        /// The v2 bytes: [`V2_SENTINEL`], the v1 layout, then the descriptor
        /// block. Callers must only send this to an importer that advertised
        /// v2 support.
        pub fn to_bytes_v2(&self) -> Vec<u8> {
            let mut b = V2_SENTINEL.to_be_bytes().to_vec();
            b.extend_from_slice(&self.to_bytes_v1());
            b.extend_from_slice(&self.desc.clone().unwrap_or_default().to_bytes());
            b
        }
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
    fn op_header_roundtrips_big_endian() {
        use super::op::*;
        let h = OpHeader {
            version: USBIP_VERSION,
            code: OP_REQ_IMPORT,
            status: 0,
        };
        let b = h.to_bytes();
        assert_eq!(&b[0..2], &[0x01, 0x11]); // v1.1.1
        assert_eq!(&b[2..4], &[0x80, 0x03]); // OP_REQ_IMPORT
        assert_eq!(OpHeader::read(&b), h);
    }

    fn sample_device() -> super::op::ExportedDevice {
        use super::op::*;
        ExportedDevice {
            path: "/sys/devices/pharos/usb1/1-1".into(),
            busid: "1-1".into(),
            busnum: 1,
            devnum: 1,
            speed: 3,
            id_vendor: 0x054c,
            id_product: 0x0ce6,
            bcd_device: 0x0100,
            device_class: 0,
            device_subclass: 0,
            device_protocol: 0,
            configuration_value: 1,
            num_configurations: 1,
            interfaces: vec![
                Interface {
                    class: 3,
                    subclass: 0,
                    protocol: 0,
                },
                Interface {
                    class: 1,
                    subclass: 1,
                    protocol: 0,
                },
            ],
        }
    }

    #[test]
    fn rep_import_is_header_plus_device() {
        use super::op::*;
        let bytes = rep_import(&sample_device());
        assert_eq!(bytes.len(), OP_HEADER_LEN + DEVICE_LEN);
        let hdr = OpHeader::read(bytes[..OP_HEADER_LEN].try_into().unwrap());
        assert_eq!(hdr.code, OP_REP_IMPORT);
        assert_eq!(hdr.status, ST_OK);
        // busid lands NUL-padded right after path[256].
        assert_eq!(&bytes[OP_HEADER_LEN + 256..OP_HEADER_LEN + 256 + 3], b"1-1");
        assert_eq!(bytes[OP_HEADER_LEN + 256 + 3], 0);
        // busnum/devnum/speed at 288.., vid/pid at 300..
        assert_eq!(
            &bytes[OP_HEADER_LEN + 288..OP_HEADER_LEN + 292],
            &[0, 0, 0, 1]
        );
        assert_eq!(
            &bytes[OP_HEADER_LEN + 296..OP_HEADER_LEN + 300],
            &[0, 0, 0, 3]
        );
        assert_eq!(
            &bytes[OP_HEADER_LEN + 300..OP_HEADER_LEN + 302],
            &[0x05, 0x4c]
        );
        // interface count is the Vec length, not a stored field.
        assert_eq!(bytes[OP_HEADER_LEN + DEVICE_LEN - 1], 2);
    }

    #[test]
    fn rep_devlist_appends_interfaces() {
        use super::op::*;
        let dev = sample_device();
        let bytes = rep_devlist(std::slice::from_ref(&dev));
        assert_eq!(
            bytes.len(),
            OP_HEADER_LEN + 4 + DEVICE_LEN + 2 * INTERFACE_LEN
        );
        assert_eq!(&bytes[OP_HEADER_LEN..OP_HEADER_LEN + 4], &[0, 0, 0, 1]); // count
                                                                             // First trailing interface: HID (3,0,0) + pad.
        let if0 = OP_HEADER_LEN + 4 + DEVICE_LEN;
        assert_eq!(&bytes[if0..if0 + 4], &[3, 0, 0, 0]);
    }

    #[test]
    fn req_import_busid_roundtrips() {
        use super::op::*;
        let bytes = req_import("1-7");
        assert_eq!(bytes.len(), OP_HEADER_LEN + BUSID_LEN);
        let hdr = OpHeader::read(bytes[..OP_HEADER_LEN].try_into().unwrap());
        assert_eq!(hdr.code, OP_REQ_IMPORT);
        let busid: [u8; BUSID_LEN] = bytes[OP_HEADER_LEN..].try_into().unwrap();
        assert_eq!(parse_busid(&busid), "1-7");
    }

    #[test]
    fn oversize_and_multibyte_strings_stay_framed() {
        use super::op::*;
        // A path over 256 bytes of multi-byte UTF-8 must truncate on a char
        // boundary and still produce an exactly 312-byte device block.
        let mut dev = sample_device();
        dev.path = "é".repeat(200); // 400 bytes
        dev.busid = "b".repeat(40); // > 32
        let bytes = rep_import(&dev);
        assert_eq!(bytes.len(), OP_HEADER_LEN + DEVICE_LEN);
        // busid field still NUL-terminated within its 32 bytes.
        assert_eq!(bytes[OP_HEADER_LEN + 256 + 31], 0);
    }

    #[test]
    fn hello_v1_matches_legacy_wire() {
        use super::hello::Hello;
        let h = Hello {
            name: "Pad".into(),
            devid: 0x0001_0002,
            speed: 3,
            desc: None,
        };
        let b = h.to_bytes_v1();
        assert_eq!(&b[0..2], &[0, 3]); // name len
        assert_eq!(&b[2..5], b"Pad");
        assert_eq!(&b[5..9], &[0x00, 0x01, 0x00, 0x02]); // devid BE
        assert_eq!(&b[9..13], &[0, 0, 0, 3]); // speed BE
    }

    #[test]
    fn hello_v2_desc_roundtrips() {
        use super::hello::{DeviceDesc, Hello};
        use super::op::Interface;
        let desc = DeviceDesc {
            busnum: 1,
            devnum: 5,
            id_vendor: 0x054c,
            id_product: 0x0ce6,
            bcd_device: 0x0100,
            device_class: 0,
            device_subclass: 0,
            device_protocol: 0,
            configuration_value: 1,
            num_configurations: 1,
            interfaces: vec![Interface {
                class: 3,
                subclass: 0,
                protocol: 0,
            }],
        };
        let h = Hello {
            name: "DualSense".into(),
            devid: (1 << 16) | 5,
            speed: 3,
            desc: Some(desc.clone()),
        };
        let v2 = h.to_bytes_v2();
        // Sentinel first — an impossible v1 name length.
        assert_eq!(&v2[0..2], &[0xFF, 0xFF]);
        // The v1 layout follows the sentinel, then the descriptor block.
        let v1 = h.to_bytes_v1();
        assert_eq!(&v2[2..2 + v1.len()], &v1[..]);
        let off = 2 + v1.len();
        let fixed: [u8; DeviceDesc::FIXED_LEN] =
            v2[off..off + DeviceDesc::FIXED_LEN].try_into().unwrap();
        let (mut parsed, n) = DeviceDesc::parse_fixed(&fixed);
        assert_eq!(n, 1);
        parsed.parse_interfaces(&v2[off + DeviceDesc::FIXED_LEN..]);
        assert_eq!(parsed, desc);
        assert_eq!(v2.len(), off + DeviceDesc::FIXED_LEN + 3);
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
