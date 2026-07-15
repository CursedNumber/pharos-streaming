// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! QUIC transport (quinn) for Pharos → Trireme.
//!
//! The host streams encoded video access units (codec-selectable
//! HEVC/AV1/AVC/VVC) over a unidirectional QUIC stream; the client reassembles
//! them. The host↔compositor wiring (an mpsc of encoded frames) and the Android
//! UniFFI client are both in place.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use quinn::crypto::rustls::QuicClientConfig;
use quinn::{ClientConfig, Endpoint, ServerConfig};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use tokio::sync::mpsc;

/// The single deliberate Pharos connect/stream port — the one port a user ever
/// sees, enters, or port-forwards. Chosen mid-range (≈ the 4433↔27000 midpoint),
/// in a quiet IANA gap, clear of QUIC-dev 4433, Steam 27015+, and Sunshine
/// 47984+. Pairing (4432) + USB/IP import (4434) stay on their own hidden ports.
pub const PHAROS_STREAM_PORT: u16 = 15777;

/// Sibling ports an auto-failover bind must never steal (pairing + USB/IP import).
const RESERVED_PORTS: &[u16] = &[4432, 4434];

/// QUIC application close code the host sends when a NEW client PRE-EMPTS the
/// current one (single streaming slot). Distinct from the plain `0` used by the
/// done/auth closes so the *losing* client can tell "another device took the
/// slot" apart from an ordinary drop and YIELD (stop auto-redialing) instead of
/// instantly reconnecting — otherwise two connected clients ping-pong forever,
/// each pre-empting the other. The client matches this exact code; the reason
/// bytes are advisory.
pub const PREEMPT_CLOSE_CODE: u32 = 0x9;

/// QUIC application close code the host sends to a *newcomer* it refuses
/// because the persisted PRIMARY client currently holds the single streaming
/// slot (issue #19). Distinct from [`PREEMPT_CLOSE_CODE`] — here nobody was
/// pre-empted; the challenger was turned away — so the client can show honest
/// copy ("host reserved by primary client") and back off instead of hammering
/// redials that will keep being refused. 0xA/0xB are taken (mirror capacity /
/// mode switch).
pub const RESERVED_CLOSE_CODE: u32 = 0xC;

/// QUIC application close code the host sends when the operator FORGETS
/// (unpairs) the connected peer from the panel (issue #19). Every future
/// connect will fail authorization anyway, so the client should stop
/// auto-redialing entirely and tell the user to re-pair.
pub const UNPAIRED_CLOSE_CODE: u32 = 0xD;

/// Stable error text `run_video_client*` fails with when the host refused the
/// connection with [`RESERVED_CLOSE_CODE`]. Exported so a client frontend can
/// match it (the error is `anyhow`, not typed) and render honest UI copy.
pub const RESERVED_BY_PRIMARY_MSG: &str = "host reserved by primary client";

/// Stable error text `run_video_client*` fails with when the host closed the
/// session with [`UNPAIRED_CLOSE_CODE`] (the operator forgot this device).
/// Exported so a client frontend can match it and stop redialing.
pub const UNPAIRED_MSG: &str = "unpaired by host; pair again to reconnect";

/// Scan upward from `base`+1 for a free UDP port to host a QUIC server on,
/// skipping [`RESERVED_PORTS`]. Last-resort when the configured port is durably
/// taken by another service — better than failing the output or hanging forever.
fn bind_free_from(base: SocketAddr) -> Option<(Endpoint, SocketAddr)> {
    let mut p = base.port();
    for _ in 0..64 {
        p = p.wrapping_add(1);
        if p == 0 || RESERVED_PORTS.contains(&p) {
            continue;
        }
        let cand = SocketAddr::new(base.ip(), p);
        if let Ok(cfg) = server_config() {
            if let Ok(ep) = Endpoint::server(cfg, cand) {
                return Some((ep, cand));
            }
        }
    }
    None
}

/// A self-signed server config + the cert (for loopback / pinning).
fn server_config() -> Result<ServerConfig> {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])?;
    let cert_der = CertificateDer::from(cert.cert.der().to_vec());
    let key = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let mut cfg = ServerConfig::with_single_cert(vec![cert_der], key.into())?;
    cfg.transport_config(transport_config()?);
    Ok(cfg)
}

/// Build a **dual-stack** QUIC server endpoint bound to `[::]:port` with
/// `IPV6_V6ONLY` off, so one socket accepts both IPv6 and IPv4 (v4-mapped) clients.
/// Used when `[network] ipv6 = true`. On any error the caller falls back to the
/// plain IPv4 `Endpoint::server` bind.
fn dualstack_server_endpoint(server_config: ServerConfig, port: u16) -> Result<Endpoint> {
    use socket2::{Domain, Protocol, Socket, Type};
    let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_only_v6(false)?; // dual-stack: also accept IPv4 as v4-mapped
    let bind: SocketAddr = (Ipv6Addr::UNSPECIFIED, port).into();
    sock.bind(&bind.into())?;
    let udp: std::net::UdpSocket = sock.into();
    let runtime = quinn::default_runtime()
        .ok_or_else(|| anyhow::anyhow!("no compatible async runtime for quinn"))?;
    Ok(Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(server_config),
        udp,
        runtime,
    )?)
}

/// Build the client endpoint. When `dual_stack`, bind `[::]:0` (V6ONLY off) so a
/// single socket can dial both families (IPv6 natively, IPv4 via v4-mapped — see
/// [`to_dialable`]); on failure it falls back to the IPv4 wildcard. When not
/// `dual_stack`, it's the proven `0.0.0.0:0` bind unchanged.
fn client_endpoint(dual_stack: bool) -> Result<Endpoint> {
    if dual_stack {
        use socket2::{Domain, Protocol, Socket, Type};
        let build = || -> Result<Endpoint> {
            let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
            sock.set_only_v6(false)?;
            let bind: SocketAddr = (Ipv6Addr::UNSPECIFIED, 0).into();
            sock.bind(&bind.into())?;
            let udp: std::net::UdpSocket = sock.into();
            let runtime = quinn::default_runtime()
                .ok_or_else(|| anyhow::anyhow!("no compatible async runtime for quinn"))?;
            Ok(Endpoint::new(
                quinn::EndpointConfig::default(),
                None,
                udp,
                runtime,
            )?)
        };
        match build() {
            Ok(ep) => return Ok(ep),
            Err(e) => tracing::warn!("dual-stack client socket failed ({e}); using IPv4"),
        }
    }
    Ok(Endpoint::client((Ipv4Addr::UNSPECIFIED, 0).into())?)
}

/// Map an IPv4 target to its v4-mapped IPv6 form so a dual-stack client socket can
/// reach it (a v6 socket can only `sendto` v6 addresses); IPv6 targets and the
/// non-dual-stack case pass through unchanged.
fn to_dialable(addr: SocketAddr, dual_stack: bool) -> SocketAddr {
    match addr {
        SocketAddr::V4(v4) if dual_stack => {
            SocketAddr::V6(SocketAddrV6::new(v4.ip().to_ipv6_mapped(), v4.port(), 0, 0))
        }
        other => other,
    }
}

/// Shared QUIC transport tuning for both ends.
///
/// **Idle timeout = 20s.** QUIC identifies a connection by its connection ID, not
/// the UDP 4-tuple, so when a peer's address changes — a client roaming
/// WiFi↔Ethernet, a DHCP renew, NAT rebinding, or the dual-homed `.201` host —
/// the connection *migrates* to the new path instead of dropping, **as long as the
/// gap stays under the idle timeout**. 5s (the old value) was too tight to ride
/// out a NIC switch (link-down + DHCP can take several seconds); 20s gives margin.
/// The client binds a wildcard socket (`0.0.0.0:0`) so the OS silently moves egress
/// to the new interface on a route change — no app action needed — and the server
/// validates the new path (quinn allows address migration by default).
///
/// Trade-off: an *ungracefully* dropped client (crash / cable yank, no close) is
/// now detected in ~20s rather than ~5s, so the multi-shot accept loop frees up for
/// the next client that much later. Keep-alive pings (2s) refresh the idle timer
/// during quiet video so a stalled-but-alive peer isn't reaped.
fn transport_config() -> Result<Arc<quinn::TransportConfig>> {
    let mut t = quinn::TransportConfig::default();
    t.max_idle_timeout(Some(quinn::IdleTimeout::try_from(
        std::time::Duration::from_secs(20),
    )?));
    t.keep_alive_interval(Some(std::time::Duration::from_secs(2)));
    // Bound the QUIC flow-control windows. An unbounded (or oversized) receive
    // window lets the sender buffer seconds of encoded video before flow-control
    // pushes back — a fixed *byte* buffer, so a LOWER bitrate makes the latency
    // WORSE in seconds (window/bitrate). But a fixed SMALL window strangles a
    // high-bitrate stream instead: a single 4K IDR can exceed the whole window,
    // serializing every keyframe over multiple credit round-trips (the
    // `write_all slow` stalls seen live at 45-60 Mbps against the old 256KB
    // default, which was tuned for a 10 Mbps 1080p couch stream).
    //
    // So the split is: the per-stream window and the sender's send-window are
    // plain MEMORY CAPS set high enough for any stream this project encodes
    // (the hosts clamp at 60 Mbps; 8MB ≈ 1s at that ceiling), while the
    // LATENCY bound lives at the receiver, which resizes the connection-level
    // receive window at runtime to ~250ms of its MEASURED video rate (see the
    // QoS tick in `run_video_client_with_input`) — the one place the true rate
    // is known. `PHAROS_STREAM_WINDOW_KB` remains as a pin: when set, all three
    // windows take that fixed value and the runtime adaptation stays off
    // (compat with the documented SBC tuning recipes).
    // Congestion controller. quinn's default (Cubic) is LOSS-BASED: it halves
    // its window on packet loss, which reads stochastic WiFi loss as
    // congestion — at the 2-4% bursty loss a contended WiFi hop shows under
    // load, Cubic's sustainable rate collapses to ~1-2 Mbps regardless of real
    // capacity, which is BELOW the 4K60 video floor → the send queue overflows
    // at any encoder target (seen live 2026-07-08). BBR is rate-based (models
    // delivery rate + min-RTT instead of reacting to individual losses) and is
    // the right controller for real-time video on lossy links, so per the
    // project's WiFi-first principle it is the DEFAULT; `PHAROS_CC` reverts
    // (`cubic` | `newreno`) for A/B or if BBR misbehaves on some path.
    match std::env::var("PHAROS_CC")
        .ok()
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("cubic") => {
            t.congestion_controller_factory(Arc::new(quinn::congestion::CubicConfig::default()));
            tracing::info!("QUIC congestion control: Cubic (PHAROS_CC)");
        }
        Some("newreno") => {
            t.congestion_controller_factory(Arc::new(quinn::congestion::NewRenoConfig::default()));
            tracing::info!("QUIC congestion control: NewReno (PHAROS_CC)");
        }
        _ => {
            t.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
            tracing::info!(
                "QUIC congestion control: BBR (rate-based, loss-tolerant; \
                 PHAROS_CC=cubic reverts)"
            );
        }
    }
    match stream_window_pin() {
        Some(win_kb) => {
            let win_bytes = win_kb.saturating_mul(1024);
            let win = quinn::VarInt::from_u32(win_bytes);
            t.stream_receive_window(win);
            t.receive_window(win);
            t.send_window(u64::from(win_bytes));
            tracing::info!(
                "QUIC flow-control window: pinned {win_kb} KB (PHAROS_STREAM_WINDOW_KB; \
                 adaptive sizing off)"
            );
        }
        None => {
            t.stream_receive_window(quinn::VarInt::from_u32(WINDOW_CAP_BYTES));
            t.receive_window(quinn::VarInt::from_u32(WINDOW_START_BYTES));
            t.send_window(u64::from(WINDOW_CAP_BYTES));
            tracing::info!(
                "QUIC flow-control window: adaptive ({}KB start, {}KB cap, ~250ms at the \
                 measured rate; PHAROS_STREAM_WINDOW_KB pins a fixed size)",
                WINDOW_START_BYTES / 1024,
                WINDOW_CAP_BYTES / 1024,
            );
        }
    }
    Ok(Arc::new(t))
}

/// The `PHAROS_STREAM_WINDOW_KB` pin, if set: a fixed size for all three QUIC
/// flow-control windows AND the off-switch for the receiver's runtime window
/// adaptation. Kept for the documented SBC recipes (e.g. the Pi couch stream
/// pins 128KB) and for A/B experiments.
fn stream_window_pin() -> Option<u32> {
    std::env::var("PHAROS_STREAM_WINDOW_KB")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&k: &u32| k > 0)
}

/// Memory cap for the per-stream receive window and the send window, and the
/// upper clamp for the adaptive connection receive window: ~1s at the hosts'
/// 60 Mbps encode ceiling. Never the operative latency bound — that's the
/// adaptive connection window below it.
const WINDOW_CAP_BYTES: u32 = 8 * 1024 * 1024;
/// Connection receive window before the first rate measurement lands: ~250ms
/// at the 60 Mbps encode ceiling, so a full-rate stream starts unthrottled.
const WINDOW_START_BYTES: u32 = 2 * 1024 * 1024;
/// Lower clamp for the adaptive window (an idle or heavily-braked stream
/// decays to this). This is a LATENCY floor, so it must stay SMALL: the
/// window is a byte budget, and at a braked 2 Mbps a 512KB floor is TWO
/// SECONDS of standing buffer — measured live 2026-07-09 as ~5s of pointer
/// lag while the picture flowed at 52fps (every frame seconds stale). 64KB
/// is ~250ms at 2 Mbps, and it cannot throughput-throttle a burst for more
/// than the ≤1s until the next resize (64KB per ~10ms LAN RTT ≈ 50 Mbps).
const WINDOW_FLOOR_BYTES: u32 = 64 * 1024;

/// The adaptive receive-window target: ~250ms of buffer at the measured video
/// rate, but never smaller than 3× the largest recent AU, clamped to
/// [floor, cap]. The AU term breaks the floor-rate IDR limit cycle found live
/// 2026-07-09: at a floor-braked 2 Mbps the 250ms budget (~64KB) is about ONE
/// 4K IDR, so every forced IDR filled the whole window, blocked the sender,
/// shed a few frames from the host queue — and the shed requested the next
/// forced IDR a second later, forever (a clean 1-2ms wire showing single-digit
/// qdrops every second, controller parked at the floor). Sizing for the real
/// AUs in flight keeps keyframes swallowable at every rate; at high rates the
/// rate budget dominates (60 Mbps: ~1.9MB budget vs ~1.5MB for 3× a VBV-capped
/// 500KB IDR). Pure for unit tests.
fn adaptive_window_target(rate_bytes_per_sec: f32, max_au_bytes: u32) -> u32 {
    ((rate_bytes_per_sec / 4.0) as u32)
        .max(max_au_bytes.saturating_mul(3))
        .clamp(WINDOW_FLOOR_BYTES, WINDOW_CAP_BYTES)
}

/// Insecure verifier — accepts any server cert. Loopback/testing only; the real
/// client pins Pharos's long-term identity (see core-crypto pairing).
#[derive(Debug)]
struct SkipServerVerification(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn client_config() -> Result<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let crypto = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(rustls::DEFAULT_VERSIONS)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification(provider)))
        .with_no_client_auth();
    let mut cfg = ClientConfig::new(Arc::new(QuicClientConfig::try_from(crypto)?));
    // Same idle/keep-alive as the server so the negotiated timeout is long enough
    // for the client to survive its own NIC/IP changes (connection migration).
    cfg.transport_config(transport_config()?);
    Ok(cfg)
}

/// Marker SAN a v2-capable importer adds to its USB-relay TLS cert, so an
/// exporter knows — before sending a byte — whether the v2 hello (descriptor
/// block for the Windows import bridge, `core_usbip::hello`) is understood.
///
/// Why a cert SAN and not ALPN: rustls *rejects* a client that offers no ALPN
/// once the server configures any (`NoApplicationProtocol`), so ALPN would cut
/// off every legacy exporter. The SAN is invisible to legacy peers instead:
/// old exporters skip cert verification entirely and old importers simply
/// don't carry the marker, so both downgrade quadrants stay v1 with no
/// handshake risk. `.invalid` is the RFC 2606 reserved TLD — never a real host.
const USB_V2_SAN: &str = "usb-v2.pharos.invalid";

/// [`server_config`] with the [`USB_V2_SAN`] capability marker added to the
/// self-signed cert. Only the USB import endpoint uses this.
fn usb_server_config() -> Result<ServerConfig> {
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), USB_V2_SAN.to_string()])?;
    let cert_der = CertificateDer::from(cert.cert.der().to_vec());
    let key = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let mut cfg = ServerConfig::with_single_cert(vec![cert_der], key.into())?;
    cfg.transport_config(transport_config()?);
    Ok(cfg)
}

/// [`SkipServerVerification`] that additionally sniffs the server cert for the
/// [`USB_V2_SAN`] capability marker. A SAN dNSName is embedded verbatim as
/// ASCII in the DER, so a byte search is sufficient — and the tag is
/// collision-proof in practice (reserved TLD, exact match). The flag is set
/// during the TLS handshake, i.e. before `connect()` resolves.
#[derive(Debug)]
struct UsbV2Detect {
    inner: SkipServerVerification,
    saw_v2: Arc<AtomicBool>,
}

impl rustls::client::danger::ServerCertVerifier for UsbV2Detect {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let der = end_entity.as_ref();
        let tag = USB_V2_SAN.as_bytes();
        if der.windows(tag.len()).any(|w| w == tag) {
            self.saw_v2.store(true, Ordering::Release);
        }
        self.inner
            .verify_server_cert(end_entity, intermediates, server_name, ocsp, now)
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// [`client_config`] with the v2-marker sniffing verifier; `saw_v2` is set iff
/// the server's cert carries [`USB_V2_SAN`].
fn usb_client_config(saw_v2: Arc<AtomicBool>) -> Result<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let crypto = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(rustls::DEFAULT_VERSIONS)?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(UsbV2Detect {
            inner: SkipServerVerification(provider),
            saw_v2,
        }))
        .with_no_client_auth();
    let mut cfg = ClientConfig::new(Arc::new(QuicClientConfig::try_from(crypto)?));
    cfg.transport_config(transport_config()?);
    Ok(cfg)
}

/// Split a `host:port` (or `[v6]:port`) string into its host and port parts.
fn split_host_port(s: &str) -> Option<(&str, u16)> {
    if let Some(rest) = s.strip_prefix('[') {
        // Bracketed IPv6 literal: `[::1]:4433`.
        let (h, p) = rest.split_once("]:")?;
        Some((h, p.parse().ok()?))
    } else {
        let (h, p) = s.rsplit_once(':')?;
        Some((h, p.parse().ok()?))
    }
}

/// Resolve a `host:port` string to a `SocketAddr`, **IDNA-aware**.
///
/// Plain `to_socket_addrs()` / `addr.parse::<SocketAddr>()` hand the host to the
/// OS resolver unchanged, and `getaddrinfo` does NOT do IDNA — so a Unicode
/// domain like `bücher.example` never resolves (and `parse` rejects *any* non-IP host
/// outright). We split off the port, run the host through IDNA ToASCII
/// (`bücher.example` → `xn--bcher-kva.example`) unless it is already an IP literal, then
/// resolve. IP literals and plain-ASCII names pass straight through.
pub fn resolve_addr(addr: &str) -> std::io::Result<SocketAddr> {
    use std::io::{Error, ErrorKind};
    let (host, port) = split_host_port(addr)
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, format!("no host:port in `{addr}`")))?;
    // IP literals (incl. bracketed IPv6) must skip IDNA — feed them as-is.
    let host_ascii = if host.parse::<std::net::IpAddr>().is_ok() {
        host.to_string()
    } else {
        idna::domain_to_ascii(host)
            .map_err(|e| Error::new(ErrorKind::InvalidInput, format!("idna `{host}`: {e}")))?
    };
    (host_ascii.as_str(), port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| Error::new(ErrorKind::NotFound, format!("no addresses for `{addr}`")))
}

/// Resolve each `host:port` candidate (IDNA-aware), preserving order and
/// dropping duplicates + any that fail to resolve. Unreachable-name warnings are
/// logged, not fatal — the surviving candidates are what [`connect_first`] races.
pub fn resolve_candidates(addrs: &[String]) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for a in addrs {
        match resolve_addr(a) {
            Ok(sa) if !out.contains(&sa) => out.push(sa),
            Ok(_) => {}
            Err(e) => tracing::warn!("resolve `{a}`: {e}"),
        }
    }
    out
}

/// Connect to the first reachable candidate, trying each in order with a short
/// per-attempt timeout. This is where **LAN-first / WAN-fallback** lives: the
/// caller passes `[lan, wan]` and an unreachable LAN address fails fast (the
/// timeout) before WAN is tried. The TLS server-name is irrelevant here
/// ([`SkipServerVerification`] — trust is the Ed25519 channel binding), so we
/// pass a fixed `"localhost"`.
async fn connect_first(endpoint: &Endpoint, addrs: &[SocketAddr]) -> Result<quinn::Connection> {
    use std::time::Duration;
    const PER_ATTEMPT: Duration = Duration::from_secs(4);
    let mut last_err: Option<anyhow::Error> = None;
    for (i, addr) in addrs.iter().enumerate() {
        let attempt = async {
            let conn = endpoint.connect(*addr, "localhost")?.await?;
            Ok::<_, anyhow::Error>(conn)
        };
        match tokio::time::timeout(PER_ATTEMPT, attempt).await {
            Ok(Ok(conn)) => {
                if i > 0 {
                    tracing::info!("connected to fallback candidate {addr} (#{i})");
                }
                return Ok(conn);
            }
            Ok(Err(e)) => {
                tracing::warn!("connect {addr}: {e}");
                last_err = Some(e);
            }
            Err(_) => {
                tracing::warn!("connect {addr}: timed out after {PER_ATTEMPT:?}");
                last_err = Some(anyhow::anyhow!("connect {addr}: timed out"));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no candidate addresses to connect")))
}

/// Send `data` from an in-process QUIC server to a QUIC client over a uni stream
/// and return what the client received. Proves the transport end to end.
pub fn quic_loopback(data: Vec<u8>) -> Result<Vec<u8>> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let server = Endpoint::server(server_config()?, (Ipv4Addr::LOCALHOST, 0).into())?;
        let addr: SocketAddr = server.local_addr()?;

        let payload = data.clone();
        let server_task = tokio::spawn(async move {
            let incoming = server
                .accept()
                .await
                .ok_or_else(|| anyhow::anyhow!("no incoming connection"))?;
            let conn = incoming.await?;
            let mut send = conn.open_uni().await?;
            send.write_all(&payload).await?;
            send.finish()?;
            conn.closed().await;
            anyhow::Ok(())
        });

        let mut client = Endpoint::client((Ipv4Addr::LOCALHOST, 0).into())?;
        client.set_default_client_config(client_config()?);
        let conn = client.connect(addr, "localhost")?.await?;
        let mut recv = conn.accept_uni().await?;
        let received = recv.read_to_end(64 * 1024 * 1024).await?;
        conn.close(0u32.into(), b"done");

        let _ = server_task.await;
        Ok(received)
    })
}

/// Domain-separated label fed to QUIC's TLS exporter so client and host
/// derive the same 32-byte value over the same TLS leg. A relay attacker
/// terminating TLS on each side computes different values, so the host's
/// signature won't verify on the client.
const VIDEO_AUTH_EXPORTER_LABEL: &[u8] = b"trireme-video-auth-v1 exporter";

/// 4-byte magic that a v2+ client writes BEFORE its 32-byte auth nonce so the
/// host can detect "this client speaks the extended handshake" without breaking
/// the v1 wire shape (which began with the raw nonce). The host reads the first
/// 4 bytes: if they match `AUTH_PROTO_MAGIC`, it's v2; otherwise treat those 4
/// bytes as the first 4 bytes of the v1 nonce.
///
/// The collision risk — a v1 client's random nonce starting with this exact
/// 32-bit value — is 1/2^32 (≈ 2.3e-10). On a false positive the v1 client's
/// handshake fails and the user reconnects; no security impact (the auth
/// signatures still bind to the full TLS exporter).
///
/// The byte sequence is deliberately invalid as the first 4 bytes of an
/// Ed25519 public key context, but the auth-stream uses raw bytes anyway.
const AUTH_PROTO_MAGIC: [u8; 4] = [0x54, 0x52, 0x21, 0x02]; // "TR!\x02"

/// Wire-version selected after the magic match. Increment when the handshake
/// extension shape changes. v2 = magic + version + nonce + pubkey + wants_gyro
/// byte. v3 appends a 12-byte `StreamMode` block (client→host requested
/// width/height/refresh_mhz). The host accepts BOTH v2 and v3 (an older v2
/// client just gets no per-client mode); host→client side is unchanged.
const AUTH_PROTO_VERSION: u32 = 4;
/// Oldest client wire version this host still accepts (kept for the v2 clients
/// — older phone/MiBox APKs — that don't send a `StreamMode` block).
const AUTH_PROTO_VERSION_MIN: u32 = 2;

/// Decides whether a connecting client may receive the stream. It's handed the
/// client's *verified* long-term pubkey (`Some` once the client has proven it
/// holds that Ed25519 key over the per-connection challenge), or `None` for an
/// anonymous client that presented no identity. Return `true` to admit.
///
/// Trust lives in the identity layer: the QUIC TLS cert is a throwaway, opaque
/// tunnel (see [`SkipServerVerification`]) — never a trust anchor — so this
/// hook, not certificate validation, is the host's gate on who may connect.
/// Host binaries build it from their paired-peers list.
pub type ClientAuthorizer = Arc<dyn Fn(Option<[u8; 32]>) -> bool + Send + Sync>;

/// An authorizer that admits any client. The connecting client still has to
/// *prove* any identity it presents, but membership isn't checked — this is the
/// permissive default; host binaries opt into peer enforcement explicitly.
pub fn allow_any_client() -> ClientAuthorizer {
    Arc::new(|_| true)
}

/// One queued video access unit, optionally carrying the latency bench's per-frame
/// stage meta so the `wire_send` stamp can be taken inline — at the same instant
/// the AU's bytes hit `write_all` — rather than in a separate, racing channel.
///
/// `meta` is `None` for normal streaming (the common path: [`QuicStreamer::send`]),
/// so an idle session's wire shape is byte-for-byte unchanged. During an on-demand
/// bench window the compositor uses [`QuicStreamer::send_with_meta`], which sets
/// `meta = Some(..)`; the transport fills `meta.wire_send_ns` just before the AU's
/// `write_all`, postcard-encodes it, and sends it as a `DG_FRAME_META` datagram
/// 1:1 with the AU.
#[derive(Clone)]
struct VideoChunk {
    /// `Arc<[u8]>` (not `Vec<u8>`) so the mirror dispatcher can fan ONE encoded
    /// AU out to N per-connection queues with a refcount bump instead of N deep
    /// copies. Wire-transparent: `&chunk.au` deref-coerces to `&[u8]` for
    /// `write_all`, and the single-client path never clones (mpsc moves it).
    au: std::sync::Arc<[u8]>,
    /// True when this AU is an IDR/I keyframe (from NVENC's `pictureType`). The
    /// bounded video queue reserves headroom for keyframes so a backlog of
    /// P-frames can never shut an IDR out: dropping an IDR strands the decoder
    /// with no clean reference for a whole GOP (~1s of propagating "pink block"
    /// corruption), whereas a dropped P-frame is resynced by the next IDR.
    /// `false` for callers that don't surface the picture type (the Windows
    /// host, tests), which preserves the historical drop-newest behaviour.
    keyframe: bool,
    meta: Option<core_proto::FrameStageMeta>,
}

/// Slots held in reserve, on top of `PHAROS_QUEUE_FRAMES`, exclusively for
/// keyframes. P-frames are admitted only while the queue holds < qdepth of them;
/// an IDR may additionally use these slots, so a momentary P-frame backlog can
/// never force the one frame that resyncs the decoder to be dropped. 2 leaves
/// room for a periodic IDR plus a forced one to slip past a full queue before it
/// drains, at a worst-case cost of ~2 extra frames of latency for keyframes only.
const KF_RESERVE: usize = 2;

/// Host-side QUIC streamer. Runs a quinn server on a background thread, accepts
/// one Trireme client, signs an Ed25519 challenge with its long-term identity
/// so the client can pin it, then forwards encoded chunks pushed via
/// [`QuicStreamer::send`] over a unidirectional stream.
///
/// Optionally also exposes an **input event** receiver: when the client opens
/// the input bidi stream (see [`run_video_client_with_input`]), each decoded
/// [`core_proto::InputEvent`] is forwarded to this receiver. The compositor
/// drains it from inside its event loop and dispatches to the smithay seat.
pub struct QuicStreamer {
    tx: mpsc::Sender<VideoChunk>,
    /// Count of video AUs the bounded `tx` queue has dropped because it was full,
    /// split by picture type. `dropped_pframes` ticking up means the link is
    /// briefly slower than the encode rate — the direct cause of reference-frame
    /// ("pink block") corruption the client sees; lower the resolution/bitrate.
    /// `dropped_keyframes` should stay 0 (keyframes get a reserve, see
    /// [`Self::send_frame`]); a non-zero value is the smoking gun for sustained
    /// corruption. Read via [`Self::dropped_frames`]; the drop is also logged
    /// (previously it was a completely silent `let _ = try_send`).
    dropped_pframes: Arc<std::sync::atomic::AtomicU64>,
    dropped_keyframes: std::sync::atomic::AtomicU64,
    /// Set when the CLIENT explicitly asks for an on-demand IDR via
    /// `DG_REQUEST_IDR` (e.g. it is rebuilding its decode chain, issue #32).
    /// The host swaps it out each frame ([`Self::take_idr_request`],
    /// rate-limited) and forces the next encode to be a keyframe. `Arc` so the
    /// serve thread's datagram task can set it too.
    request_idr: Arc<AtomicBool>,
    /// Set by the host itself whenever it DROPS a video AU (the reference chain
    /// just broke; the client will smear until a keyframe). Kept SEPARATE from
    /// `request_idr` so a host can coalesce drop-recovery IDRs much harder than
    /// client requests: under sustained congestion every queued frame drops, and
    /// honouring each drop at the client-request rate (~150ms) turns into a
    /// 4-6Hz forced-IDR storm whose oversized keyframes feed the very
    /// congestion that caused the drops (seen live on the 4K HDR wgc-zc path).
    /// [`Self::take_idr_request`] still folds both (the Linux compositor's
    /// existing behaviour); [`Self::take_idr_requests_split`] reads them apart.
    drop_idr: AtomicBool,
    /// Opus packets, sent as unreliable QUIC datagrams (see [`Self::send_audio`]).
    /// BOUNDED (drop-newest) so audio latency can't grow without limit when the
    /// datagram sender is briefly starved by a slow video `write_all` (they share
    /// one `select!` loop) — see the channel creation in `start_with_authorizer`.
    audio_tx: mpsc::Sender<Vec<u8>>,
    /// Gyro calibration samples, unreliable datagrams (see [`Self::send_gyro`]).
    gyro_tx: mpsc::UnboundedSender<Vec<u8>>,
    connected: Arc<AtomicBool>,
    /// The currently-connected client's *verified* long-term pubkey (the same
    /// 32 bytes the authorizer admitted), or `None` when idle / connected
    /// anonymously. Lets the host attribute the live stream to a specific paired
    /// peer in its status (so the panel can show that peer as Streaming rather
    /// than Unknown). Set at connect, cleared on disconnect.
    peer_pubkey: Arc<std::sync::Mutex<Option<[u8; 32]>>>,
    /// The currently-connected client's remote socket address (the UDP peer the
    /// quinn connection sees), or `None` when idle. Surfaced into `HostStatus`
    /// so the panel can show *where* the live client is dialing from, not just
    /// which paired key it is. Set at connect, cleared on disconnect — the addr
    /// companion to [`Self::peer_pubkey`].
    peer_addr: Arc<std::sync::Mutex<Option<SocketAddr>>>,
    /// Stream-time opt-in from the connected client: does it want raw IMU gyro
    /// relayed at all? Set during the auth handshake (v2 wire), cleared on
    /// disconnect or by any v1 client (which has no such field — default off).
    /// The compositor's per-frame gyro paths read this and skip both the
    /// integrate-to-mouse and the raw-sample stream when every connected client
    /// has opted out, so the IMU reader thread itself goes idle.
    wants_gyro: Arc<AtomicBool>,
    /// Does the connected client want a 10-bit HDR (Main10) stream? Set during
    /// the v4 auth handshake, cleared on disconnect or by any older client (which
    /// has no such field — default off). The compositor reads this via
    /// [`Self::wants_hdr`] and only engages HDR when it AND host capability AND an
    /// app-declared HDR surface all agree — a 10-bit stream an SDR-only client
    /// can't decode must never be forced on.
    wants_hdr: Arc<AtomicBool>,
    /// The connected client's panel refresh in whole Hz (0 = unknown / not
    /// reported). Read from the v3 auth handshake `StreamMode`; the compositor
    /// caps its encode fps to this so it never feeds the panel more frames than
    /// it can show (the fix for the 144→60 Hz decoder-overload corruption).
    /// Set at connect, cleared (0) on disconnect — companion to [`Self::wants_gyro`].
    client_refresh_hz: Arc<std::sync::atomic::AtomicU32>,
    /// The connected client's panel native resolution, packed `(w<<32)|h` (0 =
    /// unknown / not reported). Read from the v3 auth handshake `StreamMode`; the
    /// compositor reads it via [`Self::client_panel_res`] to decide whether the
    /// encode envelope must grow to render at the panel's native size (Phase 3c,
    /// restart-into-panel-res). Set at connect, cleared (0) on disconnect.
    client_panel: Arc<std::sync::atomic::AtomicU64>,
    /// The connected client's requested resolution range (Phase 2), packed
    /// `(target_h<<32)|floor_h` (0 = unset / Auto+no-floor). Set from
    /// `DG_CLIENT_RESPREF`; the compositor reads it via [`Self::client_res_pref`]
    /// and applies it as the per-output ceiling/floor. Cleared (0) on disconnect.
    client_res_pref: Arc<std::sync::atomic::AtomicU64>,
    /// The connected client's requested bitrate range (Phase 3), packed
    /// `(min_kbps<<32)|max_kbps` (0 = unset; max=0 = uncapped). Set from
    /// `DG_CLIENT_BITRATEPREF`; the compositor reads it via
    /// [`Self::client_bitrate_pref`] as the controller's bitrate bounds. Cleared
    /// on disconnect.
    client_bitrate_pref: Arc<std::sync::atomic::AtomicU64>,
    /// The connected client's requested framerate range (Phase 3), packed
    /// `(min_fps<<32)|max_fps` (0 = unset / Auto). Set from `DG_CLIENT_FPSPREF`;
    /// the compositor reads it via [`Self::client_fps_pref`] as the encode-pace
    /// ceiling + adaptation floor. Cleared on disconnect.
    client_fps_pref: Arc<std::sync::atomic::AtomicU64>,
    /// Connect-time bandwidth-probe state (issue #40): request/result
    /// generations, the client's transport hint, and the measured goodput.
    /// See [`ProbeShared`]; driven by [`Self::start_probe`] / [`Self::probe_result`].
    probe: Arc<ProbeShared>,
    /// Taken out once via [`Self::take_input_rx`].
    input_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<core_proto::InputEvent>>>,
    /// Decoded [`core_proto::WireLatencyReport`]s sent back by the client at the
    /// end of a bench window. Taken out once via [`Self::take_latency_rx`].
    latency_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<core_proto::WireLatencyReport>>>,
    /// Continuous client QoS reports (delivered fps + decode ms + measured
    /// goodput/transport, [`core_proto::ClientQosV2`]) the client emits ~1×/s.
    /// Old clients' v1 reports are widened with inert probe fields. Taken out
    /// once via [`Self::take_qos_rx`]; the compositor drains it to make pacing
    /// network-aware.
    qos_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<core_proto::ClientQosV2>>>,
    /// Latest app catalog to push to clients on the app relay stream. `watch` so
    /// a late-joining client gets the current value and each change re-sends.
    app_catalog_tx: tokio::sync::watch::Sender<Vec<core_proto::AppInfo>>,
    /// One-shot host→client status notices (toasts) on the app relay stream —
    /// e.g. "Steam was running — restarted it in-stream". Unlike the catalog
    /// (retained state) these are transient events, so an mpsc, not a watch:
    /// each notice is sent once to whoever is connected at that moment.
    app_notice_tx: tokio::sync::broadcast::Sender<String>,
    /// App-launch requests (`apps.toml` slugs) the client sent on the app relay
    /// stream. Taken out once via [`Self::take_app_launch_rx`]; the compositor
    /// drains it and launches into the virtual display.
    app_launch_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<core_proto::AppMsg>>>,
    /// Latest host clipboard text to push to the client on the clipboard relay
    /// stream (host→client). `watch` so a late-joining client resyncs to the
    /// current value and each host copy re-sends. Set via [`Self::send_clipboard`];
    /// a Clone of the sender is handed to the compositor via [`Self::clipboard_sender`]
    /// so a detached selection-reader thread can push without borrowing the streamer.
    clipboard_out_tx: tokio::sync::watch::Sender<core_proto::ClipboardMsg>,
    /// Clipboard text the client copied and sent on the clipboard relay stream
    /// (client→host). Taken out once via [`Self::take_clipboard_in_rx`]; the
    /// compositor drains it and becomes the Wayland selection owner so host apps
    /// can paste it.
    clipboard_in_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<core_proto::ClipboardMsg>>>,
    /// Current encoder output geometry, packed `(w<<32)|h` (0 = unset). The
    /// compositor sets it via [`Self::set_geometry`]; the stream loop announces it
    /// to the client as `DG_STREAM_GEOMETRY` so the client sizes its decoder to
    /// the real resolution (and re-announces on change for the res actuator).
    geom: Arc<std::sync::atomic::AtomicU64>,
    /// Colorimetry of the CURRENT encode as a tri-state (issue #28): 0 = UNDETERMINED
    /// (no pipeline built yet — the announce loop stays silent and the client
    /// plain-decodes), 1 = SDR/BT.709, 2 = HDR (10-bit Main10/BT.2020/PQ). Set via
    /// [`Self::set_hdr`] at each pipeline build; announced as `DG_STREAM_HDR` only once
    /// determined, so the client never gets a premature SDR flag it must later correct
    /// (the connect-time gray wash). Re-announced on change + ~1×/s on the geometry
    /// cadence; the client inserts its HDR→SDR tonemap only for 2 (issue #16).
    hdr: Arc<std::sync::atomic::AtomicU8>,
    /// Whether the host GAME currently has the cursor CAPTURED (true = the game
    /// grabbed/hid the mouse for relative mouselook; false = FREE, the cursor is a
    /// normal absolute pointer over a menu/desktop). The host sets it via
    /// [`Self::set_cursor_captured`]; the stream loop announces it as
    /// `DG_CURSOR_MODE` so the desktop client auto-locks its own cursor (relative)
    /// during gameplay and frees it (absolute) in menus, the native windowed-game
    /// feel with no manual toggle. Re-announced on change + ~1×/s on the same cadence
    /// as the geometry/HDR.
    cursor_captured: Arc<AtomicBool>,
    /// Forward-path packet loss permille over the last ~1s, from quinn's PathStats.
    /// Read by the compositor as a host-side congestion signal (works even when the
    /// client sends no QoS). See [`Self::loss_permille`].
    loss_permille: Arc<std::sync::atomic::AtomicU32>,
    /// Smoothed forward-path RTT in microseconds (quinn PathStats), ~1s cadence. A
    /// latency (bufferbloat) congestion signal the host bitrate loop watches — RTT
    /// inflation over the path baseline catches queue buildup that packet loss and the
    /// delivered-fps deficit both miss (issue #30). See [`Self::rtt_us`].
    rtt_us: Arc<std::sync::atomic::AtomicU32>,
    /// Live per-connection registry — populated ONLY by the mirror keep-N path;
    /// empty in single mode (so [`Self::connected_clients`] falls back to the
    /// scalar peer fields). The compositor snapshots it to publish one
    /// `ClientStatus` per mirrored client + a truthful `MirrorClamp.client_count`.
    clients: Arc<std::sync::Mutex<Vec<ClientSlot>>>,
}

/// One live mirrored client: its identity/addr for status, a per-client bounded
/// video queue (`tx`) the dispatcher fans AUs into (drop-newest, never blocks
/// the others), and self-measured counters. `id` is monotonic so "primary"
/// (the input/app driver) can be derived as `min(id)` — which auto-reassigns to
/// the next-oldest when a primary disconnects, no explicit hand-off.
struct ClientSlot {
    id: u64,
    peer_pubkey: Option<[u8; 32]>,
    peer_addr: SocketAddr,
    /// Live connection handle (cheap clone) so the dispatcher can fan audio/gyro
    /// datagrams without owning the actor's send stream.
    conn: quinn::Connection,
    /// Bounded per-client AU queue; the actor task drains it onto the wire.
    tx: mpsc::Sender<VideoChunk>,
    /// Wire fps measured by the actor over ~1s windows (0 until first window).
    fps_ewma: Arc<std::sync::atomic::AtomicU32>,
    frames_sent: Arc<std::sync::atomic::AtomicU64>,
}

/// A status-facing snapshot of one connected client, uniform across single and
/// mirror so the compositor builds `ClientStatus` the same way in both.
pub struct ClientSnapshot {
    pub addr: Option<String>,
    pub pubkey: Option<String>,
    pub fps: u32,
    pub frames_sent: u64,
}

impl QuicStreamer {
    /// `identity` is the host's long-term Ed25519 keypair. The client uses
    /// its (pinned) pubkey to verify the challenge-response on connect.
    ///
    /// Admits any authenticated client. Use [`Self::start_with_authorizer`] to
    /// restrict streaming to paired peers.
    pub fn start(addr: SocketAddr, identity: Arc<core_crypto::Identity>) -> Result<Self> {
        // No mirror flag wired → always single-client (a fresh always-false flag).
        // IPv4-only bind (dual_stack=false) — the default; `start_with_authorizer`
        // takes the flag for the config-driven host path.
        Self::start_with_authorizer(
            addr,
            identity,
            allow_any_client(),
            Arc::new(AtomicBool::new(false)),
            false,
        )
    }

    /// Like [`Self::start`] but gates every client through `authorizer` (e.g.
    /// "must be in peers.toml") *after* it has cryptographically proven its
    /// identity. The host still signs its own challenge, so host→client pinning
    /// is unchanged — this only adds the reverse direction.
    ///
    /// `mirror` is the compositor-owned fan-out flag (panel `SetStreamMode`):
    /// `false` = single-client/pre-empt (validated default), `true` = keep-N.
    pub fn start_with_authorizer(
        addr: SocketAddr,
        identity: Arc<core_crypto::Identity>,
        authorizer: ClientAuthorizer,
        mirror: Arc<AtomicBool>,
        // Bind the QUIC listener dual-stack (`[::]` V6ONLY off) so it accepts IPv6
        // + IPv4 on one socket. From `[network] ipv6`. `false` = IPv4 wildcard.
        dual_stack: bool,
    ) -> Result<Self> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        // Bounded video queue: cap the in-flight frames so latency can't grow
        // without limit when the link is briefly slower than the encode rate.
        // Depth trades latency vs smoothness — PHAROS_QUEUE_FRAMES tunes it
        // (default 8 ≈ 130ms @60fps); P-frames are dropped-newest when full.
        // The channel is sized qdepth + KF_RESERVE: P-frames are admitted only
        // while < qdepth are queued, but a keyframe may also use the reserve, so
        // an IDR is never shut out by a P-frame backlog (see `enqueue`).
        let qdepth = std::env::var("PHAROS_QUEUE_FRAMES")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(8);
        let (tx, rx) = mpsc::channel::<VideoChunk>(qdepth + KF_RESERVE);
        // Bounded audio queue — the audio analogue of the video queue above. The
        // datagram sender drains this in the SAME `select!` loop as the reliable,
        // flow-controlled video `write_all`; while a slow video write is awaited,
        // audio can't drain. An UNBOUNDED channel turned that transient into a
        // permanent 1–1.5s backlog (it tracked the QUIC stream window). A small
        // bound sheds stale audio (drop-newest in `send_audio`) so latency stays
        // capped; PHAROS_AUDIO_QUEUE_PACKETS tunes it (default 8 ≈ 160ms @ 20ms/pkt).
        let audio_depth = std::env::var("PHAROS_AUDIO_QUEUE_PACKETS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(8);
        let (audio_tx, audio_rx) = mpsc::channel::<Vec<u8>>(audio_depth);
        let (gyro_tx, gyro_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (input_tx, input_rx) = mpsc::unbounded_channel::<core_proto::InputEvent>();
        let (latency_tx, latency_rx) = mpsc::unbounded_channel::<core_proto::WireLatencyReport>();
        let (qos_tx, qos_rx) = mpsc::unbounded_channel::<core_proto::ClientQosV2>();
        let probe = Arc::new(ProbeShared::default());
        let probe_thread = probe.clone();
        let (app_catalog_tx, app_catalog_rx) =
            tokio::sync::watch::channel::<Vec<core_proto::AppInfo>>(Vec::new());
        let (app_launch_tx, app_launch_rx) = mpsc::unbounded_channel::<core_proto::AppMsg>();
        // Broadcast so each per-connection app-relay writer gets its own receiver
        // (a late reconnect re-subscribes); capacity is small — notices are rare.
        let (app_notice_tx, _app_notice_rx0) = tokio::sync::broadcast::channel::<String>(16);
        let app_notice_tx_thread = app_notice_tx.clone();
        // Clipboard relay channels (host↔client). `clipboard_out` is a watch (latest
        // host clipboard wins; a reconnecting client resyncs); `clipboard_in` is an
        // mpsc of client→host copies the compositor drains. Empty initial value is
        // skipped by the relay writer so a fresh streamer doesn't push a blank clip.
        let (clipboard_out_tx, clipboard_out_rx) =
            tokio::sync::watch::channel::<core_proto::ClipboardMsg>(
                core_proto::ClipboardMsg::Text(String::new()),
            );
        let (clipboard_in_tx, clipboard_in_rx) =
            mpsc::unbounded_channel::<core_proto::ClipboardMsg>();
        let connected = Arc::new(AtomicBool::new(false));
        let connected_thread = connected.clone();
        // Verified pubkey of whoever's connected (None when idle). Set/cleared
        // alongside `connected` inside serve_stream's accept loop.
        let peer_pubkey = Arc::new(std::sync::Mutex::new(None));
        let peer_pubkey_thread = peer_pubkey.clone();
        // Remote addr of whoever's connected (None when idle). Set/cleared
        // alongside `peer_pubkey` inside serve_stream's accept loop.
        let peer_addr = Arc::new(std::sync::Mutex::new(None));
        let peer_addr_thread = peer_addr.clone();
        // Defaults to false: a fresh streamer with no connected client wants
        // no gyro. Set true in serve_stream during a v2 client's auth when its
        // wants_gyro flag is on; cleared on disconnect.
        let wants_gyro = Arc::new(AtomicBool::new(false));
        let wants_gyro_thread = wants_gyro.clone();
        // Client's HDR request (v4 auth). Defaults false; set in serve_stream.
        let wants_hdr = Arc::new(AtomicBool::new(false));
        let wants_hdr_thread = wants_hdr.clone();
        // Connected client's panel refresh (Hz, 0 = unknown). Set in serve_stream
        // from the v3 auth handshake; the compositor reads it to cap fps.
        let client_refresh_hz = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let client_refresh_hz_thread = client_refresh_hz.clone();
        // Client's panel native resolution (Phase 3c), packed (w<<32)|h.
        let client_panel = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let client_panel_thread = client_panel.clone();
        // Client's requested resolution range (Phase 2), packed (target_h<<32)|floor_h.
        let client_res_pref = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let client_res_pref_thread = client_res_pref.clone();
        // Client's requested bitrate range (Phase 3), packed (min_kbps<<32)|max_kbps.
        let client_bitrate_pref = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let client_bitrate_pref_thread = client_bitrate_pref.clone();
        // Client's requested framerate range (Phase 3), packed (min_fps<<32)|max_fps.
        let client_fps_pref = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let client_fps_pref_thread = client_fps_pref.clone();
        let geom = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let geom_thread = geom.clone();
        // Colorimetry starts UNDETERMINED (0); set_hdr moves it to 1/2 at the first
        // pipeline build, and the announce loops stay silent until then (issue #28).
        let hdr = Arc::new(std::sync::atomic::AtomicU8::new(0));
        let hdr_thread = hdr.clone();
        let cursor_captured = Arc::new(AtomicBool::new(false));
        let cursor_captured_thread = cursor_captured.clone();
        let loss_permille = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let loss_thread = loss_permille.clone();
        let rtt_us = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let rtt_us_thread = rtt_us.clone();
        // P-frame drop counter, shared with the serve thread so its connection
        // loop can emit the DG_LINK_HEALTH heartbeat (the producer-side note_drop
        // increments it). Monotonic — the emit side resets its own per-connection
        // baseline. (dropped_keyframes stays a plain field; only this feeds health.)
        let dropped_pframes = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let drops_thread = dropped_pframes.clone();
        // On-demand IDR request flag, shared with the serve thread so its client
        // datagram task can set it on `DG_REQUEST_IDR` (the host also sets it
        // itself on a queue drop, via `note_drop`).
        let request_idr = Arc::new(AtomicBool::new(false));
        let request_idr_thread = request_idr.clone();
        // The mirror flag is read only by the serve thread (the streamer struct
        // never needs it back), so it moves straight in — no struct field.
        let clients: Arc<std::sync::Mutex<Vec<ClientSlot>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let clients_thread = clients.clone();
        std::thread::Builder::new()
            .name("quic-streamer".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::error!("quic runtime: {e}");
                        return;
                    }
                };
                if let Err(e) = rt.block_on(serve_stream(
                    addr,
                    rx,
                    audio_rx,
                    gyro_rx,
                    connected_thread,
                    peer_pubkey_thread,
                    peer_addr_thread,
                    wants_gyro_thread,
                    wants_hdr_thread,
                    client_refresh_hz_thread,
                    client_panel_thread,
                    client_res_pref_thread,
                    client_bitrate_pref_thread,
                    client_fps_pref_thread,
                    identity,
                    input_tx,
                    latency_tx,
                    qos_tx,
                    probe_thread.clone(),
                    app_catalog_rx,
                    app_notice_tx_thread.clone(),
                    app_launch_tx,
                    clipboard_out_rx,
                    clipboard_in_tx,
                    authorizer,
                    geom_thread,
                    hdr_thread,
                    cursor_captured_thread,
                    loss_thread,
                    rtt_us_thread,
                    drops_thread,
                    request_idr_thread,
                    mirror,
                    clients_thread,
                    dual_stack,
                )) {
                    tracing::error!("quic streamer: {e:?}");
                }
            })?;
        Ok(Self {
            tx,
            dropped_pframes,
            dropped_keyframes: std::sync::atomic::AtomicU64::new(0),
            request_idr,
            drop_idr: AtomicBool::new(false),
            audio_tx,
            gyro_tx,
            connected,
            peer_pubkey,
            peer_addr,
            wants_gyro,
            wants_hdr,
            client_refresh_hz,
            client_panel,
            client_res_pref,
            client_bitrate_pref,
            client_fps_pref,
            input_rx: std::sync::Mutex::new(Some(input_rx)),
            latency_rx: std::sync::Mutex::new(Some(latency_rx)),
            qos_rx: std::sync::Mutex::new(Some(qos_rx)),
            probe,
            app_catalog_tx,
            app_notice_tx,
            app_launch_rx: std::sync::Mutex::new(Some(app_launch_rx)),
            clipboard_out_tx,
            clipboard_in_rx: std::sync::Mutex::new(Some(clipboard_in_rx)),
            geom,
            hdr,
            cursor_captured,
            loss_permille,
            rtt_us,
            clients,
        })
    }

    /// Forward-path packet loss (permille, 0–1000) over the last ~1s, from quinn's
    /// PathStats. A host-side congestion signal the pacer uses when the client
    /// reports no QoS. 0 when idle / no client.
    pub fn loss_permille(&self) -> u32 {
        self.loss_permille
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Smoothed forward-path RTT in microseconds (quinn PathStats), ~1s cadence; 0 when
    /// idle / no client. The host bitrate loop watches its inflation over the path
    /// baseline as a bufferbloat signal (issue #30).
    pub fn rtt_us(&self) -> u32 {
        self.rtt_us.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Tell the streamer the encoder's current output geometry. Announced to the
    /// connected client as `DG_STREAM_GEOMETRY` (at stream open + on change), so
    /// the client sizes its decoder to the true resolution. Safe to call before
    /// any client connects (the value is latched and sent on the next connect).
    pub fn set_geometry(&self, w: u32, h: u32) {
        self.geom.store(
            ((w as u64) << 32) | h as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// Tell the streamer whether the CURRENT encode is HDR (true = 10-bit
    /// Main10/BT.2020/PQ, false = SDR/BT.709). Announced to the connected client as
    /// `DG_STREAM_HDR` (at stream open + on change), so the client only inserts its
    /// HDR→SDR tonemap when the stream is truly HDR (issue #16). Only ever called at a
    /// real pipeline (re)build — the field starts UNDETERMINED and nothing is announced
    /// until then, so a client that connects before the first build is NOT told a
    /// premature SDR (the connect-time gray wash, issue #28). A mid-session SDR↔HDR flip
    /// at a respawn boundary re-announces the new value.
    pub fn set_hdr(&self, hdr: bool) {
        // 2 = HDR, 1 = SDR (0 = undetermined, the initial state, never stored here).
        self.hdr.store(if hdr { 2 } else { 1 }, Ordering::Relaxed);
    }

    /// Tell the streamer whether the host GAME currently has the cursor CAPTURED
    /// (true = grabbed/hidden for relative mouselook, false = FREE / absolute over a
    /// menu or desktop). Announced to the connected client as `DG_CURSOR_MODE` (at
    /// stream open + on change), so the desktop client mirrors the host game's own
    /// cursor-grab state: it auto-locks to relative during gameplay and frees to
    /// absolute in menus, with no manual toggle. Safe to call before any client
    /// connects (latched, sent on the next connect); the host polls it ~60×/s.
    pub fn set_cursor_captured(&self, captured: bool) {
        self.cursor_captured.store(captured, Ordering::Relaxed);
    }

    /// Queue an encoded video chunk for delivery. Non-blocking. Picture type
    /// unknown → treated as a droppable P-frame (drop-newest when full), the
    /// historical behaviour. Used by callers that don't surface NVENC's
    /// `pictureType` (the Windows host, tests). The Linux/NVENC path uses
    /// [`Self::send_frame`] so IDRs get the keyframe reserve.
    pub fn send(&self, bytes: Vec<u8>) {
        self.enqueue(bytes.into(), false, None);
    }

    /// Like [`Self::send`] but tags the AU with its keyframe flag (from NVENC's
    /// `pictureType`) so the bounded queue never drops an IDR in favour of a
    /// P-frame backlog. Non-blocking.
    pub fn send_frame(&self, bytes: Vec<u8>, keyframe: bool) {
        self.enqueue(bytes.into(), keyframe, None);
    }

    /// Queue an encoded video chunk *with* its latency-bench stage meta, riding the
    /// same channel as [`Self::send`] so the AU and its meta stay 1:1 and in order.
    /// `meta.wire_send_ns` is left for the transport to fill at `write_all` time
    /// (the encoder can't know when the bytes actually hit the wire). Non-blocking.
    ///
    /// The compositor calls this ONLY during an on-demand bench window — emitting
    /// the *first* meta is itself the signal that tells the client to start the
    /// benchmark (no separate control message). When no bench is running,
    /// [`Self::send_frame`] is used instead.
    pub fn send_with_meta(&self, bytes: Vec<u8>, keyframe: bool, meta: core_proto::FrameStageMeta) {
        self.enqueue(bytes.into(), keyframe, Some(meta));
    }

    /// Shared admission path for [`Self::send`] / [`Self::send_frame`] /
    /// [`Self::send_with_meta`]. Drop-newest when the queue is full — EXCEPT a
    /// keyframe may also use the `KF_RESERVE` slots so it is never shut out by
    /// queued P-frames (losing an IDR = a whole GOP of propagating corruption;
    /// losing a P-frame is resynced by the next IDR). Every drop is counted and
    /// logged so the shed is observable — it used to be a silent `let _ = try_send`.
    fn enqueue(
        &self,
        au: std::sync::Arc<[u8]>,
        keyframe: bool,
        meta: Option<core_proto::FrameStageMeta>,
    ) {
        // `capacity()` = free slots right now. The channel is sized qdepth+KF_RESERVE,
        // so "> KF_RESERVE free" means fewer than qdepth frames are queued. A keyframe
        // is admitted as long as ANY slot is free, dipping into the reserve.
        let free = self.tx.capacity();
        let admit = if keyframe {
            free > 0
        } else {
            free > KF_RESERVE
        };
        if admit && self.tx.try_send(VideoChunk { au, keyframe, meta }).is_ok() {
            return;
        }
        self.note_drop(keyframe);
    }

    /// Record + log a dropped video AU (the bounded queue was full). P-frame drops
    /// are throttled to ~1 log line per 30; a keyframe drop always warns (it should
    /// not happen given the reserve, and directly explains visible corruption).
    fn note_drop(&self, keyframe: bool) {
        use std::sync::atomic::Ordering::Relaxed;
        // A dropped AU only matters while a client is consuming the stream. When
        // idle the queue fills and "drops" every frame — meaningless, and it would
        // both spam the log and churn on-demand IDRs. Only count/log/recover when
        // connected.
        if !self.connected.load(Relaxed) {
            return;
        }
        if keyframe {
            let n = self.dropped_keyframes.fetch_add(1, Relaxed) + 1;
            tracing::warn!(
                target: "transport::queue",
                "video queue dropped a KEYFRAME (total={n}) — keyframe reserve exhausted; \
                 client will smear until the next periodic IDR"
            );
        } else {
            let n = self.dropped_pframes.fetch_add(1, Relaxed) + 1;
            if n == 1 || n.is_multiple_of(30) {
                tracing::warn!(
                    target: "transport::queue",
                    "video queue full: dropped {n} P-frames total (link briefly slower than the \
                     encode rate → reference-frame corruption; lower the resolution/bitrate)"
                );
            }
        }
        // The reference chain just broke — ask the encoder for an on-demand IDR so
        // the client resyncs instead of waiting for the periodic one. Recorded on
        // the DROP flag (not `request_idr`) so hosts can coalesce drop-recovery
        // separately from client requests; the caller rate-limits the force.
        self.drop_idr.store(true, Relaxed);
    }

    /// Video AUs the bounded queue has dropped since start: `(p_frames, keyframes)`.
    /// `keyframes` should be 0; a non-zero value is the smoking gun for sustained
    /// "pink block" corruption. Cheap atomic loads — safe to poll for status.
    pub fn dropped_frames(&self) -> (u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            self.dropped_pframes.load(Relaxed),
            self.dropped_keyframes.load(Relaxed),
        )
    }

    /// Take (and clear) a pending on-demand IDR request — returns true if the
    /// compositor should force the next encode to be a keyframe. Set by the host
    /// on a queue drop ([`Self::note_drop`]) and by the client via `DG_REQUEST_IDR`.
    /// The compositor rate-limits how often it actually forces one. Folds BOTH
    /// sources (the Linux compositor's historical behaviour); a host that wants
    /// to coalesce drop-recovery harder uses [`Self::take_idr_requests_split`].
    pub fn take_idr_request(&self) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        // Swap both — `|` (not `||`) so the second flag is consumed even when
        // the first is already true.
        self.request_idr.swap(false, Relaxed) | self.drop_idr.swap(false, Relaxed)
    }

    /// Take (and clear) the pending on-demand IDR requests by source:
    /// `(client_requested, drop_recovery)`. The first is a client's explicit
    /// `DG_REQUEST_IDR` (resync fast — it is rebuilding its decode chain); the
    /// second is the host's own note-a-drop trigger, which the caller should
    /// coalesce much harder (see `drop_idr`'s field docs for the storm this
    /// prevents).
    pub fn take_idr_requests_split(&self) -> (bool, bool) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            self.request_idr.swap(false, Relaxed),
            self.drop_idr.swap(false, Relaxed),
        )
    }

    /// Queue one Opus audio packet. Delivered as an unreliable QUIC datagram
    /// (deadline-aware — a dropped packet beats head-of-line-blocking video).
    /// Non-blocking; only actually transmitted while a client is connected.
    /// Drop-newest when the bounded queue is full (like [`Self::send`]) so a
    /// backlog from a stalled video write can't accumulate into seconds of lag.
    pub fn send_audio(&self, packet: Vec<u8>) {
        let _ = self.audio_tx.try_send(packet);
    }

    /// Queue one gyro telemetry packet (12 bytes: 3×f32 LE, deg/s) for the
    /// client's calibration visualizer. Unreliable datagram, non-blocking; only
    /// transmitted while a client is connected.
    pub fn send_gyro(&self, packet: Vec<u8>) {
        let _ = self.gyro_tx.send(packet);
    }

    /// A cheap, clonable handle for an audio-capture thread to push Opus
    /// packets. It does *not* keep the streamer alive — dropping the streamer
    /// still ends the session via the video channel — so the capture thread can
    /// outlive a client and resume on reconnect (gated by [`AudioHandle::is_connected`]).
    pub fn audio_handle(&self) -> AudioHandle {
        AudioHandle {
            tx: self.audio_tx.clone(),
            connected: self.connected.clone(),
        }
    }

    /// True once a client has connected, passed the auth challenge, and the
    /// send stream is open. The host gates its encode loop on this so frames
    /// aren't buffered pre-connect.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    /// Lowercase-hex of the connected client's verified pubkey, or `None` when
    /// idle / connected anonymously. The hex shape matches `peers.toml`, so the
    /// host can join a live stream to a specific paired peer in its status.
    pub fn connected_pubkey(&self) -> Option<String> {
        let bytes = (*self.peer_pubkey.lock().ok()?)?;
        Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// The connected client's remote socket address as a string (e.g.
    /// `192.0.2.10:51234`), or `None` when idle. The addr companion to
    /// [`Self::connected_pubkey`]; the panel shows it as the live peer's
    /// address instead of `—`.
    pub fn connected_addr(&self) -> Option<String> {
        Some((*self.peer_addr.lock().ok()?)?.to_string())
    }

    /// Uniform view of the connected client(s) for the compositor's status push.
    /// In MIRROR mode this snapshots the per-connection registry (one entry per
    /// mirrored client, with self-measured fps/frames). In SINGLE mode the
    /// registry is empty, so it falls back to the scalar peer fields and yields
    /// ≤1 entry — the compositor keeps using its own `measured_fps_ewma`/frame
    /// counter for that one, so single-mode status output is unchanged.
    pub fn connected_clients(&self) -> Vec<ClientSnapshot> {
        if let Ok(slots) = self.clients.lock() {
            if !slots.is_empty() {
                return slots
                    .iter()
                    .map(|s| ClientSnapshot {
                        addr: Some(s.peer_addr.to_string()),
                        pubkey: s
                            .peer_pubkey
                            .map(|b| b.iter().map(|x| format!("{x:02x}")).collect()),
                        fps: s.fps_ewma.load(Ordering::Relaxed),
                        frames_sent: s.frames_sent.load(Ordering::Relaxed),
                    })
                    .collect();
            }
        }
        // Single mode: derive the 0/1-element view from the scalar peer fields.
        if self.is_connected() {
            vec![ClientSnapshot {
                addr: self.connected_addr(),
                pubkey: self.connected_pubkey(),
                fps: 0,
                frames_sent: 0,
            }]
        } else {
            Vec::new()
        }
    }

    /// True when the currently-connected client opted in to gyro relay during
    /// the v2 auth handshake. Stays false for the v1 wire (the client had no
    /// way to ask) and for any session with no client connected. The
    /// compositor reads this each frame and skips the gyro IMU drain + raw
    /// sample stream when no streamer wants gyro, taking the IMU reader to
    /// an idle 500ms sleep.
    pub fn wants_gyro(&self) -> bool {
        self.wants_gyro.load(Ordering::Acquire)
    }

    /// Does the connected client want a 10-bit HDR stream? Set from the v4 auth
    /// handshake; false for older clients and when no client is connected. The
    /// compositor gates HDR (10-bit Main10 session) on this AND host capability
    /// AND an app-declared HDR surface.
    pub fn wants_hdr(&self) -> bool {
        self.wants_hdr.load(Ordering::Acquire)
    }

    /// The connected client's panel refresh in whole Hz, or 0 if unknown / no
    /// client. The compositor caps its encode fps to this (never feeding a panel
    /// more frames than it shows). Set from the v3 auth handshake `StreamMode`.
    pub fn client_refresh_hz(&self) -> u32 {
        self.client_refresh_hz.load(Ordering::Acquire)
    }

    /// The connected client's panel native resolution as `(w, h)` in px; `(0, 0)`
    /// = unknown / no client (a v2 client that sent no `StreamMode` w/h, or a
    /// client that reported 0). The compositor reads it on connect to decide
    /// whether the encode envelope must grow to render at panel-native size
    /// (Phase 3c). Set from the v3 auth handshake, cleared on disconnect.
    pub fn client_panel_res(&self) -> (u32, u32) {
        let v = self.client_panel.load(Ordering::Acquire);
        ((v >> 32) as u32, v as u32)
    }

    /// The connected client's requested resolution range (Phase 2) as
    /// `(target_h, floor_h)` in px; `(0, 0)` = unset (Auto + no floor). The
    /// compositor reads it each window and applies it as the per-output ceiling
    /// and minimum. Set from `DG_CLIENT_RESPREF`, cleared on disconnect.
    pub fn client_res_pref(&self) -> (u32, u32) {
        let v = self.client_res_pref.load(Ordering::Relaxed);
        ((v >> 32) as u32, v as u32)
    }

    /// The connected client's requested bitrate range (Phase 3) as
    /// `(min_kbps, max_kbps)`; `(0, 0)` = unset, `max_kbps == 0` = uncapped. The
    /// compositor reads it as the controller's bitrate bounds. Cleared on disconnect.
    pub fn client_bitrate_pref(&self) -> (u32, u32) {
        let v = self.client_bitrate_pref.load(Ordering::Relaxed);
        ((v >> 32) as u32, v as u32)
    }

    /// The connected client's requested framerate range (Phase 3) as
    /// `(min_fps, max_fps)`; `(0, 0)` = unset, `max_fps == 0` = Auto (panel
    /// refresh). The compositor reads it as the encode-pace ceiling + adaptation
    /// floor. Cleared on disconnect.
    pub fn client_fps_pref(&self) -> (u32, u32) {
        let v = self.client_fps_pref.load(Ordering::Relaxed);
        ((v >> 32) as u32, v as u32)
    }

    /// Kick a connect-time bandwidth probe (issue #40): the per-connection pad
    /// task ramps content-free `DG_PROBE_PAD` datagrams toward `ceiling_kbps`
    /// of OFFERED load for `duration_ms`, then holds a ~1.5s tail so the
    /// client's next QoS windows report the delivered goodput. The wire never
    /// carries more than BBR allows (QUIC datagrams are congestion-controlled;
    /// excess padding is dropped in the send buffer), so the ceiling bounds
    /// waste, not safety. Poll [`Self::probe_result`] for the measurement.
    pub fn start_probe(&self, ceiling_kbps: u32, duration_ms: u32) {
        self.probe
            .ceiling_kbps
            .store(ceiling_kbps, Ordering::Relaxed);
        self.probe.duration_ms.store(duration_ms, Ordering::Relaxed);
        self.probe.req_gen.fetch_add(1, Ordering::Relaxed);
    }

    /// The finished probe's peak delivered goodput (kbps), once the latest
    /// [`Self::start_probe`] request has completed and measured anything.
    /// `None` while in flight, when no probe ran, or when the client never
    /// reported goodput (old client → the caller keeps today's behavior).
    pub fn probe_result(&self) -> Option<u32> {
        let done = self.probe.done_gen.load(Ordering::Relaxed);
        if done != self.probe.req_gen.load(Ordering::Relaxed) {
            return None; // in flight (or cancelled by a reconnect)
        }
        match self.probe.measured_kbps.load(Ordering::Relaxed) {
            0 => None,
            kbps => Some(kbps),
        }
    }

    /// The connected client's link-transport hint (`core_proto::TRANSPORT_*`),
    /// from its v2 QoS reports. `TRANSPORT_UNKNOWN` for old clients.
    pub fn client_transport(&self) -> u8 {
        self.probe.transport.load(Ordering::Relaxed)
    }

    /// Whether this client has ever reported measured goodput (v2 QoS with a
    /// non-zero `recv_kbps`) — the gate for bothering to probe at all.
    pub fn client_reports_goodput(&self) -> bool {
        self.probe.goodput_seen.load(Ordering::Relaxed)
    }

    /// Take the InputEvent receiver — call exactly once. Subsequent calls
    /// return `None`. The compositor pulls events from this and dispatches
    /// them to the smithay seat.
    pub fn take_input_rx(&self) -> Option<mpsc::UnboundedReceiver<core_proto::InputEvent>> {
        self.input_rx.lock().ok().and_then(|mut g| g.take())
    }

    /// Take the latency-report receiver — call exactly once. Subsequent calls
    /// return `None`. The panel/compositor drains it to surface each
    /// [`core_proto::WireLatencyReport`] the client sends back at the end of a
    /// bench window.
    pub fn take_latency_rx(
        &self,
    ) -> Option<mpsc::UnboundedReceiver<core_proto::WireLatencyReport>> {
        self.latency_rx.lock().ok().and_then(|mut g| g.take())
    }

    /// Take the client-QoS receiver — call exactly once. The compositor drains it
    /// to feed delivered-fps into the pacer (network-aware quality).
    pub fn take_qos_rx(&self) -> Option<mpsc::UnboundedReceiver<core_proto::ClientQosV2>> {
        self.qos_rx.lock().ok().and_then(|mut g| g.take())
    }

    /// Publish the app catalog to push to clients on the app relay stream.
    /// Cheap; the latest value is retained and (re-)sent to each connected and
    /// future client. Non-blocking — safe to call even with no client connected.
    pub fn send_app_catalog(&self, apps: Vec<core_proto::AppInfo>) {
        let _ = self.app_catalog_tx.send(apps);
    }

    /// Push a transient host→client status notice (toast) on the app relay — e.g.
    /// the auto Steam close+relaunch note. Non-blocking; dropped harmlessly if no
    /// client is connected (nobody is subscribed).
    pub fn send_app_notice(&self, message: String) {
        let _ = self.app_notice_tx.send(message);
    }

    /// Take the app-launch receiver — call exactly once. Subsequent calls return
    /// `None`. The compositor drains it and launches each requested slug into the
    /// virtual display (the client-tile path, mirroring the panel/CLI `LaunchApp`).
    pub fn take_app_launch_rx(&self) -> Option<mpsc::UnboundedReceiver<core_proto::AppMsg>> {
        self.app_launch_rx.lock().ok().and_then(|mut g| g.take())
    }

    /// Publish host clipboard text to push to the client (host→client). Latest
    /// value is retained and (re-)sent to each connected + future client. Empty
    /// string clears the client's clipboard. Non-blocking; safe with no client.
    pub fn send_clipboard(&self, text: String) {
        let _ = self
            .clipboard_out_tx
            .send(core_proto::ClipboardMsg::Text(text));
    }

    /// A Clone of the host→client clipboard sender. Handed to the compositor so a
    /// detached Wayland-selection reader thread can push clipboard text without
    /// borrowing the (non-Clone, render-stack-owned) [`QuicStreamer`]. Mirrors the
    /// [`Self::audio_handle`] carve-off. `watch::Sender` is `Clone + Send`.
    pub fn clipboard_sender(&self) -> tokio::sync::watch::Sender<core_proto::ClipboardMsg> {
        self.clipboard_out_tx.clone()
    }

    /// Take the client→host clipboard receiver — call exactly once. The compositor
    /// drains it and sets the Wayland selection so host apps can paste.
    pub fn take_clipboard_in_rx(
        &self,
    ) -> Option<mpsc::UnboundedReceiver<core_proto::ClipboardMsg>> {
        self.clipboard_in_rx.lock().ok().and_then(|mut g| g.take())
    }
}

/// Clonable audio side of a [`QuicStreamer`] (see [`QuicStreamer::audio_handle`]).
#[derive(Clone)]
pub struct AudioHandle {
    tx: mpsc::Sender<Vec<u8>>,
    connected: Arc<AtomicBool>,
}

impl AudioHandle {
    /// True while a client is connected — the host gates audio capture on this.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    /// Queue one Opus packet (sent as a datagram; dropped if no client, and
    /// drop-newest when the bounded queue is full so audio latency stays capped).
    pub fn send(&self, packet: Vec<u8>) {
        let _ = self.tx.try_send(packet);
    }
}

/// Lock-free decode/present latency taps for the Trireme player.
///
/// The network-centric stages (compose→encode→host-send on the host, plus the
/// RTT-derived `nexus` wire time) are measured entirely inside
/// [`run_video_client_with_input`], so they're always available. The
/// *client-local* decode + present stages, by contrast, are known only to the
/// player (play.rs) which owns the decoder + presentation timing. This struct is
/// the bridge: the player calls [`Self::record_decode_ms`]/[`Self::record_present_ms`]
/// as frames flow, and the bench reads back the **min** and **mean** when it
/// builds the report.
///
/// We accumulate (min + mean) rather than an EMA. The submit→emerge residency is
/// the **in-flight buffer**: ffmpeg's output is back-pressured by the 60 fps
/// render so each AU waits the full buffer depth, masking the GPU decode kernel —
/// even the min is the buffer floor, not pure decode. The bench therefore reports
/// `buffer_ms` = mean residency (buffer + decode, buffer-dominated) and folds the
/// unisolable decode kernel into it; `present_ms` = mean present. (`decode_min_ms`
/// is kept for diagnostics.)
///
/// Sums are kept as integer nanoseconds in `AtomicU64`s (so concurrent
/// `fetch_add`s never race an f32 read-modify-write), and the running decode min
/// is an `AtomicU32` of the sample's f32 bits — positive-float bit patterns order
/// the same as the floats, so `fetch_min` on the bits *is* the numeric min. Cheap
/// (relaxed atomics, no lock) so the player can update on the hot path. Wrap in an
/// `Arc` and pass `Some(arc)` to the client; `None` leaves those report fields at
/// 0.0. The bench calls [`Self::reset`] at window open so each run is independent.
#[derive(Debug)]
pub struct LatencyTaps {
    /// f32 bits of the minimum decode-ms seen since the last reset (+inf sentinel
    /// when none — its bit pattern is the largest finite-ordering value so the
    /// first real `fetch_min` always wins).
    decode_min_bits: std::sync::atomic::AtomicU32,
    /// Sum of decode-ms samples, as integer nanoseconds (ms × 1e6).
    decode_sum_ns: std::sync::atomic::AtomicU64,
    decode_count: std::sync::atomic::AtomicU32,
    /// Sum of present-ms samples, as integer nanoseconds.
    present_sum_ns: std::sync::atomic::AtomicU64,
    present_count: std::sync::atomic::AtomicU32,
    /// Host-announced video geometry, packed `(w<<32)|h` (0 = not yet learned).
    /// Set from the `DG_STREAM_GEOMETRY` datagram; read by the client's raw-frame
    /// reader to size NV12 frames to the real resolution. Deliberately NOT cleared
    /// by [`Self::reset`] — it's session geometry, not a per-bench-window stat.
    geometry: std::sync::atomic::AtomicU64,
    /// Host-announced stream colorimetry (issue #16): 0 = unknown (not yet
    /// announced), 1 = SDR, 2 = HDR. Set from the `DG_STREAM_HDR` datagram; read by
    /// the desktop player's decode chain to insert the HDR→SDR tonemap ONLY when the
    /// stream is truly HDR. The "unknown" state lets the client default to plain
    /// decode until the first announce (the crash-safe default). Like `geometry`,
    /// NOT cleared by [`Self::reset`] — it's session colorimetry, not a bench stat.
    stream_hdr: std::sync::atomic::AtomicU8,
    /// Host-announced cursor-capture state: 0 = unknown (not yet announced), 1 =
    /// FREE (absolute pointer over a menu/desktop), 2 = CAPTURED (the game grabbed
    /// the mouse for relative mouselook). Set from the `DG_CURSOR_MODE` datagram;
    /// read by the desktop player, which mirrors it into a real OS cursor grab so the
    /// client auto-locks during gameplay and frees in menus. The "unknown" state lets
    /// an older host (no announce) leave the client on manual grab only. Like
    /// `stream_hdr`, NOT cleared by [`Self::reset`] — it's session state, not a stat.
    cursor_captured: std::sync::atomic::AtomicU8,
}

impl Default for LatencyTaps {
    fn default() -> Self {
        Self {
            decode_min_bits: std::sync::atomic::AtomicU32::new(f32::INFINITY.to_bits()),
            decode_sum_ns: std::sync::atomic::AtomicU64::new(0),
            decode_count: std::sync::atomic::AtomicU32::new(0),
            present_sum_ns: std::sync::atomic::AtomicU64::new(0),
            present_count: std::sync::atomic::AtomicU32::new(0),
            geometry: std::sync::atomic::AtomicU64::new(0),
            stream_hdr: std::sync::atomic::AtomicU8::new(0),
            cursor_captured: std::sync::atomic::AtomicU8::new(0),
        }
    }
}

impl LatencyTaps {
    /// A fresh, empty tap set (decode-min seeded to +inf).
    pub fn new() -> Self {
        Self::default()
    }

    /// Zero every accumulator and reset the decode min back to +inf. The bench
    /// calls this when a window opens so a later run isn't contaminated by an
    /// earlier one.
    pub fn reset(&self) {
        self.decode_min_bits
            .store(f32::INFINITY.to_bits(), Ordering::Relaxed);
        self.decode_sum_ns.store(0, Ordering::Relaxed);
        self.decode_count.store(0, Ordering::Relaxed);
        self.present_sum_ns.store(0, Ordering::Relaxed);
        self.present_count.store(0, Ordering::Relaxed);
    }

    /// ms → integer nanoseconds (saturating, never negative).
    fn ms_to_ns(ms: f32) -> u64 {
        (ms.max(0.0) as f64 * 1.0e6) as u64
    }

    /// Record one decode-stage measurement (ms): time from handing the AU to the
    /// decoder until a frame comes out. Updates the min, sum, and count.
    pub fn record_decode_ms(&self, ms: f32) {
        // Min on the f32 bit pattern — valid because we only ever store finite
        // non-negative samples (clamped below), whose bit patterns order as ints.
        let bits = ms.max(0.0).to_bits();
        self.decode_min_bits.fetch_min(bits, Ordering::Relaxed);
        self.decode_sum_ns
            .fetch_add(Self::ms_to_ns(ms), Ordering::Relaxed);
        self.decode_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one present-stage measurement (ms): time from decode output until
    /// the frame is on screen. Updates the sum and count.
    pub fn record_present_ms(&self, ms: f32) {
        self.present_sum_ns
            .fetch_add(Self::ms_to_ns(ms), Ordering::Relaxed);
        self.present_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Minimum decode-stage time in ms since the last reset (0.0 if never
    /// recorded). This is the ≈pure-decode floor.
    pub fn decode_min_ms(&self) -> f32 {
        if self.decode_count.load(Ordering::Relaxed) == 0 {
            return 0.0;
        }
        f32::from_bits(self.decode_min_bits.load(Ordering::Relaxed))
    }

    /// Mean decode-stage time in ms since the last reset (0.0 if never recorded).
    pub fn decode_mean_ms(&self) -> f32 {
        let n = self.decode_count.load(Ordering::Relaxed);
        if n == 0 {
            return 0.0;
        }
        let sum_ns = self.decode_sum_ns.load(Ordering::Relaxed);
        (sum_ns as f64 / n as f64 / 1.0e6) as f32
    }

    /// Mean present-stage time in ms since the last reset (0.0 if never recorded).
    pub fn present_mean_ms(&self) -> f32 {
        let n = self.present_count.load(Ordering::Relaxed);
        if n == 0 {
            return 0.0;
        }
        let sum_ns = self.present_sum_ns.load(Ordering::Relaxed);
        (sum_ns as f64 / n as f64 / 1.0e6) as f32
    }

    /// Record the host-announced stream geometry (decoded from a
    /// `DG_STREAM_GEOMETRY` datagram). `w`/`h` of 0 is ignored.
    pub fn set_geometry(&self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        self.geometry
            .store(((w as u64) << 32) | h as u64, Ordering::Relaxed);
    }

    /// The host-announced stream geometry, or `None` until the first
    /// `DG_STREAM_GEOMETRY` arrives. The raw-frame reader sizes NV12 frames from
    /// this; falling back to a configured default only when it stays `None`.
    pub fn geometry(&self) -> Option<(u32, u32)> {
        let v = self.geometry.load(Ordering::Relaxed);
        if v == 0 {
            None
        } else {
            Some(((v >> 32) as u32, (v & 0xffff_ffff) as u32))
        }
    }

    /// Record the host-announced stream colorimetry (decoded from a `DG_STREAM_HDR`
    /// datagram): `true` = HDR, `false` = SDR. Stored as 2 = HDR / 1 = SDR so the
    /// still-unset state (0) stays distinguishable from a genuine SDR announce.
    pub fn set_stream_hdr(&self, hdr: bool) {
        self.stream_hdr
            .store(if hdr { 2 } else { 1 }, Ordering::Relaxed);
    }

    /// The host-announced stream colorimetry, or `None` until the first
    /// `DG_STREAM_HDR` arrives (issue #16). `Some(true)` = HDR, `Some(false)` = SDR.
    /// The desktop player inserts its HDR→SDR tonemap only for `Some(true)` — plain
    /// decode for `Some(false)` or `None` (the crash-safe default: forcing the
    /// tonemap onto an SDR stream fails zscale and blacks the window, whereas
    /// plain-decoding a genuinely-HDR stream merely looks washed-out until the next
    /// rebuild).
    pub fn stream_hdr(&self) -> Option<bool> {
        match self.stream_hdr.load(Ordering::Relaxed) {
            2 => Some(true),
            1 => Some(false),
            _ => None,
        }
    }

    /// Record the host-announced cursor-capture state (decoded from a
    /// `DG_CURSOR_MODE` datagram): `true` = CAPTURED (game grabbed the mouse,
    /// relative mouselook), `false` = FREE (absolute pointer over a menu/desktop).
    /// Stored as 2 = CAPTURED / 1 = FREE so the still-unset state (0) stays
    /// distinguishable from a genuine FREE announce (an older host that never
    /// announces leaves it at 0, so the client stays on manual grab only).
    pub fn set_cursor_captured(&self, captured: bool) {
        self.cursor_captured
            .store(if captured { 2 } else { 1 }, Ordering::Relaxed);
    }

    /// The host-announced cursor-capture state, or `None` until the first
    /// `DG_CURSOR_MODE` arrives. `Some(true)` = CAPTURED (relative), `Some(false)` =
    /// FREE (absolute). The desktop player mirrors it into its OS cursor grab so the
    /// client auto-locks during gameplay and frees in menus; on `None` (an older host
    /// that never announces) it leaves the client's grab entirely to the manual
    /// hotkey — never auto-releasing, so today's behaviour is preserved.
    pub fn cursor_captured(&self) -> Option<bool> {
        match self.cursor_captured.load(Ordering::Relaxed) {
            2 => Some(true),
            1 => Some(false),
            _ => None,
        }
    }
}

// Datagram type tags (first byte). Audio and gyro both ride unreliable
// datagrams over the one connection, so a single client read loop dispatches by
// this tag — without it the two would race the same datagram queue.
const DG_AUDIO: u8 = 0x00;
const DG_GYRO: u8 = 0x01;
// Latency-benchmark datagrams (see `core_proto::latency`). All four are inert in
// normal streaming: the host only emits DG_FRAME_META during a bench window
// (driven by `send_with_meta`), and the client only initiates the clock
// exchange + report once it sees that meta start. So an idle session never
// touches these paths.
//   DG_CLOCK_PING  — client → host: NTP-style probe (initiator send t0).
//   DG_CLOCK_PONG  — host → client: responder echo (recv t1, send t2).
//   DG_FRAME_META  — host → client: per-frame capture/encode/send stamps.
//   DG_LATENCY_REPORT — client → host: the final per-stage attribution.
const DG_CLOCK_PING: u8 = 0x02;
const DG_CLOCK_PONG: u8 = 0x03;
const DG_FRAME_META: u8 = 0x04;
const DG_LATENCY_REPORT: u8 = 0x05;
// Host → client, additive (older clients ignore unknown tags, so no protocol
// break for already-deployed APKs): the encoder's current output geometry as
// `w:u32 BE || h:u32 BE`. Sent at stream open and re-sent on change / ~1×/s, so
// a client can size its raw-frame reader to the TRUE resolution instead of
// guessing (the cause of the green/sheared NV12 on a non-1080p stream). The
// resolution actuator re-sends it whenever it live-changes the encode size.
const DG_STREAM_GEOMETRY: u8 = 0x06;
// Client → host, additive: periodic quality-of-service report (delivered fps +
// decode time) so the host can adapt its send rate to the actual path, not just
// its own encode cadence. See `core_proto::latency::ClientQos`.
const DG_CLIENT_QOS: u8 = 0x07;
// Client→host: "I lost a reference / my decode looks corrupt — send an IDR NOW."
// Tiny, unreliable (a lost request just means the client re-asks on its next bad
// frame); orthogonal to the host self-trigger on queue drops, so a client that
// never sends it (older APK) still recovers via the periodic/drop-driven IDR.
const DG_REQUEST_IDR: u8 = 0x08;
// Host→client link-health heartbeat (~2×/s). 1-byte payload: 1 = the host dropped
// video AUs in the last ~500ms window (link slower than the encode rate → the
// client lights a "degraded" indicator), 0 = clean. Additive: older clients
// ignore the unknown tag. Unreliable — a lost beat self-heals on the next tick.
const DG_LINK_HEALTH: u8 = 0x09;
// Client→host resolution preference (Phase 2): postcard `ClientResPref`
// (target_h, floor_h) for the adaptive ladder. Sent ~1×/s (continuous, like
// DG_CLIENT_QOS) so a lost one self-heals; the host stores it and the compositor
// applies it idempotently. Additive: older hosts ignore the unknown tag.
const DG_CLIENT_RESPREF: u8 = 0x0a;
// Client→host bitrate preference (Phase 3): postcard `ClientBitratePref`
// (min_kbps, max_kbps; max=0=uncapped) — the primary adaptive lever. Same
// continuous ~1×/s self-healing send as DG_CLIENT_RESPREF. Additive.
const DG_CLIENT_BITRATEPREF: u8 = 0x0b;
// Client→host framerate preference (Phase 3): postcard `ClientFpsPref`
// (min_fps, max_fps; max=0=Auto/panel) — caps the encode pace + adaptation floor.
// Same continuous ~1×/s self-healing send as DG_CLIENT_RESPREF. Additive.
const DG_CLIENT_FPSPREF: u8 = 0x0c;
// Host→client, additive (older clients ignore unknown tags): the colorimetry of
// the CURRENT encode as a 1-byte flag — `1` = HDR (10-bit HEVC Main10, BT.2020/PQ),
// `0` = SDR (8-bit BT.709). Sent at stream open and re-sent on change / ~1×/s, on
// the same cadence + burst as DG_STREAM_GEOMETRY. The client conditions its decode
// chain on it: the HDR→SDR tonemap filter runs ONLY when this says HDR, so a client
// with the HDR capability opted in (TRIREME_HDR) doesn't force the tonemap onto a
// genuinely-SDR stream (which fails zscale `no path between colorspaces` → black
// window; issue #16). Modeled on DG_STREAM_GEOMETRY; room to grow (issue #15) but
// minimal now — 1 byte. A lost datagram self-heals on the ~1×/s resend.
const DG_STREAM_HDR: u8 = 0x0d;
// Host→client, additive (older clients ignore unknown tags): the host GAME's own
// cursor-capture state as a 1-byte flag — `1` = CAPTURED (the game grabbed/hid the
// mouse for relative mouselook), `0` = FREE (a normal absolute pointer over a
// menu/desktop). Sent at stream open and re-sent on change / ~1×/s, on the same
// cadence + burst as DG_STREAM_HDR. The desktop client mirrors it into a real OS
// cursor grab so a windowed FPS auto-locks the client cursor (relative) during
// gameplay and frees it (absolute) in menus — the native windowed-game feel with no
// manual toggle. Modeled on DG_STREAM_HDR; 1 byte. A lost datagram self-heals on the
// ~1×/s resend; an older host never sends it, so the client stays on manual grab.
const DG_CURSOR_MODE: u8 = 0x0e;
// Host→client, additive: bandwidth-probe PADDING (issue #40). Content-free
// filler the host ramps for ~2s at session start so the client's delivered-
// goodput measurement (ClientQosV2.recv_kbps) reads the path's real capacity
// instead of the app-limited video rate. Congestion-controlled like every QUIC
// datagram, so the wire never carries more than BBR allows — excess offered
// load dies in the host's datagram send buffer, not on the network. Clients
// need no explicit handling: the byte counter sees the datagram before the tag
// dispatch, and older clients ignore the unknown tag entirely (they just also
// don't report recv_kbps, so a host never probes them — see ClientQosV2).
const DG_PROBE_PAD: u8 = 0x0f;

/// Per-connection state for the connect-time bandwidth probe (issue #40),
/// shared between the compositor-facing [`QuicStreamer`] API, the per-connection
/// pad-sender task, and the datagram task that folds client QoS into the
/// measurement. Generations make the API race-free: `start_probe` bumps
/// `req_gen`; the pad task runs one ramp per generation and publishes into
/// `measured_kbps`/`done_gen`; `probe_result` is `Some` only when the done
/// generation has caught up.
#[derive(Default)]
struct ProbeShared {
    /// Bumped by [`QuicStreamer::start_probe`]; each bump requests one ramp.
    req_gen: std::sync::atomic::AtomicU32,
    /// Offered-load ceiling (kbps) for the pending ramp.
    ceiling_kbps: std::sync::atomic::AtomicU32,
    /// Ramp duration (ms) for the pending ramp.
    duration_ms: std::sync::atomic::AtomicU32,
    /// Generation whose result is published in `measured_kbps`.
    done_gen: std::sync::atomic::AtomicU32,
    /// Peak client-reported delivered goodput (kbps) over the finished probe
    /// window; 0 = no measurement (old client / no samples).
    measured_kbps: std::sync::atomic::AtomicU32,
    /// Measurement-window end (unix ms): QoS samples arriving before this
    /// fold into `pending_max_kbps`.
    window_until_ms: std::sync::atomic::AtomicU64,
    /// Running max of recv_kbps samples inside the current window.
    pending_max_kbps: std::sync::atomic::AtomicU32,
    /// Latest client transport hint (core_proto TRANSPORT_*).
    transport: std::sync::atomic::AtomicU8,
    /// True once any v2 QoS with a non-zero recv_kbps arrived — i.e. this
    /// client can measure goodput, so a probe is worth running.
    goodput_seen: AtomicBool,
}

impl ProbeShared {
    /// Reset per connection so a reconnecting client starts with a clean slate
    /// (any in-flight ramp is cancelled by aligning the generations).
    fn reset(&self) {
        let g = self.req_gen.load(Ordering::Relaxed);
        self.done_gen.store(g, Ordering::Relaxed);
        self.measured_kbps.store(0, Ordering::Relaxed);
        self.window_until_ms.store(0, Ordering::Relaxed);
        self.pending_max_kbps.store(0, Ordering::Relaxed);
        self.transport.store(0, Ordering::Relaxed);
        self.goodput_seen.store(false, Ordering::Relaxed);
    }
}

/// UNIX-epoch nanoseconds from the wall clock. Used for every timestamp that
/// crosses the link (ClockPing/Pong t0..t3, FrameStageMeta stamps): a monotonic
/// `Instant` has no shared epoch across two machines, so subtracting the host's
/// `send_ns` from the client's `recv_ns` would be garbage unless both ends read
/// the *same* clock domain. SystemTime gives that shared (UNIX) epoch. Per-stage
/// *deltas* measured entirely on one machine may still use `Instant`.
fn unix_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Prefix a datagram payload with its type tag.
fn tagged_datagram(tag: u8, payload: &[u8]) -> bytes::Bytes {
    let mut d = Vec::with_capacity(payload.len() + 1);
    d.push(tag);
    d.extend_from_slice(payload);
    bytes::Bytes::from(d)
}

/// Run the mutual-auth handshake on a freshly-accepted connection, bounded by a
/// 5s timeout. Returns the client's verified pubkey (`None` = anonymous), its
/// `wants_gyro` and `wants_hdr` flags, and the requested `StreamMode`, or `Err`
/// on any failure (the caller closes the connection
/// and moves on). Shared verbatim by the single-client accept path and the
/// mirror keep-N path so the wire handshake is byte-identical in both — see the
/// inline doc at the original single-client call site for the v1/v2 wire detail.
async fn authenticate(
    conn: &quinn::Connection,
    identity: &core_crypto::Identity,
) -> Result<(Option<[u8; 32]>, bool, bool, core_proto::StreamMode)> {
    let auth_fut = async {
        use rand::RngCore;
        let (mut auth_send, mut auth_recv) = conn.accept_bi().await?;
        let mut head = [0u8; 4];
        auth_recv.read_exact(&mut head).await?;
        let (wants_gyro_flag, wants_hdr_flag, mut client_nonce);
        let mut wire_version: u32 = 1; // v1 = no magic prefix (none in-tree)
        if head == AUTH_PROTO_MAGIC {
            let mut ver_buf = [0u8; 4];
            auth_recv.read_exact(&mut ver_buf).await?;
            wire_version = u32::from_be_bytes(ver_buf);
            // Accept the whole supported range so older v2 clients (no StreamMode
            // block) keep working against this newer host.
            if !(AUTH_PROTO_VERSION_MIN..=AUTH_PROTO_VERSION).contains(&wire_version) {
                anyhow::bail!("unsupported auth wire version {wire_version}");
            }
            client_nonce = [0u8; 32];
            auth_recv.read_exact(&mut client_nonce).await?;
        } else {
            client_nonce = [0u8; 32];
            client_nonce[..4].copy_from_slice(&head);
            auth_recv.read_exact(&mut client_nonce[4..]).await?;
        }
        let mut client_pub = [0u8; 32];
        auth_recv.read_exact(&mut client_pub).await?;
        let mut stream_mode = core_proto::StreamMode::default();
        if head == AUTH_PROTO_MAGIC {
            let mut g = [0u8; 1];
            auth_recv.read_exact(&mut g).await?;
            wants_gyro_flag = g[0] != 0;
            // v3+: a 12-byte requested-mode block (w, h, refresh_mhz) follows the
            // gyro byte. The host caps fps to its refresh and (Tier-3) targets w×h.
            if wire_version >= 3 {
                let mut mode_buf = [0u8; 12];
                auth_recv.read_exact(&mut mode_buf).await?;
                stream_mode = core_proto::StreamMode::from_bytes(&mode_buf);
            }
            // v4+: a trailing 1-byte wants_hdr flag after the mode block. The
            // client advertises it wants a 10-bit HDR stream; the host only
            // engages HDR when this AND host capability AND app-declared HDR agree.
            if wire_version >= 4 {
                let mut h = [0u8; 1];
                auth_recv.read_exact(&mut h).await?;
                wants_hdr_flag = h[0] != 0;
            } else {
                wants_hdr_flag = false;
            }
        } else {
            wants_gyro_flag = false;
            wants_hdr_flag = false;
        }

        let mut exporter = [0u8; 32];
        conn.export_keying_material(&mut exporter, VIDEO_AUTH_EXPORTER_LABEL, &[])
            .map_err(|e| anyhow::anyhow!("export_keying_material: {e:?}"))?;

        let sig = core_crypto::sign_video_auth(identity, &client_nonce, &exporter);
        let mut host_nonce = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut host_nonce);
        auth_send.write_all(&sig).await?;
        auth_send.write_all(&identity.public_bytes()).await?;
        auth_send.write_all(&host_nonce).await?;
        auth_send.finish().ok();

        let mut client_sig = [0u8; 64];
        auth_recv.read_exact(&mut client_sig).await?;
        if client_pub == [0u8; 32] {
            // anonymous — presented no identity
            Ok((None, wants_gyro_flag, wants_hdr_flag, stream_mode))
        } else {
            core_crypto::verify_client_auth(&host_nonce, &exporter, &client_sig, &client_pub)
                .map_err(|e| anyhow::anyhow!("client auth: {e}"))?;
            Ok((
                Some(client_pub),
                wants_gyro_flag,
                wants_hdr_flag,
                stream_mode,
            ))
        }
    };
    // Bound auth: a peer that completes the QUIC handshake but never sends the
    // auth frame (a half-dead pre-empting client, a port probe) must not hang
    // the accept loop — it fails out in 5s.
    match tokio::time::timeout(std::time::Duration::from_secs(5), auth_fut).await {
        Ok(r) => r,
        Err(_) => Err(anyhow::anyhow!("auth timed out")),
    }
}

/// Hard cap on simultaneous mirrored clients. Mirror is ONE encode, so this
/// bounds per-connection task cost + the N× unicast UPLINK (mirror saves GPU
/// encode, not wire bytes) — a little headroom over the ~3-NVENC-session bar.
const MIRROR_MAX_CLIENTS: usize = 4;
/// Close code for a client rejected because the mirror is full — distinct from
/// PREEMPT (0x9) so the client surfaces "host full", not "another device active".
const CAPACITY_CLOSE_CODE: u32 = 0xA;
/// Close code when the host switches stream mode (mirror↔single): the client
/// treats it as a benign reconnect, NOT a pre-emption.
const MODE_SWITCH_CLOSE_CODE: u32 = 0xB;
/// Close code the CLIENT sends when its own input send path is genuinely stuck
/// (transition cap or age bound, issue #13): tears the whole session down so the
/// client's normal StreamEnded → reconnect path fires with a fresh connection.
/// Deliberately NOT one of the host's special stop codes (RESERVED / UNPAIRED),
/// so `app_close_error` maps it to `None` = an ordinary drop = fast reconnect.
const INPUT_STUCK_CLOSE_CODE: u32 = 0xE;

/// Per-device input gate: the set of client pubkeys whose input forwarding is
/// DISABLED (default empty → every device's input reaches the seat, today's
/// behavior). A plain process-global because the host is one process with one
/// smithay seat; the compositor's control handler flips entries via
/// [`set_input_enabled`] when the panel toggles a device card. Read by the
/// per-connection input tasks ([`input_allowed`]) just before forwarding. This
/// REPLACES the mirror primary-only heuristic with the user's explicit choice.
static INPUT_DISABLED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<[u8; 32]>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// Enable/disable input forwarding for one client pubkey, live. Called from the
/// compositor control handler on a panel `SetInputEnabled`.
pub fn set_input_enabled(pubkey: [u8; 32], enabled: bool) {
    if let Ok(mut s) = INPUT_DISABLED.lock() {
        if enabled {
            s.remove(&pubkey);
        } else {
            s.insert(pubkey);
        }
    }
}

/// Whether this connection's input should reach the seat. Anonymous clients
/// (no pinned pubkey — not representable as a canvas card) are always allowed;
/// a pinned device is allowed unless the panel disabled it.
fn input_allowed(pubkey: Option<[u8; 32]>) -> bool {
    match pubkey {
        None => true,
        Some(pk) => INPUT_DISABLED
            .lock()
            .map(|s| !s.contains(&pk))
            .unwrap_or(true),
    }
}

/// The host's PRIMARY client (issue #19), or `None` when no primary is set. A
/// process-global for the same reason as [`INPUT_DISABLED`]: the host is one
/// process, and the control handler (panel `SetPrimaryPeer`) must reach the
/// streaming loop's pre-emption policy without re-plumbing every streamer.
/// Host binaries seed it from the persisted registry at startup
/// (`host-core::pair::sync_primary_to_transport`) and update it live.
static PRIMARY_PEER: std::sync::LazyLock<std::sync::Mutex<Option<[u8; 32]>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(None));

/// Set (or clear) the primary client. While the primary holds the single
/// streaming slot, a different client's connect is refused with
/// [`RESERVED_CLOSE_CODE`] instead of pre-empting; every other pairing of
/// incumbent/challenger keeps the validated newest-wins pre-empt.
pub fn set_primary_peer(pubkey: Option<[u8; 32]>) {
    if let Ok(mut g) = PRIMARY_PEER.lock() {
        *g = pubkey;
    }
}

/// The current primary client's pubkey, if one is set.
pub fn primary_peer() -> Option<[u8; 32]> {
    PRIMARY_PEER.lock().ok().and_then(|g| *g)
}

/// The current primary client's pubkey as lowercase hex (the `peers.toml` /
/// `ClientStatus` shape), for the hosts' status publishers.
pub fn primary_peer_hex() -> Option<String> {
    primary_peer().map(|pk| pk.iter().map(|b| format!("{b:02x}")).collect())
}

/// One tracked live session: the peer's verified pubkey + its connection.
type LiveSession = ([u8; 32], quinn::Connection);

/// Registry of live, authorized streaming sessions by verified pubkey, so the
/// control plane can close a specific peer's session (panel `ForgetPeer`)
/// without a handle on the streamer that owns it. Entries are added once a
/// session is admitted (single-client and mirror paths both) and removed on
/// session end; [`disconnect_peer`] also prunes already-closed stragglers.
/// Anonymous sessions (no pubkey) aren't tracked — there's nothing to address.
static LIVE_SESSIONS: std::sync::LazyLock<std::sync::Mutex<Vec<LiveSession>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// Track an admitted session so [`disconnect_peer`] can reach it. No-op for
/// anonymous clients.
fn register_session(pubkey: Option<[u8; 32]>, conn: &quinn::Connection) {
    let Some(pk) = pubkey else { return };
    if let Ok(mut s) = LIVE_SESSIONS.lock() {
        // Prune corpses while we're here so the vec can't grow unboundedly
        // across many reconnects even if an unregister was ever missed.
        s.retain(|(_, c)| c.close_reason().is_none());
        s.push((pk, conn.clone()));
    }
}

/// Drop a session from the registry (by the exact connection, so a peer's
/// *newer* session is never evicted by its older one's teardown).
fn unregister_session(conn: &quinn::Connection) {
    if let Ok(mut s) = LIVE_SESSIONS.lock() {
        s.retain(|(_, c)| c.stable_id() != conn.stable_id());
    }
}

/// Client side: map a deliberate application close from the host (preempt /
/// reserved-by-primary / unpaired) to its stable, matchable error. `None` for
/// an ordinary drop, an idle timeout, or any other code — those take the
/// caller's normal reconnect path.
fn app_close_error(conn: &quinn::Connection) -> Option<anyhow::Error> {
    let quinn::ConnectionError::ApplicationClosed(ac) = conn.close_reason()? else {
        return None;
    };
    match u64::from(ac.error_code) {
        x if x == PREEMPT_CLOSE_CODE as u64 => Some(anyhow::anyhow!("preempted by another client")),
        x if x == RESERVED_CLOSE_CODE as u64 => Some(anyhow::anyhow!(RESERVED_BY_PRIMARY_MSG)),
        x if x == UNPAIRED_CLOSE_CODE as u64 => Some(anyhow::anyhow!(UNPAIRED_MSG)),
        _ => None,
    }
}

/// Close every live streaming session belonging to `pubkey` with
/// [`UNPAIRED_CLOSE_CODE`] — the "forget peer" teardown. Returns how many live
/// sessions were closed (0 = the peer wasn't connected). The streaming loop
/// observes the close like any peer drop and frees the slot.
pub fn disconnect_peer(pubkey: [u8; 32]) -> usize {
    let Ok(mut s) = LIVE_SESSIONS.lock() else {
        return 0;
    };
    let mut closed = 0;
    s.retain(|(pk, c)| {
        if *pk == pubkey {
            if c.close_reason().is_none() {
                c.close(UNPAIRED_CLOSE_CODE.into(), b"unpaired");
                closed += 1;
            }
            return false;
        }
        c.close_reason().is_none()
    });
    closed
}

/// Removes a mirrored client's registry slot on ANY actor exit (closed / send
/// error / panic), so a dead client can't wedge a slot or hold the connected
/// gate. Clears `connected`/`wants_gyro` when the last client leaves.
struct SlotGuard {
    clients: Arc<std::sync::Mutex<Vec<ClientSlot>>>,
    connected: Arc<AtomicBool>,
    wants_gyro: Arc<AtomicBool>,
    wants_hdr: Arc<AtomicBool>,
    id: u64,
    /// This client's connection, so the drop also clears its LIVE_SESSIONS
    /// entry (the ForgetPeer reach — see [`register_session`]).
    conn: quinn::Connection,
}
impl Drop for SlotGuard {
    fn drop(&mut self) {
        unregister_session(&self.conn);
        if let Ok(mut slots) = self.clients.lock() {
            let before = slots.len();
            slots.retain(|s| s.id != self.id);
            // Only clear the shared gate if WE actually removed our own slot AND
            // the registry is now empty. A straggler whose slot was already
            // cleared by the mode-switch teardown (removed == false) must NOT
            // clobber a `connected` flag the single-client path has since
            // re-armed — otherwise it blacks out the new single stream.
            let removed = slots.len() < before;
            if removed && slots.is_empty() {
                self.connected.store(false, Ordering::Release);
                self.wants_gyro.store(false, Ordering::Release);
                self.wants_hdr.store(false, Ordering::Release);
            }
        }
    }
}

/// Send the encoder geometry to one client as a `DG_STREAM_GEOMETRY` datagram —
/// identical wire shape to the single-client announce (w_be ‖ h_be, 8 bytes).
fn announce_geom_to(g: u64, conn: &quinn::Connection) {
    if g != 0 {
        let payload = [((g >> 32) as u32).to_be_bytes(), (g as u32).to_be_bytes()].concat();
        let _ = conn.send_datagram(tagged_datagram(DG_STREAM_GEOMETRY, &payload));
    }
}

/// Send the current colorimetry to one client as a `DG_STREAM_HDR` datagram — a
/// single-byte flag (`1` = HDR, `0` = SDR). The caller passes `state == 2` and only
/// calls this once colorimetry is DETERMINED (the host `hdr` tri-state is past 0);
/// while undetermined the caller sends nothing and the client plain-decodes (issue #28).
fn announce_hdr_to(hdr: bool, conn: &quinn::Connection) {
    let _ = conn.send_datagram(tagged_datagram(DG_STREAM_HDR, &[hdr as u8]));
}

/// Send the host game's current cursor-capture state to one client as a
/// `DG_CURSOR_MODE` datagram — a single-byte flag (`1` = CAPTURED / relative, `0` =
/// FREE / absolute). Always sent (FREE is the honest default), so the client learns
/// the state promptly and mirrors it into its own cursor grab.
fn announce_cursor_to(captured: bool, conn: &quinn::Connection) {
    let _ = conn.send_datagram(tagged_datagram(DG_CURSOR_MODE, &[captured as u8]));
}

/// Video queue depth (frames) — same `PHAROS_QUEUE_FRAMES` knob as the single
/// path, applied per mirrored client.
fn video_qdepth() -> usize {
    std::env::var("PHAROS_QUEUE_FRAMES")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(8)
}

/// MIRROR keep-N fan-out loop. Borrows `&mut rx` (the SAME producer channel the
/// single path uses — never moved) and `select!`s over: accept (auth + admit a
/// new client in a spawned task so a slow handshake can't stall video), the
/// encoder channel (fan each AU to every client's bounded queue, drop-newest,
/// never blocking), and audio/gyro datagrams. Returns when the mode flag flips
/// back to single (closing all clients with `MODE_SWITCH_CLOSE_CODE`), so the
/// caller falls back into the validated single-client loop.
#[allow(clippy::too_many_arguments)]
async fn run_mirror_loop(
    endpoint: &Endpoint,
    rx: &mut mpsc::Receiver<VideoChunk>,
    audio_rx: &mut mpsc::Receiver<Vec<u8>>,
    gyro_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    mirror: &Arc<AtomicBool>,
    clients: &Arc<std::sync::Mutex<Vec<ClientSlot>>>,
    identity: &Arc<core_crypto::Identity>,
    authorizer: &ClientAuthorizer,
    first_conn: quinn::Connection,
    first_id: Option<[u8; 32]>,
    first_wants_gyro: bool,
    first_wants_hdr: bool,
    connected: &Arc<AtomicBool>,
    wants_gyro: &Arc<AtomicBool>,
    wants_hdr: &Arc<AtomicBool>,
    geom: &Arc<std::sync::atomic::AtomicU64>,
    hdr: &Arc<std::sync::atomic::AtomicU8>,
    cursor_captured: &Arc<AtomicBool>,
    input_tx: &mpsc::UnboundedSender<core_proto::InputEvent>,
) {
    tracing::info!("mirror: entering keep-N fan-out loop");
    // Drop any single-session backlog so newcomers don't decode stale AUs.
    while rx.try_recv().is_ok() {}
    while audio_rx.try_recv().is_ok() {}

    let next_id = Arc::new(std::sync::atomic::AtomicU64::new(0));
    // Admit the first (already-authed) connection that brought us into mirror.
    admit_mirror_client(
        first_conn,
        first_id,
        first_wants_gyro,
        first_wants_hdr,
        clients,
        &next_id,
        connected,
        wants_gyro,
        wants_hdr,
        geom,
        hdr,
        cursor_captured,
        input_tx,
        mirror,
    );

    loop {
        // Mode flipped back to single → tear down all mirrored clients (a benign
        // reconnect for them) and return so the caller re-enters the single loop.
        if !mirror.load(Ordering::Acquire) {
            let mut slots = match clients.lock() {
                Ok(s) => s,
                Err(_) => return,
            };
            tracing::info!("mirror: mode off, closing {} client(s)", slots.len());
            for s in slots.iter() {
                s.conn.close(MODE_SWITCH_CLOSE_CODE.into(), b"mode switch");
            }
            slots.clear();
            drop(slots);
            connected.store(false, Ordering::Release);
            wants_gyro.store(false, Ordering::Release);
            wants_hdr.store(false, Ordering::Release);
            while rx.try_recv().is_ok() {}
            return;
        }

        tokio::select! {
            biased;
            // A new client connects → auth + admit in a spawned task so a slow
            // 5s handshake never stalls the video fan-out below.
            inc = endpoint.accept() => {
                if let Some(inc) = inc {
                    let clients = clients.clone();
                    let next_id = next_id.clone();
                    let identity = identity.clone();
                    let authorizer = authorizer.clone();
                    let connected = connected.clone();
                    let wants_gyro = wants_gyro.clone();
                    let wants_hdr = wants_hdr.clone();
                    let geom = geom.clone();
                    let hdr = hdr.clone();
                    let cursor_captured = cursor_captured.clone();
                    let input_tx = input_tx.clone();
                    let mirror = mirror.clone();
                    tokio::spawn(async move {
                        let conn = match inc.await {
                            Ok(c) => c,
                            Err(e) => { tracing::warn!("mirror incoming: {e:?}"); return; }
                        };
                        // Cheap fast-reject before the 5s auth (the authoritative
                        // capacity + mode gate is under the lock in admit).
                        if clients.lock().map(|c| c.len()).unwrap_or(usize::MAX) >= MIRROR_MAX_CLIENTS {
                            tracing::warn!("mirror full ({MIRROR_MAX_CLIENTS}); rejecting {}", conn.remote_address());
                            conn.close(CAPACITY_CLOSE_CODE.into(), b"host full");
                            return;
                        }
                        let (cid, cwg, cwh, _cmode) = match authenticate(&conn, &identity).await {
                            Ok(v) => v,
                            Err(e) => { tracing::warn!("mirror auth failed: {e:?}"); conn.close(0u32.into(), b"auth"); return; }
                        };
                        if !authorizer(cid) {
                            tracing::warn!("mirror client rejected (not paired)");
                            conn.close(0u32.into(), b"unauthorized");
                            return;
                        }
                        // Mode may have flipped off during the auth await — early
                        // reject so we don't resurrect a slot; admit re-checks under
                        // the lock too (belt-and-braces against the teardown race).
                        if !mirror.load(Ordering::Acquire) {
                            conn.close(MODE_SWITCH_CLOSE_CODE.into(), b"mode switch");
                            return;
                        }
                        admit_mirror_client(conn, cid, cwg, cwh, &clients, &next_id, &connected, &wants_gyro, &wants_hdr, &geom, &hdr, &cursor_captured, &input_tx, &mirror);
                    });
                }
            }
            // A fresh encoded AU → fan to every client's bounded queue. Drop-newest
            // on full (a slow client sheds frames for ITSELF; ≤1s IDR resyncs it);
            // try_send never awaits, so one stalled client can't block the others.
            chunk = rx.recv() => {
                let Some(chunk) = chunk else { return; }; // producer gone (shutdown)
                if let Ok(slots) = clients.lock() {
                    for s in slots.iter() {
                        // Same keyframe-protecting admission as the single-client
                        // queue: a P-frame is dropped-newest when this client's
                        // queue is full, but an IDR may use the KF_RESERVE slots so
                        // a momentarily slow client never loses the one frame that
                        // resyncs it (queue sized video_qdepth()+KF_RESERVE below).
                        let free = s.tx.capacity();
                        let admit = if chunk.keyframe { free > 0 } else { free > KF_RESERVE };
                        if admit {
                            let _ = s.tx.try_send(chunk.clone());
                        }
                    }
                }
            }
            // Audio → fan as datagrams to ALL clients (output; small + unreliable).
            pkt = audio_rx.recv() => {
                if let Some(pkt) = pkt {
                    if let Ok(slots) = clients.lock() {
                        for s in slots.iter() {
                            let _ = s.conn.send_datagram(tagged_datagram(DG_AUDIO, &pkt));
                        }
                    }
                }
            }
            // Gyro telemetry → PRIMARY only (it's the calibrating client's).
            pkt = gyro_rx.recv() => {
                if let Some(pkt) = pkt {
                    if let Ok(slots) = clients.lock() {
                        if let Some(p) = slots.iter().min_by_key(|s| s.id) {
                            let _ = p.conn.send_datagram(tagged_datagram(DG_GYRO, &pkt));
                        }
                    }
                }
            }
            // Periodic wake so the mode-flip check above runs even when idle.
            _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {}
        }
    }
}

/// Register one authenticated connection as a mirrored client and spawn its
/// per-connection actor (drains its bounded queue onto a uni stream, measures
/// fps, RAII-deregisters on exit) + its input task (forwards events only while
/// PRIMARY). The dispatcher in [`run_mirror_loop`] feeds the queue.
#[allow(clippy::too_many_arguments)]
fn admit_mirror_client(
    conn: quinn::Connection,
    client_id: Option<[u8; 32]>,
    client_wants_gyro: bool,
    client_wants_hdr: bool,
    clients: &Arc<std::sync::Mutex<Vec<ClientSlot>>>,
    next_id: &Arc<std::sync::atomic::AtomicU64>,
    connected: &Arc<AtomicBool>,
    wants_gyro: &Arc<AtomicBool>,
    wants_hdr: &Arc<AtomicBool>,
    geom: &Arc<std::sync::atomic::AtomicU64>,
    hdr: &Arc<std::sync::atomic::AtomicU8>,
    cursor_captured: &Arc<AtomicBool>,
    input_tx: &mpsc::UnboundedSender<core_proto::InputEvent>,
    mirror: &Arc<AtomicBool>,
) {
    let id = next_id.fetch_add(1, Ordering::Relaxed);
    let (slot_tx, mut slot_rx) = mpsc::channel::<VideoChunk>(video_qdepth() + KF_RESERVE);
    let fps_ewma = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let frames_sent = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let addr = conn.remote_address();
    // Authoritative admission: the capacity bound AND the mode re-check happen
    // UNDER the same lock as the push (and the same lock the mode-off teardown
    // takes), so a burst of concurrent admit tasks can't overshoot
    // MIRROR_MAX_CLIENTS (TOCTOU), and an admit whose 5s auth straddled a
    // mirror→single flip can't resurrect a phantom slot. The cheap pre-auth
    // check in run_mirror_loop stays a fast-reject; this is the real gate.
    {
        let mut slots = match clients.lock() {
            Ok(s) => s,
            Err(_) => return,
        };
        if !mirror.load(Ordering::Acquire) {
            drop(slots);
            conn.close(MODE_SWITCH_CLOSE_CODE.into(), b"mode switch");
            return;
        }
        if slots.len() >= MIRROR_MAX_CLIENTS {
            drop(slots);
            tracing::warn!("mirror full ({MIRROR_MAX_CLIENTS}); rejecting {addr}");
            conn.close(CAPACITY_CLOSE_CODE.into(), b"host full");
            return;
        }
        slots.push(ClientSlot {
            id,
            peer_pubkey: client_id,
            peer_addr: addr,
            conn: conn.clone(),
            tx: slot_tx,
            fps_ewma: fps_ewma.clone(),
            frames_sent: frames_sent.clone(),
        });
        connected.store(true, Ordering::Release);
        if client_wants_gyro {
            wants_gyro.store(true, Ordering::Release);
        }
        // Mirror HDR: OR across clients (matching gyro). A mixed SDR/HDR mirror is
        // an edge case the session-switch gate (with wants_hdr) must still resolve
        // conservatively; here we only record that some client asked.
        if client_wants_hdr {
            wants_hdr.store(true, Ordering::Release);
        }
        tracing::info!("mirror: admitted {addr} (id={id}, total={})", slots.len());
    }
    // Reachable by the control plane's ForgetPeer teardown (issue #19); the
    // actor's SlotGuard unregisters it on any exit path.
    register_session(client_id, &conn);

    // Per-connection ACTOR: drains the bounded queue onto a uni stream.
    let clients_actor = clients.clone();
    let connected_actor = connected.clone();
    let wants_gyro_actor = wants_gyro.clone();
    let wants_hdr_actor = wants_hdr.clone();
    let geom_actor = geom.clone();
    let hdr_actor = hdr.clone();
    let cursor_captured_actor = cursor_captured.clone();
    let conn_actor = conn.clone();
    tokio::spawn(async move {
        // RAII: removes the slot + updates gates on ANY exit path.
        let _guard = SlotGuard {
            clients: clients_actor,
            connected: connected_actor,
            wants_gyro: wants_gyro_actor,
            wants_hdr: wants_hdr_actor,
            id,
            conn: conn_actor.clone(),
        };
        let mut send = match conn_actor.open_uni().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("mirror open_uni: {e:?}");
                return;
            }
        };
        let mut last_geom_m = geom_actor.load(Ordering::Relaxed);
        announce_geom_to(last_geom_m, &conn_actor);
        // Frames left in a dense re-send burst armed on a geometry change (mirror
        // shares one coded size across clients; the ladder can still move it).
        let mut geom_change_resend: u32 = 0;
        // Colorimetry announce (DG_STREAM_HDR), same cadence as the geometry above,
        // but as a tri-state (issue #28): 0 = undetermined → send nothing (the client
        // plain-decodes) until the host determines it; 1/2 = SDR/HDR. Re-sent on change
        // + ~1×/s. Mirror shares one encode, so all clients see the same flag.
        let mut last_hdr_m = hdr_actor.load(Ordering::Relaxed);
        if last_hdr_m != 0 {
            announce_hdr_to(last_hdr_m == 2, &conn_actor);
        }
        let mut hdr_change_resend: u32 = 0;
        // Cursor-mode announce (DG_CURSOR_MODE), same cadence as the HDR flag above:
        // sent at open so each mirror client's cursor grab defaults correctly, re-sent
        // on change + ~1×/s. Mirror shares one host game, so all clients see the same
        // capture state.
        let mut last_cursor_m = cursor_captured_actor.load(Ordering::Relaxed);
        announce_cursor_to(last_cursor_m, &conn_actor);
        let mut cursor_change_resend: u32 = 0;
        let mut n: u64 = 0;
        let mut win_start = tokio::time::Instant::now();
        let mut win_frames: u32 = 0;
        loop {
            tokio::select! {
                biased;
                _ = conn_actor.closed() => break,
                chunk = slot_rx.recv() => {
                    let Some(chunk) = chunk else { break };
                    let len_be = (chunk.au.len() as u32).to_be_bytes();
                    // Race the write against the connection closing so a dead-but-
                    // kept-alive client frees its own slot without wedging others.
                    let write_fut = async {
                        send.write_all(&len_be).await?;
                        send.write_all(&chunk.au).await?;
                        anyhow::Ok(())
                    };
                    tokio::select! {
                        biased;
                        _ = conn_actor.closed() => break,
                        r = write_fut => { if r.is_err() { break; } }
                    }
                    frames_sent.fetch_add(1, Ordering::Relaxed);
                    n = n.wrapping_add(1);
                    win_frames += 1;
                    // Re-announce geometry on a CHANGE (dense burst so a mirror
                    // client sees a ladder step promptly, not up to ~1s later) and
                    // ~1×/s as a steady backstop vs a lost datagram.
                    let g = geom_actor.load(Ordering::Relaxed);
                    if g != last_geom_m {
                        geom_change_resend = 8;
                        last_geom_m = g;
                    }
                    if geom_change_resend > 0 || n.is_multiple_of(60) {
                        announce_geom_to(g, &conn_actor);
                    }
                    geom_change_resend = geom_change_resend.saturating_sub(1);
                    // Colorimetry re-announce, mirroring the geometry logic above.
                    let h = hdr_actor.load(Ordering::Relaxed);
                    if h != last_hdr_m {
                        hdr_change_resend = 8;
                        last_hdr_m = h;
                    }
                    if h != 0 && (hdr_change_resend > 0 || n.is_multiple_of(60)) {
                        announce_hdr_to(h == 2, &conn_actor);
                    }
                    hdr_change_resend = hdr_change_resend.saturating_sub(1);
                    // Cursor-mode re-announce, mirroring the HDR logic above.
                    let c = cursor_captured_actor.load(Ordering::Relaxed);
                    if c != last_cursor_m {
                        cursor_change_resend = 8;
                        last_cursor_m = c;
                    }
                    if cursor_change_resend > 0 || n.is_multiple_of(60) {
                        announce_cursor_to(c, &conn_actor);
                    }
                    cursor_change_resend = cursor_change_resend.saturating_sub(1);
                }
                // Window tick: publish this client's measured wire fps every 1s
                // — INCLUDING zero-frame windows, so a stalled client decays to 0
                // instead of freezing at its last good rate.
                _ = tokio::time::sleep_until(win_start + std::time::Duration::from_secs(1)) => {
                    let el = win_start.elapsed().as_secs_f32();
                    let fps = if el > 0.0 { (win_frames as f32 / el).round() as u32 } else { 0 };
                    fps_ewma.store(fps, Ordering::Relaxed);
                    win_frames = 0;
                    win_start = tokio::time::Instant::now();
                }
            }
        }
        // _guard Drop deregisters the slot.
    });

    // Per-connection INPUT task: forward decoded events to the seat only when
    // THIS device's input is enabled (the panel's per-device toggle). Default
    // is enabled; a disabled device's input is drained + discarded so N mirror
    // viewers don't fight over the one seat — the user picks the driver(s).
    let input_tx_conn = input_tx.clone();
    let conn_input = conn;
    let input_pubkey = client_id;
    tokio::spawn(async move {
        loop {
            let mut recv = match conn_input.accept_bi().await {
                Ok((_send, recv)) => recv,
                Err(_) => return, // connection ended
            };
            let tx = input_tx_conn.clone();
            tokio::spawn(async move {
                let mut len_buf = [0u8; 4];
                loop {
                    if recv.read_exact(&mut len_buf).await.is_err() {
                        return;
                    }
                    let len = u32::from_be_bytes(len_buf) as usize;
                    if len > 4096 {
                        tracing::warn!("input event too large ({len} bytes); dropping stream");
                        return;
                    }
                    let mut buf = vec![0u8; len];
                    if recv.read_exact(&mut buf).await.is_err() {
                        return;
                    }
                    if !input_allowed(input_pubkey) {
                        continue; // disabled device: drain + discard
                    }
                    match core_proto::decode_input(&buf) {
                        Ok(ev) => {
                            if tx.send(ev).is_err() {
                                return;
                            }
                        }
                        Err(e) => tracing::warn!("decode input: {e}"),
                    }
                }
            });
        }
    });
}

/// Pre-emption guard (issue #19): `Some(primary)` iff the single-client slot
/// is currently held by the persisted PRIMARY client — the one case where a
/// newcomer must NOT pre-empt. Mirror mode keeps its own admit path (newcomers
/// join rather than steal), and a non-primary incumbent keeps the validated
/// newest-wins pre-empt, so both return `None`.
fn incumbent_is_primary(mirror: &Arc<AtomicBool>, client_id: Option<[u8; 32]>) -> Option<[u8; 32]> {
    if mirror.load(Ordering::Acquire) {
        return None;
    }
    let primary = primary_peer()?;
    (client_id == Some(primary)).then_some(primary)
}

/// Vet a challenger while the PRIMARY holds the single slot (issue #19), on
/// its own task so a slow (or hostile) handshake can never stall the primary's
/// video. The challenger runs the normal mutual auth + authorization, then:
///   * a different (or anonymous) client is refused with
///     [`RESERVED_CLOSE_CODE`] — nobody is pre-empted, the primary streams on;
///   * the primary ITSELF redialing (its previous session crashed or stalled
///     and the corpse still holds the slot) frees the slot: the stale
///     incumbent is closed with [`PREEMPT_CLOSE_CODE`] and the challenger is
///     closed plain (code 0) so its ordinary fast-reconnect lands on the freed
///     slot — without this, the primary would be locked out of its own host
///     for the ~20s idle timeout.
fn spawn_challenger_vet(
    incoming: quinn::Incoming,
    identity: Arc<core_crypto::Identity>,
    authorizer: ClientAuthorizer,
    primary: [u8; 32],
    incumbent: quinn::Connection,
) {
    tokio::spawn(async move {
        let conn = match incoming.await {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!("challenger connect failed: {e:?}");
                return;
            }
        };
        // `authenticate` self-bounds at 5s, so this task can't linger.
        let client_id = match authenticate(&conn, &identity).await {
            Ok((id, ..)) => id,
            Err(e) => {
                tracing::warn!("challenger auth failed: {e:?}");
                conn.close(0u32.into(), b"auth");
                return;
            }
        };
        if !authorizer(client_id) {
            tracing::warn!("challenger rejected; not in paired peers");
            conn.close(0u32.into(), b"unauthorized");
            return;
        }
        if client_id == Some(primary) {
            tracing::info!("primary redialed over its stale session; freeing the slot");
            incumbent.close(PREEMPT_CLOSE_CODE.into(), b"preempted");
            conn.close(0u32.into(), b"slot freeing; redial");
        } else {
            tracing::info!("refusing new client: host reserved by primary client");
            conn.close(RESERVED_CLOSE_CODE.into(), b"reserved by primary");
        }
    });
}

/// Per-connection clipboard relay: the bidirectional clipboard sync stream the
/// host opens (server-initiated bidi #2, right after the app relay). Symmetric to
/// the app relay's read/write, framed `u32 BE len || postcard ClipboardMsg`:
///
/// - read (client → host): the client copied something; forward it to the
///   compositor which becomes the Wayland selection owner (host apps can paste).
/// - write (host → client): a host app copied; push the text so the client mirrors
///   it into its OS clipboard. Latest-wins `watch`; an empty first value (fresh
///   streamer) is skipped, but a later empty (a genuine host clear) IS sent.
///
/// Both halves cap a frame at [`core_proto::MAX_CLIPBOARD_FRAME`] (the text cap plus
/// the postcard envelope). A client that never accepts this stream simply leaves the
/// writer parked in flow control.
async fn clipboard_relay(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    mut out_rx: tokio::sync::watch::Receiver<core_proto::ClipboardMsg>,
    in_tx: mpsc::UnboundedSender<core_proto::ClipboardMsg>,
) {
    // Reader: client → host copies. Zero-length frames are the host/peer's
    // stream-open nudge (see the writer) — skip them.
    let read = async move {
        let mut len_buf = [0u8; 4];
        loop {
            if recv.read_exact(&mut len_buf).await.is_err() {
                return;
            }
            let len = u32::from_be_bytes(len_buf) as usize;
            if len == 0 {
                continue;
            }
            if len > core_proto::MAX_CLIPBOARD_FRAME {
                tracing::warn!("clipboard frame too large ({len} bytes); dropping stream");
                return;
            }
            let mut buf = vec![0u8; len];
            if recv.read_exact(&mut buf).await.is_err() {
                return;
            }
            match core_proto::decode_clipboard(&buf) {
                Ok(msg) => {
                    if in_tx.send(msg).is_err() {
                        return;
                    }
                }
                Err(e) => tracing::warn!("decode clipboard msg: {e}"),
            }
        }
    };
    // Writer: host → client. First NUDGE the stream open with a zero-length frame
    // so the client's accept_bi() resolves immediately even with an idle host
    // clipboard — without it the client can't accept the stream and its client→host
    // writer never starts (client→host would be dead until the first host copy).
    // Then forward every host clipboard CHANGE during this session. We deliberately
    // do NOT resync the retained watch value on connect: a freshly-joined client
    // must not auto-receive whatever the host copied before it connected (could be a
    // secret) — only live, in-session copies flow.
    let write = async move {
        let send_frame = |msg: &core_proto::ClipboardMsg| core_proto::encode_clipboard(msg);
        if send.write_all(&0u32.to_be_bytes()).await.is_err() {
            return;
        }
        // Mark the current value as seen without sending it.
        out_rx.borrow_and_update();
        loop {
            if out_rx.changed().await.is_err() {
                break;
            }
            let cur = out_rx.borrow_and_update().clone();
            match send_frame(&cur) {
                Ok(buf) => {
                    let len_be = (buf.len() as u32).to_be_bytes();
                    if send.write_all(&len_be).await.is_err() || send.write_all(&buf).await.is_err()
                    {
                        break;
                    }
                }
                Err(e) => tracing::warn!("encode clipboard: {e}"),
            }
        }
        send.finish().ok();
    };
    // `select!`, not `join!`: when the client disconnects the read half returns at
    // once; biasing on either half tears the writer down promptly instead of
    // leaving it parked on `changed()` (the watch sender lives for the whole
    // streamer, so it never errors on a per-client disconnect).
    tokio::select! {
        _ = read => {}
        _ = write => {}
    }
}

#[allow(clippy::too_many_arguments)]
async fn serve_stream(
    addr: SocketAddr,
    mut rx: mpsc::Receiver<VideoChunk>,
    mut audio_rx: mpsc::Receiver<Vec<u8>>,
    mut gyro_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    connected: Arc<AtomicBool>,
    peer_pubkey: Arc<std::sync::Mutex<Option<[u8; 32]>>>,
    peer_addr: Arc<std::sync::Mutex<Option<SocketAddr>>>,
    wants_gyro: Arc<AtomicBool>,
    wants_hdr: Arc<AtomicBool>,
    client_refresh_hz: Arc<std::sync::atomic::AtomicU32>,
    client_panel: Arc<std::sync::atomic::AtomicU64>,
    client_res_pref: Arc<std::sync::atomic::AtomicU64>,
    client_bitrate_pref: Arc<std::sync::atomic::AtomicU64>,
    client_fps_pref: Arc<std::sync::atomic::AtomicU64>,
    identity: Arc<core_crypto::Identity>,
    input_tx: mpsc::UnboundedSender<core_proto::InputEvent>,
    latency_tx: mpsc::UnboundedSender<core_proto::WireLatencyReport>,
    qos_tx: mpsc::UnboundedSender<core_proto::ClientQosV2>,
    probe: Arc<ProbeShared>,
    app_catalog_rx: tokio::sync::watch::Receiver<Vec<core_proto::AppInfo>>,
    app_notice_tx: tokio::sync::broadcast::Sender<String>,
    app_launch_tx: mpsc::UnboundedSender<core_proto::AppMsg>,
    clipboard_out_rx: tokio::sync::watch::Receiver<core_proto::ClipboardMsg>,
    clipboard_in_tx: mpsc::UnboundedSender<core_proto::ClipboardMsg>,
    authorizer: ClientAuthorizer,
    geom: Arc<std::sync::atomic::AtomicU64>,
    hdr: Arc<std::sync::atomic::AtomicU8>,
    cursor_captured: Arc<AtomicBool>,
    loss: Arc<std::sync::atomic::AtomicU32>,
    rtt_us: Arc<std::sync::atomic::AtomicU32>,
    drops: Arc<std::sync::atomic::AtomicU64>,
    request_idr: Arc<AtomicBool>,
    mirror: Arc<AtomicBool>,
    clients: Arc<std::sync::Mutex<Vec<ClientSlot>>>,
    dual_stack: bool,
) -> Result<()> {
    // Retry on EADDRINUSE: a host Relaunch re-execs in place, and the previous
    // image's UDP socket can still be releasing when the new one rebinds — so
    // wait it out (up to ~3s) instead of failing the whole host.
    let endpoint = 'bind: {
        // Dual-stack: one `[::]:port` socket serves IPv6 + IPv4. On any failure fall
        // through to the IPv4 bind below (never leave the host dead for it).
        if dual_stack {
            match dualstack_server_endpoint(server_config()?, addr.port()) {
                Ok(ep) => {
                    tracing::info!(
                        "QUIC dual-stack (IPv6+IPv4) listener on [::]:{}",
                        addr.port()
                    );
                    break 'bind ep;
                }
                Err(e) => tracing::warn!(
                    "dual-stack bind [::]:{} failed ({e}); falling back to IPv4 {addr}",
                    addr.port()
                ),
            }
        }
        let mut attempt = 0u32;
        loop {
            let cfg = server_config()?;
            match Endpoint::server(cfg, addr) {
                Ok(ep) => break ep,
                Err(e) if e.to_string().to_lowercase().contains("in use") && attempt < 20 => {
                    attempt += 1;
                    tracing::warn!("stream bind {addr} busy (attempt {attempt}); retrying…");
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                }
                // Durably taken (another service owns it, not our own releasing
                // socket). Auto-resolve to a free port + log LOUDLY rather than
                // leave the host dead — this is the failure the panel's
                // walked-into-usbimport bug produced.
                Err(e) if e.to_string().to_lowercase().contains("in use") => {
                    match bind_free_from(addr) {
                        Some((ep, used)) => {
                            tracing::error!(
                                "stream bind {addr} is taken by another service; \
                                 auto-failed-over to {used} — point the client at \
                                 {} or free {addr}",
                                used.port()
                            );
                            break ep;
                        }
                        None => return Err(e.into()),
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
    };
    tracing::info!("QUIC streamer listening on {}", endpoint.local_addr()?);
    // Multi-shot accept: when a client drops we go back to accept(), so the
    // host stays up across reconnects. Frames queued during the gap get
    // drained (they're for the previous session and would corrupt the
    // newcomer's decoder). The compositor gates its encoder on `connected`,
    // so the queue should be empty by the time we re-accept; the drain is
    // belt-and-braces against the encode-in-flight race window.
    //
    // NOTE: the per-session NVENC encoder on the host keeps running across
    // reconnects, so the first frames a new client sees may be P-frames
    // referencing the prior session. The encoder is now configured for a
    // 1-second IDR period with repeated VPS/SPS/PPS (see the encode-config
    // setup in compositor `lib.rs`/`multi.rs`), so a reconnecting decoder
    // resyncs within ≤1s instead of stalling forever on `PPS id out of range`.
    // A new client can PRE-EMPT the active session: the inner streaming loop
    // stashes the newcomer's raw `Incoming` here, closes the incumbent, and
    // breaks, so the next turn serves the newcomer. This frees the single-client
    // slot the instant a replacement connects — covering both a deliberate device
    // switch AND a crashed/powered-off client whose dead connection would
    // otherwise hold the slot until the 20s idle timer (and sometimes far longer,
    // while 2s keep-alives keep the corpse warm). The newcomer is still
    // authenticated (with a timeout) at the loop top before it gets any frames.
    let mut pending_incoming: Option<quinn::Incoming> = None;
    loop {
        let incoming = match pending_incoming.take() {
            Some(i) => i,
            None => match endpoint.accept().await {
                Some(i) => i,
                None => {
                    tracing::info!("endpoint closed");
                    return Ok(());
                }
            },
        };
        let conn = match incoming.await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("incoming.await: {e:?}");
                continue;
            }
        };
        tracing::info!("Trireme connected from {}", conn.remote_address());

        // Mutual auth, both directions channel-bound to the TLS exporter:
        //   client → host: [optional v2 prefix: 4-byte magic + u32 version]
        //                  client nonce + client pubkey (all-zero = anonymous)
        //                  [v2 only: 1-byte wants_gyro flag]
        //   host → client: host signature + host pubkey + host challenge nonce
        //   client → host: client signature over the host challenge
        //
        // **v1/v2 detection:** the host reads the first 4 bytes. If they match
        // [`AUTH_PROTO_MAGIC`] it's a v2 client and we then read the version u32
        // + the rest of the v2 frame. Otherwise those 4 bytes are the first 4
        // bytes of the v1 nonce — read the remaining 28 bytes and proceed
        // exactly as the v1 wire, with wants_gyro defaulting to false.
        //
        // Collision: a v1 client's random nonce matching the magic is 1/2^32.
        // On that vanishingly-rare false positive the client's "version" reads
        // as random bytes, auth fails, the user reconnects. No security impact —
        // the signatures still bind to the TLS exporter.
        //
        // The host always proves itself (host→client pinning unchanged); the
        // client proves itself too, and we then *authorize* the verified
        // pubkey. Trust is entirely in these Ed25519 identities — the TLS cert
        // is just the tunnel (see `SkipServerVerification`), never a trust
        // anchor (avoids the Nova/Polaris self-signed-cert pitfall).
        //
        // The handshake itself lives in [`authenticate`] (shared verbatim with the
        // mirror accept path); failure here closes the connection and awaits the
        // next. We then *authorize* the verified pubkey below.
        let (client_id, client_wants_gyro, client_wants_hdr, client_mode) =
            match authenticate(&conn, &identity).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("auth failed: {e:?}; awaiting next client");
                    conn.close(0u32.into(), b"auth");
                    continue;
                }
            };
        if !authorizer(client_id) {
            tracing::warn!(
                "client rejected ({}); not in paired peers",
                if client_id.is_some() {
                    "presented identity"
                } else {
                    "anonymous"
                }
            );
            conn.close(0u32.into(), b"unauthorized");
            continue;
        }

        // MIRROR keep-N: hand this authed connection to the fan-out loop, which
        // admits it + accepts more, sharing ONE encode across all. Returns when
        // the mode flips back to single; then `continue` re-enters the validated
        // pre-empt path below (which is byte-identical — reached only when the
        // mirror flag is off, the default). See MIRROR_EXTEND_PLAN.md.
        if mirror.load(Ordering::Acquire) {
            run_mirror_loop(
                &endpoint,
                &mut rx,
                &mut audio_rx,
                &mut gyro_rx,
                &mirror,
                &clients,
                &identity,
                &authorizer,
                conn,
                client_id,
                client_wants_gyro,
                client_wants_hdr,
                &connected,
                &wants_gyro,
                &wants_hdr,
                &geom,
                &hdr,
                &cursor_captured,
                &input_tx,
            )
            .await;
            continue;
        }

        // Drain leftover video frames + audio packets queued for the previous
        // client (audio is unreliable, so stale packets are simply dropped). The
        // video drain also clears any leftover bench meta — it now rides inline on
        // each VideoChunk, so there's no separate channel to flush.
        while rx.try_recv().is_ok() {}
        while audio_rx.try_recv().is_ok() {}

        let mut send = match conn.open_uni().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("open_uni: {e:?}");
                continue;
            }
        };
        wants_gyro.store(client_wants_gyro, Ordering::Release);
        wants_hdr.store(client_wants_hdr, Ordering::Release);
        // Per-client panel refresh (Hz) for the compositor's fps cap. 0 from a v2
        // client (no StreamMode) → the compositor leaves fps at its config ceiling.
        client_refresh_hz.store(client_mode.refresh_hz(), Ordering::Release);
        // Panel native resolution (Phase 3c): packed (w<<32)|h, 0 from a v2 client.
        // The compositor reads it on connect to decide whether to grow the encode
        // envelope (restart-into-panel-res). 0×0 leaves the envelope at config.
        client_panel.store(
            (u64::from(client_mode.width) << 32) | u64::from(client_mode.height),
            Ordering::Release,
        );
        tracing::info!(
            "client requested mode: {}x{} @ {}Hz (refresh_mhz={}) — fps caps to refresh when >0",
            client_mode.width,
            client_mode.height,
            client_mode.refresh_hz(),
            client_mode.refresh_mhz
        );
        // Attribute the stream to its verified peer (None for an anonymous
        // client) so the host's status can name who's connected, and record the
        // remote addr it dialed from.
        if let Ok(mut g) = peer_pubkey.lock() {
            *g = client_id;
        }
        if let Ok(mut g) = peer_addr.lock() {
            *g = Some(conn.remote_address());
        }
        // Reachable by the control plane's ForgetPeer teardown (issue #19).
        register_session(client_id, &conn);
        connected.store(true, Ordering::Release);
        tracing::info!(
            "streaming to client (wants_gyro={}, wants_hdr={})",
            client_wants_gyro,
            client_wants_hdr
        );

        // Spawn a per-connection task that accepts the input bidi stream (if
        // the client opens one) and forwards each decoded InputEvent to the
        // compositor via `input_tx`. The stream is optional — clients without
        // input capture (trireme-kms, trireme decode) just never open it.
        // When the connection ends, the read loop ends with an error and the
        // task exits cleanly.
        let input_tx_conn = input_tx.clone();
        let conn_input = conn.clone();
        let input_pubkey = client_id;
        tokio::spawn(async move {
            loop {
                let mut recv = match conn_input.accept_bi().await {
                    Ok((_send, recv)) => recv,
                    Err(_) => return, // connection ended
                };
                let tx = input_tx_conn.clone();
                tokio::spawn(async move {
                    let mut len_buf = [0u8; 4];
                    loop {
                        if recv.read_exact(&mut len_buf).await.is_err() {
                            return;
                        }
                        let len = u32::from_be_bytes(len_buf) as usize;
                        // Sanity ceiling — postcard'd events are tens of bytes.
                        if len > 4096 {
                            tracing::warn!("input event too large ({len} bytes); dropping stream");
                            return;
                        }
                        let mut buf = vec![0u8; len];
                        if recv.read_exact(&mut buf).await.is_err() {
                            return;
                        }
                        // Per-device gate: drain + discard if the panel disabled
                        // this device's input (default = allowed).
                        if !input_allowed(input_pubkey) {
                            continue;
                        }
                        match core_proto::decode_input(&buf) {
                            Ok(ev) => {
                                if tx.send(ev).is_err() {
                                    return;
                                }
                            }
                            Err(e) => tracing::warn!("decode input: {e}"),
                        }
                    }
                });
            }
        });

        // Spawn the per-connection APP RELAY task. The host *opens* a dedicated
        // bidi stream (a direction the client only accepts, so it never collides
        // with the input/video streams). It pushes the catalog (host→client,
        // re-sent on change) and reads launch requests (client→host). Both are
        // `AppMsg` framed as `u32 BE len || postcard`. Clients that don't speak
        // the app protocol simply never accept the stream — harmless.
        let conn_app = conn.clone();
        let mut cat_rx = app_catalog_rx.clone();
        let mut app_notice_rx = app_notice_tx.subscribe();
        let launch_tx_conn = app_launch_tx.clone();
        let clip_out_rx = clipboard_out_rx.clone();
        let clip_in_tx = clipboard_in_tx.clone();
        tokio::spawn(async move {
            let (mut send, mut recv) = match conn_app.open_bi().await {
                Ok(s) => s,
                Err(_) => return, // connection ended
            };
            // Open the CLIPBOARD RELAY as the second server-initiated bidi, right
            // after the app relay (idx0) — sequential in THIS task so the ordering
            // is deterministic (quinn yields accepts in ascending stream-id = open
            // order). The client demuxes: 1st server-bidi = app, 2nd = clipboard.
            // An old client that only accepts the app stream never accepts this one
            // (our writes just park in flow control — harmless).
            match conn_app.open_bi().await {
                Ok((clip_send, clip_recv)) => {
                    tokio::spawn(clipboard_relay(
                        clip_send,
                        clip_recv,
                        clip_out_rx,
                        clip_in_tx,
                    ));
                }
                Err(_) => return, // connection ended
            }
            tracing::info!("apprelay-host: app stream opened to client");
            // Reader: client → host launch requests.
            let read = async move {
                let mut len_buf = [0u8; 4];
                loop {
                    if recv.read_exact(&mut len_buf).await.is_err() {
                        tracing::info!("apprelay-host: app recv closed/err (read loop exit)");
                        return;
                    }
                    let len = u32::from_be_bytes(len_buf) as usize;
                    tracing::info!("apprelay-host: read {len}b frame from client");
                    // Launch messages are tiny; cap defensively.
                    if len > 64 * 1024 {
                        tracing::warn!("app msg too large ({len} bytes); dropping stream");
                        return;
                    }
                    let mut buf = vec![0u8; len];
                    if recv.read_exact(&mut buf).await.is_err() {
                        return;
                    }
                    match core_proto::decode_app(&buf) {
                        // Forward every client→host action (Launch/SwitchTo/EndProcess)
                        // to the compositor; Catalog flows the other way (ignore).
                        Ok(msg @ core_proto::AppMsg::Launch { .. })
                        | Ok(msg @ core_proto::AppMsg::SwitchTo { .. })
                        | Ok(msg @ core_proto::AppMsg::EndProcess { .. }) => {
                            tracing::info!("app relay: received {msg:?}");
                            if launch_tx_conn.send(msg).is_err() {
                                return;
                            }
                        }
                        Ok(_) => {} // Catalog flows the other way; ignore.
                        Err(e) => tracing::warn!("decode app msg: {e}"),
                    }
                }
            };
            // Writer: host → client catalog (retained state, sent first + on
            // change) AND one-shot notices (toasts), multiplexed on the one app
            // stream. A small framing helper keeps both paths identical.
            let write = async move {
                async fn frame(send: &mut quinn::SendStream, msg: &core_proto::AppMsg) -> bool {
                    match core_proto::encode_app(msg) {
                        Ok(buf) => {
                            let len_be = (buf.len() as u32).to_be_bytes();
                            send.write_all(&len_be).await.is_ok()
                                && send.write_all(&buf).await.is_ok()
                        }
                        Err(e) => {
                            tracing::warn!("encode app msg: {e}");
                            true // an encode slip isn't a stream error
                        }
                    }
                }
                // Send the current catalog up front (skip an empty seed).
                {
                    let apps = cat_rx.borrow_and_update().clone();
                    if !apps.is_empty()
                        && !frame(&mut send, &core_proto::AppMsg::Catalog { apps }).await
                    {
                        return;
                    }
                }
                loop {
                    tokio::select! {
                        changed = cat_rx.changed() => {
                            if changed.is_err() {
                                break;
                            }
                            let apps = cat_rx.borrow_and_update().clone();
                            if !apps.is_empty()
                                && !frame(&mut send, &core_proto::AppMsg::Catalog { apps }).await
                            {
                                break;
                            }
                            continue;
                        }
                        notice = app_notice_rx.recv() => {
                            match notice {
                                Ok(message) => {
                                    if !frame(&mut send, &core_proto::AppMsg::Notice { message }).await {
                                        break;
                                    }
                                }
                                // Lagged (dropped some notices) — keep going; a
                                // missed toast is harmless. Closed → sender gone.
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                            }
                            continue;
                        }
                    }
                }
                // Signal EOF to the client's catalog reader on teardown.
                send.finish().ok();
            };
            tokio::join!(read, write);
        });

        // Spawn a per-connection INBOUND datagram task. The host normally only
        // *sends* datagrams (audio/gyro/frame-meta); this reads the latency
        // benchmark's two client→host datagram kinds:
        //   DG_CLOCK_PING  — answer immediately with a DG_CLOCK_PONG carrying the
        //                    echoed t0, our receive instant (t1) and send instant
        //                    (t2) so the client can do the NTP-style offset/RTT calc.
        //   DG_LATENCY_REPORT — decode the final WireLatencyReport and forward it.
        // On `read_datagram` error the connection has ended, so we return (no leak
        // across reconnects — a fresh task is spawned for the next client). Inert
        // when no bench runs: the client never sends these unprompted.
        let latency_tx_conn = latency_tx.clone();
        let qos_tx_conn = qos_tx.clone();
        let conn_dg = conn.clone();
        let request_idr_dg = request_idr.clone();
        let client_res_pref_dg = client_res_pref.clone();
        let client_bitrate_pref_dg = client_bitrate_pref.clone();
        let client_fps_pref_dg = client_fps_pref.clone();
        let probe_dg = probe.clone();
        tokio::spawn(async move {
            loop {
                let dg = match conn_dg.read_datagram().await {
                    Ok(d) => d,
                    Err(_) => return, // connection ended
                };
                match dg.first().copied() {
                    Some(DG_CLOCK_PING) => {
                        let t1 = unix_nanos();
                        let ping = match core_proto::latency::decode_clock_ping(&dg[1..]) {
                            Ok(p) => p,
                            Err(e) => {
                                tracing::debug!("decode clock ping: {e}");
                                continue;
                            }
                        };
                        let pong = core_proto::ClockPong {
                            seq: ping.seq,
                            t0_nanos: ping.t0_nanos,
                            t1_nanos: t1,
                            t2_nanos: unix_nanos(),
                        };
                        match core_proto::latency::encode_clock_pong(&pong) {
                            Ok(b) => {
                                if let Err(e) =
                                    conn_dg.send_datagram(tagged_datagram(DG_CLOCK_PONG, &b))
                                {
                                    tracing::debug!("clock pong datagram dropped: {e}");
                                }
                            }
                            Err(e) => tracing::debug!("encode clock pong: {e}"),
                        }
                    }
                    Some(DG_LATENCY_REPORT) => {
                        match core_proto::latency::decode_wire_latency(&dg[1..]) {
                            Ok(report) => {
                                let _ = latency_tx_conn.send(report);
                            }
                            Err(e) => tracing::debug!("decode latency report: {e}"),
                        }
                    }
                    Some(DG_CLIENT_QOS) => {
                        // v2 adds measured goodput + transport hint (issue #40);
                        // a v1 report (older client) widens with inert fields.
                        match core_proto::latency::decode_client_qos_any(&dg[1..]) {
                            Ok(qos) => {
                                probe_dg.transport.store(qos.transport, Ordering::Relaxed);
                                if qos.recv_kbps > 0 {
                                    probe_dg.goodput_seen.store(true, Ordering::Relaxed);
                                    // Inside an open probe window: fold this sample
                                    // into the running max (the measurement).
                                    let until = probe_dg.window_until_ms.load(Ordering::Relaxed);
                                    if unix_nanos() / 1_000_000 <= until {
                                        probe_dg
                                            .pending_max_kbps
                                            .fetch_max(qos.recv_kbps, Ordering::Relaxed);
                                    }
                                }
                                let _ = qos_tx_conn.send(qos);
                            }
                            Err(e) => tracing::debug!("decode client qos: {e}"),
                        }
                    }
                    Some(DG_REQUEST_IDR) => {
                        // Client saw corruption / a lost reference — force the next
                        // encode to be an IDR (the compositor rate-limits it).
                        request_idr_dg.store(true, Ordering::Relaxed);
                    }
                    Some(DG_CLIENT_RESPREF) => {
                        // Client's requested resolution range (Phase 2). Store packed
                        // (target_h<<32)|floor_h; the compositor reads it each window
                        // and applies it as the per-output ceiling/floor.
                        match core_proto::latency::decode_client_res_pref(&dg[1..]) {
                            Ok(p) => {
                                let packed = (u64::from(p.target_h) << 32) | u64::from(p.floor_h);
                                client_res_pref_dg.store(packed, Ordering::Relaxed);
                            }
                            Err(e) => tracing::debug!("decode client res pref: {e}"),
                        }
                    }
                    Some(DG_CLIENT_BITRATEPREF) => {
                        // Client's requested bitrate range (Phase 3). Store packed
                        // (min_kbps<<32)|max_kbps; the compositor reads it as the
                        // controller's bitrate bounds (max=0 = uncapped).
                        match core_proto::latency::decode_client_bitrate_pref(&dg[1..]) {
                            Ok(p) => {
                                let packed = (u64::from(p.min_kbps) << 32) | u64::from(p.max_kbps);
                                client_bitrate_pref_dg.store(packed, Ordering::Relaxed);
                            }
                            Err(e) => tracing::debug!("decode client bitrate pref: {e}"),
                        }
                    }
                    Some(DG_CLIENT_FPSPREF) => {
                        // Client's requested framerate range (Phase 3). Store packed
                        // (min_fps<<32)|max_fps; the compositor reads it as the
                        // encode-pace ceiling + adaptation floor (max=0 = Auto/panel).
                        match core_proto::latency::decode_client_fps_pref(&dg[1..]) {
                            Ok(p) => {
                                let packed = (u64::from(p.min_fps) << 32) | u64::from(p.max_fps);
                                client_fps_pref_dg.store(packed, Ordering::Relaxed);
                            }
                            Err(e) => tracing::debug!("decode client fps pref: {e}"),
                        }
                    }
                    _ => {}
                }
            }
        });

        // Bandwidth-probe pad sender (issue #40): one task per connection. Runs
        // one padded ramp per `start_probe` generation; the QoS arm above folds
        // the client's delivered-goodput samples into the measurement, and this
        // task publishes the result when the window closes. Idles on a coarse
        // tick while no probe is pending; ends with the connection.
        let probe_pad = probe.clone();
        let conn_pad = conn.clone();
        tokio::spawn(async move {
            let mut last_gen = probe_pad.done_gen.load(Ordering::Relaxed);
            loop {
                tokio::select! {
                    _ = conn_pad.closed() => return,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
                }
                let gen = probe_pad.req_gen.load(Ordering::Relaxed);
                if gen == last_gen {
                    continue;
                }
                last_gen = gen;
                let ceiling_kbps =
                    u64::from(probe_pad.ceiling_kbps.load(Ordering::Relaxed)).max(1_000);
                let duration_ms =
                    u64::from(probe_pad.duration_ms.load(Ordering::Relaxed)).clamp(500, 10_000);
                probe_pad.pending_max_kbps.store(0, Ordering::Relaxed);
                probe_pad.window_until_ms.store(
                    unix_nanos() / 1_000_000 + duration_ms + 1_500,
                    Ordering::Relaxed,
                );
                // One reusable tagged pad datagram, sized to the path MTU
                // (`Bytes` clones are refcounted — no per-send allocation).
                let pad_len = conn_pad
                    .max_datagram_size()
                    .unwrap_or(1_000)
                    .min(1_200)
                    .saturating_sub(1)
                    .max(256);
                let pad = tagged_datagram(DG_PROBE_PAD, &vec![0u8; pad_len]);
                tracing::info!(
                    target: "compositor::wire",
                    "probe[{gen}]: ramping to {ceiling_kbps}kbps offered load over {duration_ms}ms (pad={pad_len}B)"
                );
                const SLOT_MS: u64 = 25;
                let slots = (duration_ms / SLOT_MS).max(1);
                let ramp_slots = (slots / 4).max(1); // 25% ramp, then hold at ceiling
                'ramp: for i in 0..slots {
                    let frac = ((i + 1) as f64 / ramp_slots as f64).min(1.0);
                    let rate_kbps = ceiling_kbps as f64 * frac;
                    let mut budget = (rate_kbps * 1000.0 / 8.0 * SLOT_MS as f64 / 1000.0) as u64;
                    while budget > 0 {
                        // Keep headroom in quinn's datagram send buffer so real
                        // datagrams (audio, geometry announces) are never evicted
                        // by padding. Padding that doesn't fit simply isn't
                        // offered — BBR is already sending as fast as the path
                        // allows, which is the very thing being measured.
                        if conn_pad.datagram_send_buffer_space() < 64 * 1024 {
                            break;
                        }
                        if conn_pad.send_datagram(pad.clone()).is_err() {
                            break 'ramp; // connection gone / datagrams unsupported
                        }
                        budget = budget.saturating_sub(pad.len() as u64);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(SLOT_MS)).await;
                }
                // Tail: let the client QoS window covering the hold phase land.
                tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
                let measured = probe_pad.pending_max_kbps.load(Ordering::Relaxed);
                probe_pad.measured_kbps.store(measured, Ordering::Relaxed);
                probe_pad.done_gen.store(gen, Ordering::Relaxed);
                tracing::info!(
                    target: "compositor::wire",
                    "probe[{gen}]: done — peak delivered goodput {measured}kbps"
                );
            }
        });

        // Stream until the client drops or the channel closes. We have to
        // race the send-loop against `conn.closed()` — the compositor stops
        // pushing the moment we flip `connected=false` further down, so a
        // bare `rx.recv().await` would block past the disconnect. The
        // `closed()` future resolves the instant the connection ends.
        enum LoopEnd {
            PeerDropped,
            CompositorClosed,
            SendError(anyhow::Error),
            // A newer client connected and took the slot; `pending_incoming`
            // holds its raw Incoming to serve on the next loop turn.
            Preempted,
        }
        // Announce the encoder geometry to the client up front (before any AU) so
        // it can size its decoder to the true resolution; then re-announce on
        // change and ~1×/s (it's an unreliable datagram, so a periodic resend
        // recovers a lost one — one tiny datagram/sec is negligible).
        let mut geom_resend: u32 = 0;
        // Frames remaining in a dense re-send burst armed on a geometry CHANGE
        // (the resolution ladder). The connect-time `geom_resend <= 8` burst only
        // covers the first frames of the connection; a mid-stream change needs its
        // own burst so the client receives the new size promptly under loss
        // instead of betting on one datagram + the ~1s periodic backstop.
        let mut geom_change_resend: u32 = 0;
        let mut last_lost: u64 = 0;
        let mut last_sent: u64 = 0;
        let announce_geom = |g: u64, conn: &quinn::Connection| {
            if g != 0 {
                let payload = [
                    (((g >> 32) as u32).to_be_bytes()),
                    ((g as u32).to_be_bytes()),
                ]
                .concat();
                let _ = conn.send_datagram(tagged_datagram(DG_STREAM_GEOMETRY, &payload));
            }
        };
        let mut last_geom = {
            let g = geom.load(Ordering::Relaxed);
            announce_geom(g, &conn);
            g
        };
        // Colorimetry announce (DG_STREAM_HDR), on the same cadence as the geometry:
        // re-sent densely on a mid-session SDR↔HDR flip and ~1×/s as a backstop against
        // a lost datagram. Tri-state like the client's own (issue #28): 0 = undetermined
        // → announce nothing (the client plain-decodes) until the first pipeline build
        // determines colorimetry; 1/2 = SDR/HDR. This is what kills the connect-time gray
        // wash — the client is never told a premature SDR it must later correct.
        let mut hdr_change_resend: u32 = 0;
        let mut last_hdr = {
            let h = hdr.load(Ordering::Relaxed);
            if h != 0 {
                announce_hdr_to(h == 2, &conn);
            }
            h
        };
        // Cursor-mode announce (DG_CURSOR_MODE), on the same cadence as the HDR flag:
        // sent up front so the client's cursor grab defaults correctly, re-sent densely
        // on a CHANGE (game grabbed/released the mouse) so the transition reaches the
        // client promptly under loss, and ~1×/s as a backstop. FREE (false) is the
        // honest default the announce always carries.
        let mut cursor_change_resend: u32 = 0;
        let mut last_cursor = {
            let c = cursor_captured.load(Ordering::Relaxed);
            announce_cursor_to(c, &conn);
            c
        };

        // Link-health heartbeat: every ~500ms tell the client whether we've shed any
        // video AU since the last beat (the bounded queue dropped → reference loss →
        // visible corruption). `last_drops` is seeded from the CURRENT counter so a
        // previous session's accumulated drops don't make the first beat of this
        // (per-connection) loop falsely report degraded.
        let mut health_tick = tokio::time::interval(std::time::Duration::from_millis(500));
        health_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_drops = drops.load(Ordering::Relaxed);
        // QUIC path telemetry (every 4th health beat ≈ 2s): cwnd/rtt/loss straight
        // from quinn — THE readout separating "the path is fine" from "the
        // congestion controller collapsed on stochastic WiFi loss" (a cwnd parked
        // in the tens of KB with congestion events climbing is the collapse).
        let mut stats_beat = 0u32;
        let (mut stats_last_lost, mut stats_last_cong) = (0u64, 0u64);

        let end_reason: LoopEnd;
        'stream: loop {
            tokio::select! {
                biased;
                _ = conn.closed() => {
                    end_reason = LoopEnd::PeerDropped;
                    break;
                }
                // Pre-emption (polled before video so a switch/replacement lands
                // promptly even mid-stream): a new client connecting takes over.
                // Stash its raw Incoming, drop the incumbent (instant switch; a dead
                // client's slot frees without waiting on the 20s idle timer), and
                // break — the newcomer is authenticated at the loop top.
                //
                // EXCEPT while the PRIMARY client holds the slot (issue #19): then
                // the newcomer is vetted on its own task and refused with
                // RESERVED_CLOSE_CODE unless it *is* the primary redialing — the
                // primary's stream is never yanked by an auto-redialing sibling.
                inc = endpoint.accept() => {
                    if let Some(inc) = inc {
                        if let Some(primary) = incumbent_is_primary(&mirror, client_id) {
                            spawn_challenger_vet(
                                inc,
                                identity.clone(),
                                authorizer.clone(),
                                primary,
                                conn.clone(),
                            );
                            continue; // keep streaming to the primary
                        }
                        tracing::info!("new client connecting; pre-empting current session");
                        // If mirror just turned on, this incumbent shouldn't yield
                        // forever — close with MODE_SWITCH (a benign redial) so it
                        // reconnects and rejoins as a co-client. Off (the default) =
                        // PREEMPT 0x9, byte-identical single-client behaviour.
                        let close_code = if mirror.load(Ordering::Acquire) {
                            MODE_SWITCH_CLOSE_CODE
                        } else {
                            PREEMPT_CLOSE_CODE
                        };
                        conn.close(close_code.into(), b"preempted");
                        pending_incoming = Some(inc);
                        end_reason = LoopEnd::Preempted;
                        break;
                    }
                }
                _ = health_tick.tick() => {
                    // Heartbeat: degraded iff a video AU was dropped since the last
                    // beat. One tiny unreliable datagram; a lost beat self-heals next
                    // tick, and the client holds the indicator briefly (hysteresis) so
                    // a single-window blip doesn't flicker.
                    let now = drops.load(Ordering::Relaxed);
                    let degraded = now > last_drops;
                    last_drops = now;
                    let _ = conn.send_datagram(tagged_datagram(DG_LINK_HEALTH, &[degraded as u8]));
                    stats_beat = stats_beat.wrapping_add(1);
                    if stats_beat.is_multiple_of(4) {
                        let s = conn.stats();
                        let (lost, cong) = (s.path.lost_packets, s.path.congestion_events);
                        tracing::info!(
                            "quic path: cwnd={}KB rtt={}ms lost=+{} congestion=+{}",
                            s.path.cwnd / 1024,
                            s.path.rtt.as_millis(),
                            lost.saturating_sub(stats_last_lost),
                            cong.saturating_sub(stats_last_cong),
                        );
                        (stats_last_lost, stats_last_cong) = (lost, cong);
                    }
                }
                next = rx.recv() => {
                    let Some(chunk) = next else {
                        end_reason = LoopEnd::CompositorClosed;
                        break;
                    };
                    // Re-announce geometry on change (the res actuator) and ~1×/s as
                    // belt-and-braces against a lost geometry datagram.
                    geom_resend = geom_resend.wrapping_add(1);
                    let g = geom.load(Ordering::Relaxed);
                    // Arm a dense re-send burst on a CHANGE (the res actuator), so the
                    // client almost certainly receives the new geometry promptly even
                    // under loss (it's unreliable + un-retransmitted) — a single
                    // change-frame announce + the ~1s backstop would leave up to ~1s of
                    // sheared/green frames per ladder step.
                    if g != 0 && g != last_geom {
                        geom_change_resend = 8;
                        last_geom = g;
                    }
                    // Announce during the connect-time burst, during a change burst,
                    // and ~1×/s thereafter as a steady backstop.
                    if g != 0
                        && (geom_change_resend > 0
                            || geom_resend <= 8
                            || geom_resend.is_multiple_of(120))
                    {
                        announce_geom(g, &conn);
                    }
                    geom_change_resend = geom_change_resend.saturating_sub(1);
                    // Colorimetry, same cadence as geometry (reusing `geom_resend` as
                    // the shared frame counter): arm a dense burst on a CHANGE so a
                    // respawn-boundary SDR↔HDR flip reaches the client promptly under
                    // loss, and back it up ~1×/s. The client rebuilds its decode chain
                    // when the announced flag flips (issue #16).
                    let h = hdr.load(Ordering::Relaxed);
                    if h != last_hdr {
                        hdr_change_resend = 8;
                        last_hdr = h;
                    }
                    if h != 0
                        && (hdr_change_resend > 0
                            || geom_resend <= 8
                            || geom_resend.is_multiple_of(120))
                    {
                        announce_hdr_to(h == 2, &conn);
                    }
                    hdr_change_resend = hdr_change_resend.saturating_sub(1);
                    // Cursor mode, same cadence as the HDR flag above (also keyed off
                    // `geom_resend`): arm a dense burst on a CHANGE so a gameplay↔menu
                    // cursor-grab transition reaches the client promptly under loss, and
                    // back it up ~1×/s. The client re-syncs its own cursor grab when the
                    // announced flag flips.
                    let c = cursor_captured.load(Ordering::Relaxed);
                    if c != last_cursor {
                        cursor_change_resend = 8;
                        last_cursor = c;
                    }
                    if cursor_change_resend > 0
                        || geom_resend <= 8
                        || geom_resend.is_multiple_of(120)
                    {
                        announce_cursor_to(c, &conn);
                    }
                    cursor_change_resend = cursor_change_resend.saturating_sub(1);
                    // Step 0 congestion observability: surface quinn's path stats
                    // (RTT, cwnd, packet loss) ~1×/s. The pacer's network term is
                    // driven by client-reported delivered fps; these localise WHERE
                    // loss is (host link vs. last-mile) and are the cheapest free
                    // host-side signal for a future loss-based term.
                    if geom_resend.is_multiple_of(120) {
                        let s = conn.stats().path;
                        let dlost = s.lost_packets.saturating_sub(last_lost);
                        let dsent = s.sent_packets.saturating_sub(last_sent);
                        last_lost = s.lost_packets;
                        last_sent = s.sent_packets;
                        // Forward-path loss permille over the last window — the host's OWN
                        // congestion signal, usable to adapt even for a client that sends
                        // no DG_CLIENT_QOS (e.g. an older APK). Published for the pacer.
                        let permille = dlost
                            .saturating_mul(1000)
                            .checked_div(dsent)
                            .map_or(0, |v| v.min(1000) as u32);
                        loss.store(permille, Ordering::Relaxed);
                        rtt_us.store(
                            s.rtt.as_micros().min(u128::from(u32::MAX)) as u32,
                            Ordering::Relaxed,
                        );
                        tracing::info!(
                            target: "compositor::wire",
                            "quic path: rtt={:.0}ms cwnd={} lost_1s={} loss={}permille lost_total={}",
                            s.rtt.as_secs_f32() * 1000.0,
                            s.cwnd,
                            dlost,
                            permille,
                            s.lost_packets,
                        );
                    }
                    // Bench window only: stamp `wire_send_ns` at the LAST possible
                    // host instant (right before the bytes go on the wire), then send
                    // the meta as a DG_FRAME_META datagram 1:1 with this AU. Inert in
                    // normal streaming — `meta` is None, so this whole block is skipped.
                    if let Some(mut meta) = chunk.meta {
                        meta.wire_send_ns = unix_nanos();
                        match core_proto::latency::encode_frame_meta(&meta) {
                            Ok(b) => {
                                if let Err(e) =
                                    conn.send_datagram(tagged_datagram(DG_FRAME_META, &b))
                                {
                                    tracing::debug!("frame-meta datagram dropped: {e}");
                                }
                            }
                            Err(e) => tracing::debug!("encode frame meta: {e}"),
                        }
                    }
                    let len_be = (chunk.au.len() as u32).to_be_bytes();
                    let chunk_bytes = chunk.au.len();
                    let write_t0 = std::time::Instant::now();
                    // The video write can park INDEFINITELY feeding a dead-but-kept-
                    // alive peer: the flow-control window exhausts, no ACKs return
                    // credit, and the 2s keep-alives stop quinn's idle timer from ever
                    // reaping the corpse — so a bare `write_all().await` here wedges the
                    // whole single-client slot until a manual host restart (the bug we
                    // are fixing). Race the send against the connection closing AND a
                    // NEW client arriving: either frees the slot immediately, with no
                    // dependence on the idle timer. Cancelling the half-written
                    // length-prefixed stream is safe — we only do it while ABANDONING
                    // this connection (PeerDropped/Preempted); the stream is never reused.
                    let send_fut = async {
                        send.write_all(&len_be).await?;
                        send.write_all(&chunk.au).await?;
                        anyhow::Ok(())
                    };
                    // Pinned + re-polled (`&mut send_fut`) so the primary-guard
                    // arm below can vet a challenger WITHOUT dropping a
                    // half-written frame — abandoning the length-prefixed write
                    // mid-AU while keeping the connection would desync the
                    // client's framing for good. The future is only ever dropped
                    // on the paths that abandon the whole connection.
                    tokio::pin!(send_fut);
                    loop {
                        tokio::select! {
                            biased;
                            _ = conn.closed() => {
                                end_reason = LoopEnd::PeerDropped;
                                break 'stream;
                            }
                            inc = endpoint.accept() => {
                                if let Some(inc) = inc {
                                    if let Some(primary) = incumbent_is_primary(&mirror, client_id) {
                                        // Primary holds the slot (issue #19): vet
                                        // the newcomer off-path and keep driving
                                        // the pinned send on the next pass.
                                        spawn_challenger_vet(
                                            inc,
                                            identity.clone(),
                                            authorizer.clone(),
                                            primary,
                                            conn.clone(),
                                        );
                                    } else {
                                        tracing::info!("new client connecting mid-send; pre-empting current session");
                                        // Mirror-on → MODE_SWITCH so the evicted incumbent
                                        // redials + rejoins; off (default) → PREEMPT 0x9.
                                        let close_code = if mirror.load(Ordering::Acquire) {
                                            MODE_SWITCH_CLOSE_CODE
                                        } else {
                                            PREEMPT_CLOSE_CODE
                                        };
                                        conn.close(close_code.into(), b"preempted");
                                        pending_incoming = Some(inc);
                                        end_reason = LoopEnd::Preempted;
                                        break 'stream;
                                    }
                                }
                            }
                            r = &mut send_fut => {
                                if let Err(e) = r {
                                    end_reason = LoopEnd::SendError(e);
                                    break 'stream;
                                }
                                // Wall-clock guard: a slow (but completed) write surfaces the
                                // documented PHAROS_STREAM_WINDOW_KB flow-control cap as a named
                                // log line, with chunk size + window setting for one-grep triage.
                                let write_ms = write_t0.elapsed().as_millis();
                                if write_ms > 200 {
                                    let win_kb: String = match stream_window_pin() {
                                        Some(kb) => kb.to_string(),
                                        None => "adaptive".into(),
                                    };
                                    tracing::warn!(
                                        "quic write_all slow ms={write_ms} stream_window_kb={win_kb} chunk_bytes={chunk_bytes}"
                                    );
                                }
                                break;
                            }
                        }
                    }
                }
                // Audio: each Opus packet as one unreliable datagram, tagged so
                // the client can tell it from gyro telemetry. Drops on congestion
                // (no retransmit) and never blocks the video stream.
                Some(pkt) = audio_rx.recv() => {
                    if let Err(e) = conn.send_datagram(tagged_datagram(DG_AUDIO, &pkt)) {
                        tracing::debug!("audio datagram dropped: {e}");
                    }
                }
                // Gyro telemetry: 12-byte sample, tagged, for the calibration UI.
                Some(pkt) = gyro_rx.recv() => {
                    if let Err(e) = conn.send_datagram(tagged_datagram(DG_GYRO, &pkt)) {
                        tracing::debug!("gyro datagram dropped: {e}");
                    }
                }
            }
        }

        connected.store(false, Ordering::Release);
        unregister_session(&conn);
        if let Ok(mut g) = peer_pubkey.lock() {
            *g = None;
        }
        if let Ok(mut g) = peer_addr.lock() {
            *g = None;
        }
        // Clear the gyro opt-in too: the next client's auth will re-set it.
        // Without this, a v1 client connecting after a v2 wants_gyro=true
        // session would inherit the previous flag.
        wants_gyro.store(false, Ordering::Release);
        // Same for the HDR opt-in — the next client's v4 auth re-sets it.
        wants_hdr.store(false, Ordering::Release);
        // Same for the refresh cap — clear it so the next client (or a v2 client
        // that reports none) doesn't inherit this one's panel refresh.
        client_refresh_hz.store(0, Ordering::Release);
        // And the panel native res, so a v2 client (no StreamMode w/h) connecting
        // after this one doesn't inherit a stale panel size.
        client_panel.store(0, Ordering::Release);
        // And the resolution preference, so the next client starts at Auto.
        client_res_pref.store(0, Ordering::Release);
        // And the bitrate preference, so the next client starts at the host default.
        client_bitrate_pref.store(0, Ordering::Release);
        // And the framerate preference, so the next client starts at Auto/panel.
        client_fps_pref.store(0, Ordering::Release);
        probe.reset();
        match end_reason {
            LoopEnd::CompositorClosed => {
                send.finish().ok();
                conn.closed().await;
                return Ok(());
            }
            LoopEnd::PeerDropped => {
                tracing::info!("peer dropped; awaiting next client");
            }
            LoopEnd::Preempted => {
                tracing::info!("session pre-empted by a new client; handing over");
                // The incumbent conn was already closed in the pre-emption arm;
                // the stashed Incoming is served on the next loop turn.
            }
            LoopEnd::SendError(e) => {
                tracing::info!("send error ({e:?}); awaiting next client");
                let _ = send.reset(0u32.into());
                conn.close(0u32.into(), b"send err");
            }
        }
    }
}

/// Client: connect to a Pharos QUIC streamer and read the whole HEVC stream
/// (until the host finishes). Returns the received bytes. For verification.
/// Passes `expected_pubkey: None` so the pinning step is skipped (TOFU).
pub fn fetch_stream(server_addr: SocketAddr, max: usize) -> Result<Vec<u8>> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let mut client = Endpoint::client((Ipv4Addr::UNSPECIFIED, 0).into())?;
        client.set_default_client_config(client_config()?);
        let conn = client.connect(server_addr, "localhost")?.await?;
        do_video_auth(
            &conn,
            None,
            None,
            false,
            false,
            core_proto::StreamMode::default(),
        )
        .await?;
        let mut recv = conn.accept_uni().await?;
        // De-frame the length-prefixed access units back into a raw HEVC stream.
        let mut out = Vec::new();
        let mut len_buf = [0u8; 4];
        while recv.read_exact(&mut len_buf).await.is_ok() {
            let len = u32::from_be_bytes(len_buf) as usize;
            if out.len() + len > max {
                anyhow::bail!("stream exceeds max size");
            }
            let mut buf = vec![0u8; len];
            recv.read_exact(&mut buf).await?;
            out.extend_from_slice(&buf);
        }
        Ok(out)
    })
}

/// Connect to a Pharos QUIC streamer and deliver each access unit (a complete
/// encoded frame) to `on_frame` as it arrives, until the host finishes.
/// Blocking. The Trireme client calls this (via UniFFI) to feed MediaCodec.
///
/// `expected_pubkey` — Some(host_pub) pins to a previously paired key;
/// any other key (or relay-attack signature) fails the connect. None falls
/// back to TOFU (still validates the signature, but accepts any host_pub).
///
/// `client_identity` — Some(id) presents and proves our long-term identity so
/// an enforcing host can authorize us against its peers list; None connects
/// anonymously (admitted only by a permissive host).
pub fn run_video_client<F: FnMut(Vec<u8>)>(
    server_addr: SocketAddr,
    expected_pubkey: Option<[u8; 32]>,
    client_identity: Option<&core_crypto::Identity>,
    mut on_frame: F,
    // Phase 2: an optional one-shot resolution preference (target_h, floor_h) the
    // probe sends right after auth so the headless `decode` path can exercise the
    // client→host res ladder. `None` = don't send (plain probe).
    res_pref: Option<core_proto::ClientResPref>,
    // Phase 3: an optional one-shot bitrate preference (min,max kbps; max=0=uncapped)
    // for the same headless test path. `None` = don't send.
    bitrate_pref: Option<core_proto::ClientBitratePref>,
    // Phase 3: an optional one-shot framerate preference (min,max fps; max=0=Auto)
    // for the same headless test path. `None` = don't send.
    fps_pref: Option<core_proto::ClientFpsPref>,
) -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let mut client = Endpoint::client((Ipv4Addr::UNSPECIFIED, 0).into())?;
        client.set_default_client_config(client_config()?);
        let conn = client.connect(server_addr, "localhost")?.await?;
        do_video_auth(
            &conn,
            expected_pubkey.as_ref(),
            client_identity,
            false,
            false,
            core_proto::StreamMode::default(),
        )
        .await?;
        // Announce the requested resolution range a few times (unreliable datagram;
        // the host applies it idempotently on its ~1s window poll).
        if let Some(pref) = res_pref {
            if let Ok(b) = core_proto::latency::encode_client_res_pref(&pref) {
                for _ in 0..5 {
                    let _ = conn.send_datagram(tagged_datagram(DG_CLIENT_RESPREF, &b));
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
        if let Some(pref) = bitrate_pref {
            if let Ok(b) = core_proto::latency::encode_client_bitrate_pref(&pref) {
                for _ in 0..5 {
                    let _ = conn.send_datagram(tagged_datagram(DG_CLIENT_BITRATEPREF, &b));
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
        if let Some(pref) = fps_pref {
            if let Ok(b) = core_proto::latency::encode_client_fps_pref(&pref) {
                for _ in 0..5 {
                    let _ = conn.send_datagram(tagged_datagram(DG_CLIENT_FPSPREF, &b));
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
        // Deliberate host closes (preempt / reserved-by-primary / unpaired,
        // issue #19) land as application close codes — map them to their
        // stable errors here exactly like `run_video_client_with_input`, both
        // on the accept (a refusal arrives before any stream opens) and at the
        // end of the read loop.
        let mut recv = match conn.accept_uni().await {
            Ok(r) => r,
            Err(e) => match app_close_error(&conn) {
                Some(err) => return Err(err),
                None => return Err(e.into()),
            },
        };
        let mut len_buf = [0u8; 4];
        let mut mid_frame_err: Option<anyhow::Error> = None;
        while recv.read_exact(&mut len_buf).await.is_ok() {
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut buf = vec![0u8; len];
            // Record (don't `?`) a mid-frame error so a close that lands
            // between the length prefix and the body still reaches the
            // close-reason mapping below.
            if let Err(e) = recv.read_exact(&mut buf).await {
                mid_frame_err = Some(e.into());
                break;
            }
            on_frame(buf);
        }
        if let Some(err) = app_close_error(&conn) {
            return Err(err);
        }
        if let Some(err) = mid_frame_err {
            return Err(err);
        }
        Ok(())
    })
}

/// Same as [`run_video_client`] but also opens a reliable bidi stream over the
/// same connection for client → host input events. The trireme player drives
/// this — capture winit events, send through `input_rx`, the host's compositor
/// dispatches via the smithay seat. Pass `None` to opt out (equivalent to the
/// plain `run_video_client`).
///
/// `on_audio` — when `Some`, each Opus packet (carried as an unreliable QUIC
/// datagram) is handed over for decode + playback; `None` ignores audio (the
/// host's datagrams are simply dropped, no blocking).
///
/// `on_gyro` — when `Some`, each live gyro telemetry sample (gx, gy, gz in
/// deg/s) is delivered for the calibration visualizer. Rides the same tagged
/// datagram channel as audio.
///
/// `latency_taps` — when `Some`, an on-demand latency benchmark profiles the LIVE
/// stream. The benchmark is *driven by the host*: the moment the host starts
/// emitting `DG_FRAME_META` (which it only does inside a bench window) the client
/// runs a short clock burst for RTT, accumulates the host stage stamps straight off
/// each meta (pure host-clock subtractions), reads the player's decode min/mean +
/// present mean from the taps, and sends back a [`core_proto::WireLatencyReport`].
/// When `None` the host + wire stages are still computed (decode/buffer/present left
/// 0.0). Either way this is **completely inert until the host begins a bench** — no
/// pings, no report, identical wire behaviour to a plain session.
/// Sink for decoded Opus audio packets (one per QUIC datagram).
pub type AudioSink = Box<dyn FnMut(Vec<u8>) + Send>;
/// Sink for live gyro telemetry samples (gx, gy, gz in deg/s).
pub type GyroSink = Box<dyn FnMut(f32, f32, f32) + Send>;

#[allow(clippy::too_many_arguments)]
pub fn run_video_client_with_input<F: FnMut(Vec<u8>)>(
    server_addrs: Vec<SocketAddr>,
    expected_pubkey: Option<[u8; 32]>,
    client_identity: Option<&core_crypto::Identity>,
    mut on_frame: F,
    input_rx: Option<mpsc::UnboundedReceiver<core_proto::InputEvent>>,
    mut on_audio: Option<AudioSink>,
    mut on_gyro: Option<GyroSink>,
    cancel: Option<Arc<AtomicBool>>,
    latency_taps: Option<Arc<LatencyTaps>>,
    on_catalog: Option<Box<dyn FnMut(Vec<core_proto::AppInfo>) + Send>>,
    // Host→client transient status notices (toasts) on the app relay — e.g. the
    // auto Steam close+relaunch note. `None` = the client ignores notices.
    on_app_notice: Option<Box<dyn FnMut(String) + Send>>,
    app_launch_rx: Option<mpsc::UnboundedReceiver<core_proto::AppMsg>>,
    wants_gyro: bool,
    // Whether the client wants a 10-bit HDR (Main10) stream — sent in the v4 auth
    // handshake so the host gates HDR on client capability. Threaded from the FFI;
    // currently always false until the client HDR toggle is wired (task 6).
    wants_hdr: bool,
    // Host→client link-health: invoked with `true` while the host is shedding
    // frames, `false` when clean (DG_LINK_HEALTH heartbeat ~2×/s). The app drives a
    // "degraded" indicator from it. `None` = ignore (older callers).
    mut on_link_health: Option<Box<dyn FnMut(bool) + Send>>,
    // Client→host on-demand IDR request: when the app sets this (e.g. its decoder
    // hit a corrupt-stream error), the loop sends one DG_REQUEST_IDR and clears it.
    // A backstop to the host's own drop-driven IDR; `None` = never request.
    request_idr: Option<Arc<AtomicBool>>,
    // Client→host requested stream mode (resolution + panel refresh), sent in the
    // v3 auth handshake. `default()` (all zeros) = Auto/unknown → host keeps its
    // native res and doesn't cap fps.
    requested_mode: core_proto::StreamMode,
    // Phase 2: the client's LIVE resolution range, packed (target_h<<32)|floor_h
    // (0 = Auto + no floor). The app mutates this atomic from its settings UI; the
    // loop sends it on each QoS tick as `DG_CLIENT_RESPREF` so the host adapts the
    // ladder within [floor, target]. `None` = never send (Auto).
    res_pref: Option<Arc<std::sync::atomic::AtomicU64>>,
    // Phase 3: the client's LIVE bitrate range, packed (min_kbps<<32)|max_kbps
    // (max=0 = uncapped). Mutated from the settings UI; re-sent each QoS tick as
    // `DG_CLIENT_BITRATEPREF`. `None` = never send.
    bitrate_pref: Option<Arc<std::sync::atomic::AtomicU64>>,
    // Phase 3: the client's LIVE framerate range, packed (min_fps<<32)|max_fps
    // (max=0 = Auto/panel). Mutated from the settings UI; re-sent each QoS tick as
    // `DG_CLIENT_FPSPREF`. `None` = never send.
    fps_pref: Option<Arc<std::sync::atomic::AtomicU64>>,
    // The client's link-transport hint (core_proto TRANSPORT_*), reported in each
    // v2 QoS tick so the host can bound its bandwidth-probe ceiling (issue #40).
    // Live (an Android network callback may flip WiFi↔Ethernet mid-session);
    // `None` = always TRANSPORT_UNKNOWN.
    transport_hint: Option<Arc<std::sync::atomic::AtomicU8>>,
    // Host→client clipboard: invoked with the text a host app just copied so the
    // client mirrors it into its OS clipboard (empty string = the host cleared its
    // clipboard). `None` = the client doesn't sync host→client.
    on_clipboard: Option<Box<dyn FnMut(String) + Send>>,
    // Client→host clipboard: the client pushes text it copied here; the loop writes
    // it on the clipboard relay stream and the host becomes the Wayland selection
    // owner. `None` = the client doesn't sync client→host.
    clipboard_rx: Option<mpsc::UnboundedReceiver<core_proto::ClipboardMsg>>,
) -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        // Bind dual-stack only when a candidate is IPv6, so IPv4-only sessions keep
        // the proven `0.0.0.0:0` socket. v4 candidates are v4-mapped for the
        // dual-stack socket; v6 candidates dial natively.
        let need_v6 = server_addrs.iter().any(|a| a.is_ipv6());
        let mut client = client_endpoint(need_v6)?;
        client.set_default_client_config(client_config()?);
        let dial_addrs: Vec<SocketAddr> = server_addrs
            .iter()
            .map(|a| to_dialable(*a, need_v6))
            .collect();
        let conn = connect_first(&client, &dial_addrs).await?;
        do_video_auth(
            &conn,
            expected_pubkey.as_ref(),
            client_identity,
            wants_gyro,
            wants_hdr,
            requested_mode,
        )
        .await?;

        // Open the input bidi stream up front (when requested) so the server
        // sees it before the first event needs to flow. The read half stays
        // open but idle — we never receive from the host on this stream.
        //
        // Under backpressure (a wedged client present or a network stall) we must
        // NOT let the unbounded `input_rx` grow for minutes and later replay stale
        // input into the live session (#13). So a COALESCER drains `input_rx`
        // promptly into a bounded `Staging` buffer (motion coalesced, transitions
        // kept in order), and a separate WRITER pops compacted items and
        // `write_all`s them uninterrupted. `write_all` is not cancel-safe, so the
        // two are split across tasks rather than `select!`ed against a `recv()` —
        // a partial length-prefix write must never be dropped. If the writer stays
        // stuck (transition cap or age bound), the coalescer trips shutdown and
        // closes the WHOLE connection (INPUT_STUCK_CLOSE_CODE) so the video +
        // datagram loops error out, `run_video_client_with_input` returns Err, and
        // the client reconnects fresh (rather than sitting on a live-but-muted
        // stream). On a clean teardown (`input_rx` closes) the coalescer sets a
        // `closing` flag and the writer flushes the remainder and `finish()`es the
        // stream with a real FIN. `_input_send` is an abort-on-drop guard so this
        // inner task never leaks across a reconnect.
        let mut _input_send: Option<AbortOnDrop> = None;
        if let Some(mut input_rx) = input_rx {
            let (mut send, _recv) = conn.open_bi().await?;
            // Nudge the stream open so accept_bi() fires on the server even
            // before the first real event (writes are buffered until needed).
            send.write_all(&[]).await.ok();
            let conn_input = conn.clone();
            let handle = tokio::spawn(async move {
                let staging = Arc::new(tokio::sync::Mutex::new(Staging::new()));
                let notify = Arc::new(tokio::sync::Notify::new());

                // Coalescer: owns `input_rx`, never touches the network, so the
                // channel is always drained promptly (invariant 4). Folds each
                // event (plus a non-blocking `try_recv` burst drain) into the
                // shared staging, then wakes the writer.
                let co_staging = staging.clone();
                let co_notify = notify.clone();
                let co_conn = conn_input.clone();
                let coalescer = async move {
                    while let Some(ev) = input_rx.recv().await {
                        let mut s = co_staging.lock().await;
                        s.fold(ev);
                        // Drain whatever else is already queued in one shot so a
                        // burst collapses into the staging before we ever wake the
                        // writer; keeps `input_rx` from backing up under a stall.
                        while let Ok(ev) = input_rx.try_recv() {
                            s.fold(ev);
                        }
                        let stuck = s.check_stuck();
                        drop(s);
                        co_notify.notify_one();
                        if stuck {
                            // Link is genuinely dead. Tear the WHOLE session down so
                            // the client reconnects fresh: closing the connection
                            // errors out the video + datagram read loops, so
                            // `run_video_client_with_input` returns Err and the
                            // normal StreamEnded → reconnect path fires. The code is
                            // deliberately NOT one of the host's stop codes, so the
                            // client treats it as an ordinary drop, not a refusal.
                            co_conn.close(INPUT_STUCK_CLOSE_CODE.into(), b"input stuck");
                            return;
                        }
                    }
                    // `input_rx` closed (client tearing down): flag a graceful close
                    // and wake the writer so it flushes the remainder and `finish()`es
                    // the stream with a real FIN (not a dropped-stream RESET_STREAM).
                    // Then park on the connection instead of returning, so the WRITER
                    // arm (not this one) wins the outer `select!` and actually reaches
                    // its flush + `finish()`; if the link drops first this still ends.
                    {
                        let mut s = co_staging.lock().await;
                        s.closing = true;
                    }
                    co_notify.notify_one();
                    co_conn.closed().await;
                };

                // Writer: pops one compacted item under the lock, releases it, then
                // `write_all`s uninterrupted (never inside a `select!`, so a partial
                // length-prefix write is never cancelled). On the stuck path it just
                // returns (the coalescer has already closed the connection to force a
                // reconnect). On a clean close (`closing` set with the staging
                // drained) it flushes the remainder and `finish()`es the stream with
                // a real FIN so no staged item is silently dropped.
                let wr_staging = staging.clone();
                let wr_notify = notify.clone();
                let writer = async move {
                    let mut send = send;
                    loop {
                        let (next, closing) = {
                            let mut s = wr_staging.lock().await;
                            if s.shutdown {
                                // Genuinely-stuck link: the coalescer has closed the
                                // whole connection, so just end this task; the session
                                // tears down and reconnects rather than replaying
                                // minutes-late input.
                                tracing::warn!(
                                    "input send stuck (backlog/age bound); tearing down"
                                );
                                return;
                            }
                            (s.pop_front(), s.closing)
                        };
                        let Some(ev) = next else {
                            if closing {
                                // Clean teardown and nothing left staged: end the
                                // stream with a graceful FIN instead of a dropped
                                // RESET_STREAM.
                                send.finish().ok();
                                return;
                            }
                            // Nothing staged: park until the coalescer wakes us. If
                            // the coalescer has already exited (channel closed), the
                            // outer select's `coalescer` arm ends the pair, so this
                            // never parks forever.
                            wr_notify.notified().await;
                            continue;
                        };
                        let buf = match core_proto::encode_input(&ev) {
                            Ok(b) => b,
                            Err(e) => {
                                tracing::warn!("encode input: {e}");
                                continue;
                            }
                        };
                        let len_be = (buf.len() as u32).to_be_bytes();
                        if send.write_all(&len_be).await.is_err()
                            || send.write_all(&buf).await.is_err()
                        {
                            return; // stream/connection gone
                        }
                    }
                };

                // Both run on this one spawned task; whichever finishes first ends
                // the pair. On a stuck link the coalescer closes the connection and
                // returns (the writer sees `shutdown` and ends, and `conn_closed`
                // fires too). On a clean close the coalescer flags `closing` and then
                // parks on the connection, so the WRITER arm wins the select, flushes
                // the remainder, and `finish()`es the stream with a real FIN. A
                // watcher on the connection ends the pair even if `input_rx` never
                // closes but the link dropped. `write_all` is never inside this
                // `select!`, so no partial length-prefix write is ever cancelled.
                let watch_conn = conn_input.clone();
                let conn_closed = async move {
                    watch_conn.closed().await;
                };
                tokio::select! {
                    _ = coalescer => {}
                    _ = writer => {}
                    _ = conn_closed => {}
                }
            });
            _input_send = Some(AbortOnDrop(handle));
        }

        // App relay + clipboard relay: the host opens TWO dedicated bidi streams,
        // the app relay first (server-bidi idx0) then the clipboard relay (idx1).
        // We accept them IN THAT ORDER inside one task (quinn yields accepts in
        // ascending stream-id = open order) so the clipboard stream is demuxed
        // correctly. The app relay is always accepted first — even by a client that
        // only wants clipboard — so the ordering holds; its handlers just idle when
        // no catalog/launch is wired. An old host never opens idx1, so the clipboard
        // accept simply parks (harmless).
        let want_clipboard = on_clipboard.is_some() || clipboard_rx.is_some();
        if on_catalog.is_some()
            || on_app_notice.is_some()
            || app_launch_rx.is_some()
            || want_clipboard
        {
            let conn_app = conn.clone();
            tokio::spawn(async move {
                let (send, recv) = match conn_app.accept_bi().await {
                    Ok(s) => s,
                    Err(_) => return, // host never opened it / connection ended
                };
                // Accept the clipboard relay (idx1) right after the app relay (idx0),
                // in this same task, then run it as a sub-task. Only when this client
                // syncs clipboard; otherwise we never accept it (an app-only client).
                if want_clipboard {
                    let conn_clip = conn_app.clone();
                    let mut on_clipboard = on_clipboard;
                    let mut clipboard_rx = clipboard_rx;
                    tokio::spawn(async move {
                        let (clip_send, clip_recv) = match conn_clip.accept_bi().await {
                            Ok(s) => s,
                            Err(_) => return, // old host never opened it
                        };
                        // Writer: client → host copies.
                        let write = async move {
                            let mut clip_send = clip_send;
                            if let Some(mut rx) = clipboard_rx.take() {
                                while let Some(msg) = rx.recv().await {
                                    let buf = match core_proto::encode_clipboard(&msg) {
                                        Ok(b) => b,
                                        Err(e) => {
                                            tracing::warn!("encode clipboard: {e}");
                                            continue;
                                        }
                                    };
                                    let len_be = (buf.len() as u32).to_be_bytes();
                                    if clip_send.write_all(&len_be).await.is_err()
                                        || clip_send.write_all(&buf).await.is_err()
                                    {
                                        break;
                                    }
                                }
                            }
                            clip_send.finish().ok();
                        };
                        // Reader: host → client copies.
                        let read = async move {
                            let mut clip_recv = clip_recv;
                            let mut len_buf = [0u8; 4];
                            loop {
                                if clip_recv.read_exact(&mut len_buf).await.is_err() {
                                    return;
                                }
                                let len = u32::from_be_bytes(len_buf) as usize;
                                if len == 0 {
                                    continue; // host's stream-open nudge
                                }
                                if len > core_proto::MAX_CLIPBOARD_FRAME {
                                    tracing::warn!(
                                        "clipboard frame too large ({len} bytes); dropping"
                                    );
                                    return;
                                }
                                let mut buf = vec![0u8; len];
                                if clip_recv.read_exact(&mut buf).await.is_err() {
                                    return;
                                }
                                if let Ok(core_proto::ClipboardMsg::Text(t)) =
                                    core_proto::decode_clipboard(&buf)
                                {
                                    if let Some(cb) = on_clipboard.as_mut() {
                                        cb(t);
                                    }
                                }
                            }
                        };
                        tokio::join!(write, read);
                    });
                }
                // Writer: client → host app actions (Launch/SwitchTo/EndProcess).
                let write = async move {
                    let mut send = send;
                    if let Some(mut rx) = app_launch_rx {
                        while let Some(msg) = rx.recv().await {
                            let buf = match core_proto::encode_app(&msg) {
                                Ok(b) => b,
                                Err(e) => {
                                    tracing::warn!("encode app msg: {e}");
                                    continue;
                                }
                            };
                            let len_be = (buf.len() as u32).to_be_bytes();
                            if send.write_all(&len_be).await.is_err()
                                || send.write_all(&buf).await.is_err()
                            {
                                break;
                            }
                        }
                    }
                    // Signal EOF to the host's launch reader on teardown (matches
                    // the input stream's finish()).
                    send.finish().ok();
                };
                // Reader: host → client catalog.
                let read = async move {
                    let mut recv = recv;
                    let mut on_catalog = on_catalog;
                    let mut on_app_notice = on_app_notice;
                    let mut len_buf = [0u8; 4];
                    loop {
                        if recv.read_exact(&mut len_buf).await.is_err() {
                            return;
                        }
                        let len = u32::from_be_bytes(len_buf) as usize;
                        // Catalogs carry box-art bytes, so allow a few MB.
                        if len > 16 * 1024 * 1024 {
                            tracing::warn!("app catalog too large ({len} bytes); dropping");
                            return;
                        }
                        let mut buf = vec![0u8; len];
                        if recv.read_exact(&mut buf).await.is_err() {
                            return;
                        }
                        match core_proto::decode_app(&buf) {
                            Ok(core_proto::AppMsg::Catalog { apps }) => {
                                if let Some(cb) = on_catalog.as_mut() {
                                    cb(apps);
                                }
                            }
                            Ok(core_proto::AppMsg::Notice { message }) => {
                                if let Some(cb) = on_app_notice.as_mut() {
                                    cb(message);
                                }
                            }
                            _ => {} // client→host variants never arrive here
                        }
                    }
                };
                tokio::join!(write, read);
            });
        }

        // The bench lives in the datagram loop alone now: host stages come straight
        // off each FrameStageMeta (pure host-clock subtractions), the wire stage is
        // RTT-derived, and the client stages are read from the player taps. There's
        // no cross-clock frame join, so the video loop no longer has to stamp AU
        // receive instants — it just forwards bytes. Wrapped in `Rc<RefCell<_>>`
        // only because the datagram loop borrows it across `.await` points.
        // The bench owns the taps; keep a clone so the datagram loop can stash the
        // host-announced geometry into the same shared taps (read by the decoder).
        let geom_taps = latency_taps.clone();
        let bench = std::rc::Rc::new(std::cell::RefCell::new(BenchState::new(latency_taps)));
        // Frames received since the last QoS report — the video task bumps it, the
        // datagram task drains it ~1×/s into a `DG_CLIENT_QOS` report so the host
        // can see the delivered rate (both blocks run on this one task, so a shared
        // atomic suffices — no lock, no spawn bound).
        let recv_count = std::sync::atomic::AtomicU32::new(0);
        // Video bytes received this QoS window — feeds the adaptive receive-window
        // sizing (the receiver-side latency bound; see `transport_config`).
        let recv_bytes = std::sync::atomic::AtomicU64::new(0);
        // Largest AU seen this QoS window — the window must always be able to
        // swallow a whole keyframe (see `adaptive_window_target`).
        let recv_max_au = std::sync::atomic::AtomicU64::new(0);
        // ALL bytes received this QoS window — video stream AND datagrams
        // (audio, probe padding…). Reported as ClientQosV2.recv_kbps: the
        // delivered-goodput measurement the host's bandwidth probe reads
        // (issue #40). Separate from `recv_bytes` so probe padding never
        // inflates the adaptive receive-window sizing.
        let qos_recv_bytes = std::sync::atomic::AtomicU64::new(0);

        // Video on the uni stream; audio as unreliable datagrams. Both run
        // concurrently on this one task (no spawn → no Send/'static bound on the
        // callbacks); the video stream ending = session over.
        let video = async {
            // A RESERVED/UNPAIRED refusal (issue #19) lands right after auth,
            // BEFORE the host ever opens the video stream — so the close-code
            // mapping must cover the accept itself, not just the read loop's end.
            let mut recv = match conn.accept_uni().await {
                Ok(r) => r,
                Err(e) => match app_close_error(&conn) {
                    Some(err) => return Err(err),
                    None => return Err(e.into()),
                },
            };
            let mut len_buf = [0u8; 4];
            while recv.read_exact(&mut len_buf).await.is_ok() {
                let len = u32::from_be_bytes(len_buf) as usize;
                let mut buf = vec![0u8; len];
                // Break (don't `?`) on a mid-frame error so we always reach the
                // close-reason check below — a host pre-emption can land between
                // the length prefix and the body just as easily as between frames.
                if recv.read_exact(&mut buf).await.is_err() {
                    break;
                }
                on_frame(buf);
                recv_count.fetch_add(1, Ordering::Relaxed);
                recv_bytes.fetch_add(4 + len as u64, Ordering::Relaxed);
                recv_max_au.fetch_max(len as u64, Ordering::Relaxed);
                qos_recv_bytes.fetch_add(4 + len as u64, Ordering::Relaxed);
            }
            // The stream ended. If the host CLOSED us with one of its deliberate
            // application codes (preempt / reserved-by-primary / unpaired),
            // surface the matching stable error so the client can YIELD, back
            // off, or stop instead of instantly redialing. An ordinary drop /
            // idle-timeout leaves close_reason None or a different code, so the
            // normal fast reconnect path is untouched.
            if let Some(err) = app_close_error(&conn) {
                return Err(err);
            }
            anyhow::Ok(())
        };
        // One datagram loop dispatches by type tag — two readers would race the
        // shared datagram queue. Always runs: besides audio/gyro it also carries
        // the latency-bench datagrams (FRAME_META in, CLOCK_PONG in, and it sends
        // the CLOCK_PING burst + final report). Cheap when idle — just a read loop.
        let bench_dg = bench.clone();
        let datagrams = async {
            // Deadline that fires ~750ms after the last frame-meta so we can close
            // the bench window even though no datagram arrives to wake the loop.
            // Far in the future while no bench is active.
            let idle_timer = tokio::time::sleep(std::time::Duration::from_secs(86_400));
            tokio::pin!(idle_timer);
            // Continuous QoS heartbeat: report delivered fps + decode-ms to the host
            // ~1×/s so it can pace to the actual path. The first tick fires at once;
            // a recv_fps of 0 right at connect is benign (the host's pacer ignores
            // QoS until it has seen real frames).
            let mut qos_tick = tokio::time::interval(std::time::Duration::from_secs(1));
            qos_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Fast poll for a pending on-demand IDR request. A decode error sets the
            // `request_idr` flag; we want the DG_REQUEST_IDR on the wire in ~1 RTT,
            // NOT up to a full QoS tick (≤1s) later — otherwise the on-demand path is
            // no faster than the periodic ~1s IDR it's meant to beat. 100ms keeps the
            // worst case to ~0.1s + RTT, the empty datagrams are negligible, and the
            // host coalesces forced IDRs via PHAROS_MIN_IDR_INTERVAL_MS (no storm).
            let mut idr_tick = tokio::time::interval(std::time::Duration::from_millis(100));
            idr_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // Adaptive receive window (see `transport_config`): resize the
            // connection-level flow-control window each QoS tick to ~250ms at the
            // measured video rate — the receiver-side latency bound that replaces
            // the old one-size 256KB static window. EWMA is fast-up / slow-down:
            // a burst after an idle desktop re-expands the window within one tick,
            // while a rate drop (bitrate brake, idle) decays it over a few seconds
            // so the bound tightens without thrashing. Inert when
            // PHAROS_STREAM_WINDOW_KB pins a fixed size.
            let window_adaptive = stream_window_pin().is_none();
            let mut rate_ewma: f32 = 0.0;
            let mut window_applied: u32 = WINDOW_START_BYTES;
            // Rolling per-tick maxima of the largest AU, spanning ~5s — longer
            // than the host's 4s IDR period, so the window never shrinks below
            // keyframe size just because the last second happened to hold none.
            let mut au_maxes = [0u64; 5];
            let mut au_ix = 0usize;
            // Wall-clock of the previous QoS tick, so recv_kbps is normalized by
            // the REAL elapsed window (interval ticks can stretch under load —
            // a probe measurement mustn't inflate from a short window).
            let mut last_qos_at = std::time::Instant::now();
            loop {
                tokio::select! {
                    biased;
                    dg = conn.read_datagram() => {
                        let Ok(dg) = dg else { break };
                        // Goodput accounting BEFORE tag dispatch: probe padding
                        // (DG_PROBE_PAD) and every other datagram count toward
                        // the delivered rate the host's bandwidth probe measures.
                        qos_recv_bytes.fetch_add(dg.len() as u64, Ordering::Relaxed);
                        match dg.first().copied() {
                            Some(DG_AUDIO) => {
                                if let Some(cb) = on_audio.as_mut() {
                                    cb(dg[1..].to_vec());
                                }
                            }
                            Some(DG_GYRO) => {
                                if let (Some(cb), true) = (on_gyro.as_mut(), dg.len() >= 13) {
                                    let gx = f32::from_le_bytes([dg[1], dg[2], dg[3], dg[4]]);
                                    let gy = f32::from_le_bytes([dg[5], dg[6], dg[7], dg[8]]);
                                    let gz = f32::from_le_bytes([dg[9], dg[10], dg[11], dg[12]]);
                                    cb(gx, gy, gz);
                                }
                            }
                            Some(DG_STREAM_GEOMETRY) if dg.len() >= 9 => {
                                let w = u32::from_be_bytes([dg[1], dg[2], dg[3], dg[4]]);
                                let h = u32::from_be_bytes([dg[5], dg[6], dg[7], dg[8]]);
                                if let Some(t) = &geom_taps {
                                    t.set_geometry(w, h);
                                }
                            }
                            Some(DG_STREAM_HDR) if dg.len() >= 2 => {
                                // Host colorimetry announce (issue #16): 1 = HDR,
                                // 0 = SDR. Stash it in the shared taps; the desktop
                                // player reads it to decide whether to run the
                                // HDR→SDR tonemap, and rebuilds its decode chain on a
                                // flip. Stored in the SAME taps as the geometry (it
                                // reuses `geom_taps`, kept for exactly this).
                                if let Some(t) = &geom_taps {
                                    t.set_stream_hdr(dg[1] != 0);
                                }
                            }
                            Some(DG_CURSOR_MODE) if dg.len() >= 2 => {
                                // Host cursor-mode announce: 1 = CAPTURED (the game
                                // grabbed the mouse for relative mouselook), 0 = FREE
                                // (absolute pointer over a menu/desktop). Stash it in
                                // the shared taps; the desktop player reads it each
                                // frame and mirrors it into its own OS cursor grab so
                                // the client auto-locks during gameplay and frees in
                                // menus. Stored in the SAME taps as the geometry/HDR.
                                if let Some(t) = &geom_taps {
                                    t.set_cursor_captured(dg[1] != 0);
                                }
                            }
                            Some(DG_LINK_HEALTH) if dg.len() >= 2 => {
                                // Host heartbeat: surface degraded/clean to the app's
                                // indicator. Cheap, non-blocking (just a flag flip).
                                if let Some(cb) = on_link_health.as_mut() {
                                    cb(dg[1] != 0);
                                }
                            }
                            Some(DG_FRAME_META) => {
                                if let Ok(meta) = core_proto::latency::decode_frame_meta(&dg[1..]) {
                                    let first = bench_dg.borrow_mut().on_frame_meta(meta);
                                    if first {
                                        // Host just opened a bench window. Fire the
                                        // clock burst so offset/RTT are ready before
                                        // the report, and (re)arm the idle deadline.
                                        send_clock_burst(&conn).await;
                                    }
                                    idle_timer.as_mut().reset(
                                        tokio::time::Instant::now()
                                            + std::time::Duration::from_millis(BENCH_IDLE_MS),
                                    );
                                }
                            }
                            Some(DG_CLOCK_PONG) => {
                                if let Ok(pong) = core_proto::latency::decode_clock_pong(&dg[1..]) {
                                    let t3 = unix_nanos();
                                    bench_dg.borrow_mut().on_clock_pong(pong, t3);
                                }
                            }
                            _ => {}
                        }
                        // A hard cap also ends the window (busy stream, deadline never
                        // idles): finalize + send the report inline. Build the report
                        // and DROP the borrow before awaiting the send — a borrow held
                        // across `.await` would risk a runtime double-borrow panic.
                        let at_cap = bench_dg.borrow().window_at_cap();
                        if at_cap {
                            let report = bench_dg.borrow_mut().finish();
                            if let Some(report) = report {
                                send_latency_report(&conn, &report).await;
                            }
                            idle_timer.as_mut().reset(
                                tokio::time::Instant::now()
                                    + std::time::Duration::from_secs(86_400),
                            );
                        }
                    }
                    _ = &mut idle_timer => {
                        // ~750ms since the last meta → the host stopped emitting →
                        // window over. Build + send the report, then disarm. Drop the
                        // borrow before awaiting (see above).
                        let report = bench_dg.borrow_mut().finish();
                        if let Some(report) = report {
                            send_latency_report(&conn, &report).await;
                        }
                        idle_timer.as_mut().reset(
                            tokio::time::Instant::now()
                                + std::time::Duration::from_secs(86_400),
                        );
                    }
                    _ = idr_tick.tick() => {
                        // On-demand IDR: a decode error (corrupt stream) set the flag;
                        // get the request on the wire promptly so the host forces a
                        // keyframe in ~1 RTT instead of waiting for the periodic ~1s
                        // IDR. Level flag (swap-to-false) — a persistent corruption
                        // keeps re-flagging it; a one-off lost datagram falls back to
                        // the periodic IDR. Empty payload: the host reads only the tag.
                        if let Some(flag) = &request_idr {
                            if flag.swap(false, Ordering::Relaxed) {
                                let _ = conn.send_datagram(tagged_datagram(DG_REQUEST_IDR, &[]));
                            }
                        }
                    }
                    _ = qos_tick.tick() => {
                        // Adaptive receive window: ~250ms at the measured video rate.
                        // Applied only on a >25% move so a steady stream never churns
                        // MAX_DATA frames.
                        if window_adaptive {
                            let bytes = recv_bytes.swap(0, Ordering::Relaxed) as f32;
                            rate_ewma = if bytes > rate_ewma {
                                bytes // fast up: react to a burst within one tick
                            } else {
                                0.5 * rate_ewma + 0.5 * bytes // slow down: decay over ~3-4s
                            };
                            au_maxes[au_ix] = recv_max_au.swap(0, Ordering::Relaxed);
                            au_ix = (au_ix + 1) % au_maxes.len();
                            let max_au = au_maxes.iter().copied().max().unwrap_or(0);
                            let target =
                                adaptive_window_target(rate_ewma, max_au.min(u64::from(u32::MAX)) as u32);
                            if target.abs_diff(window_applied).saturating_mul(4) > window_applied {
                                conn.set_receive_window(quinn::VarInt::from_u32(target));
                                tracing::debug!(
                                    "receive window → {}KB (video ~{:.1} Mbps)",
                                    target / 1024,
                                    rate_ewma * 8.0 / 1e6,
                                );
                                window_applied = target;
                            }
                        }
                        // ~1s window, so the drained count is ≈ delivered fps.
                        let recv_fps = recv_count.swap(0, Ordering::Relaxed) as f32;
                        let decode_ms =
                            geom_taps.as_ref().map(|t| t.decode_mean_ms()).unwrap_or(0.0);
                        // Delivered goodput over the REAL elapsed window (v2 —
                        // the host bandwidth probe's measurement, issue #40).
                        let elapsed = last_qos_at.elapsed().as_secs_f64().max(0.25);
                        last_qos_at = std::time::Instant::now();
                        let recv_kbps = ((qos_recv_bytes.swap(0, Ordering::Relaxed) as f64
                            * 8.0
                            / 1000.0
                            / elapsed) as u64)
                            .min(u64::from(u32::MAX)) as u32;
                        let transport = transport_hint
                            .as_ref()
                            .map(|t| t.load(Ordering::Relaxed))
                            .unwrap_or(core_proto::latency::TRANSPORT_UNKNOWN);
                        let qos = core_proto::ClientQosV2 {
                            recv_fps,
                            decode_ms,
                            recv_kbps,
                            transport,
                        };
                        if let Ok(b) = core_proto::latency::encode_client_qos_v2(&qos) {
                            let _ = conn.send_datagram(tagged_datagram(DG_CLIENT_QOS, &b));
                        }
                        // Phase 2: re-send the client's resolution range each tick
                        // (continuous, like QoS — a lost one self-heals; the host
                        // applies it idempotently and only acts on a change).
                        if let Some(rp) = &res_pref {
                            let v = rp.load(Ordering::Relaxed);
                            let pref = core_proto::ClientResPref {
                                target_h: (v >> 32) as u32,
                                floor_h: v as u32,
                            };
                            if let Ok(b) = core_proto::latency::encode_client_res_pref(&pref) {
                                let _ = conn.send_datagram(tagged_datagram(DG_CLIENT_RESPREF, &b));
                            }
                        }
                        // Phase 3: re-send the client's bitrate range each tick (same
                        // continuous self-healing discipline as the resolution range).
                        if let Some(bp) = &bitrate_pref {
                            let v = bp.load(Ordering::Relaxed);
                            let pref = core_proto::ClientBitratePref {
                                min_kbps: (v >> 32) as u32,
                                max_kbps: v as u32,
                            };
                            if let Ok(b) = core_proto::latency::encode_client_bitrate_pref(&pref) {
                                let _ =
                                    conn.send_datagram(tagged_datagram(DG_CLIENT_BITRATEPREF, &b));
                            }
                        }
                        // Phase 3: re-send the client's framerate range each tick (same
                        // continuous self-healing discipline as the other ranges).
                        if let Some(fp) = &fps_pref {
                            let v = fp.load(Ordering::Relaxed);
                            let pref = core_proto::ClientFpsPref {
                                min_fps: (v >> 32) as u16,
                                max_fps: v as u16,
                            };
                            if let Ok(b) = core_proto::latency::encode_client_fps_pref(&pref) {
                                let _ = conn.send_datagram(tagged_datagram(DG_CLIENT_FPSPREF, &b));
                            }
                        }
                    }
                }
            }
            anyhow::Ok(())
        };
        // A short-lived caller (e.g. the gyro calibration overlay) can ask us to
        // tear the connection down by flipping `cancel`; we poll it and bail,
        // closing the connection so the host returns to accept().
        let session = async {
            let (video_result, _dg_result) = tokio::join!(video, datagrams);
            video_result
        };
        match cancel {
            Some(flag) => {
                let watch = async {
                    while !flag.load(Ordering::Relaxed) {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                };
                tokio::select! {
                    r = session => r,
                    _ = watch => {
                        // Explicitly close so the host sees the drop immediately.
                        // `close()` only *queues* the CONNECTION_CLOSE frame; we
                        // must drive the connection until it's actually sent
                        // (`closed().await`), else the runtime drops first and the
                        // host only notices via QUIC idle-timeout — it reads as
                        // "still streaming" for ~30s.
                        conn.close(0u32.into(), b"client done");
                        conn.closed().await;
                        Ok(())
                    }
                }
            }
            None => session.await,
        }
    })
}

// --- Latency benchmark (client side) ---------------------------------------
//
// A one-shot, host-driven profile of the LIVE stream. The host opening a bench
// window = it starts sending DG_FRAME_META; that first meta is the start signal.
// The client then: (1) fires a short DG_CLOCK_PING burst to measure RTT (min and
// max over the burst), (2) accumulates the host stage stamps off each FrameStageMeta
// — pure host-clock subtractions, no cross-clock join — (3) reads the player taps
// for the client decode/buffer/present stages, (4) sends back one DG_LATENCY_REPORT.
// The wire stage is `rtt_min/2` (no frame-vs-clock join, no offset arithmetic). The
// window ends when the host stops emitting meta (idle gap) or a hard cap is hit;
// state then resets so a later bench can run again.

/// Idle gap after the last frame-meta that ends a bench window.
const BENCH_IDLE_MS: u64 = 750;
/// Hard frame cap on a window (also ~5s at 60fps) so a never-idling stream can't
/// run the bench forever.
const BENCH_MAX_FRAMES: usize = 300;
/// Number of clock-sync probes in the burst. Keep the min-RTT sample.
const CLOCK_BURST: u32 = 8;

/// One client-side latency-bench session. Lives in an `Rc<RefCell<_>>` borrowed by
/// the datagram loop. The host stage stamps are host-clock UNIX nanos, so every
/// host-side delta is a same-clock subtraction (offset-free); the clock burst is
/// kept purely for RTT (→ the wire stage) and a diagnostic offset.
struct BenchState {
    /// Player decode/present taps (None → those report fields stay 0.0). Reset at
    /// window open so each bench run is independent.
    taps: Option<Arc<LatencyTaps>>,
    /// True between the first frame-meta of a window and `finish()`.
    active: bool,
    /// RTT spread over the clock burst, nanos. `rtt_min` drives the wire stage
    /// (`nexus = rtt_min/2`); `(rtt_max - rtt_min)/2` is the uncertainty. None
    /// until the first pong lands.
    rtt_min_ns: Option<u64>,
    rtt_max_ns: u64,
    /// Diagnostic-only clock offset (host − client) from the min-RTT pong. Not
    /// used by any stage anymore.
    clock_offset_ns: i64,
    /// Running host-stage sums over the window (nanos) + the meta count. Host
    /// stages are pure host-clock subtractions, so no AU join is needed.
    compose_sum_ns: u64,
    encode_sum_ns: u64,
    host_send_sum_ns: u64,
    metas: u32,
}

impl BenchState {
    fn new(taps: Option<Arc<LatencyTaps>>) -> Self {
        Self {
            taps,
            active: false,
            rtt_min_ns: None,
            rtt_max_ns: 0,
            clock_offset_ns: 0,
            compose_sum_ns: 0,
            encode_sum_ns: 0,
            host_send_sum_ns: 0,
            metas: 0,
        }
    }

    /// A frame-meta arrived. Opens a window on the first one (resetting per-window
    /// state, including the player taps so the decode min/mean is fresh). Returns
    /// true iff this is the first meta of a fresh window (→ caller fires the clock
    /// burst). Accumulates the host stages straight off the stamps — guarding
    /// against an out-of-order stamp by skipping any meta whose subtractions would
    /// underflow.
    fn on_frame_meta(&mut self, meta: core_proto::FrameStageMeta) -> bool {
        let first = !self.active;
        if first {
            self.active = true;
            self.rtt_min_ns = None;
            self.rtt_max_ns = 0;
            self.clock_offset_ns = 0;
            self.compose_sum_ns = 0;
            self.encode_sum_ns = 0;
            self.host_send_sum_ns = 0;
            self.metas = 0;
            // Fresh decode/present accumulation for this window.
            if let Some(t) = &self.taps {
                t.reset();
            }
        }
        // All three stages must be monotone in the stamps; if any would underflow
        // the meta is bogus (clock blip / mis-stamp) — skip it rather than feed a
        // garbage stage in. `samples` counts only the metas we actually folded.
        let compose = meta.encode_start_ns.checked_sub(meta.capture_ns);
        let encode = meta.encode_done_ns.checked_sub(meta.encode_start_ns);
        let host_send = meta.wire_send_ns.checked_sub(meta.encode_done_ns);
        if let (Some(c), Some(e), Some(hs)) = (compose, encode, host_send) {
            self.compose_sum_ns = self.compose_sum_ns.saturating_add(c);
            self.encode_sum_ns = self.encode_sum_ns.saturating_add(e);
            self.host_send_sum_ns = self.host_send_sum_ns.saturating_add(hs);
            self.metas += 1;
        }
        first
    }

    /// One clock pong (with the client's recv instant t3). Track the min and max
    /// RTT over the burst; keep the min-RTT sample's offset as a diagnostic.
    fn on_clock_pong(&mut self, pong: core_proto::ClockPong, t3_nanos: u64) {
        let t0 = pong.t0_nanos as i64;
        let t1 = pong.t1_nanos as i64;
        let t2 = pong.t2_nanos as i64;
        let t3 = t3_nanos as i64;
        // NTP: offset = ((t1-t0)+(t2-t3))/2 ; rtt = (t3-t0)-(t2-t1).
        let offset = ((t1 - t0) + (t2 - t3)) / 2;
        let rtt = ((t3 - t0) - (t2 - t1)).max(0) as u64;
        if self.rtt_min_ns.map(|m| rtt < m).unwrap_or(true) {
            self.rtt_min_ns = Some(rtt);
            self.clock_offset_ns = offset; // diagnostic, from the best sample
        }
        self.rtt_max_ns = self.rtt_max_ns.max(rtt);
    }

    /// True once the window has accumulated the hard frame cap.
    fn window_at_cap(&self) -> bool {
        self.active && self.metas as usize >= BENCH_MAX_FRAMES
    }

    /// Close the window and build the report (None if no window / no folded
    /// metas). Resets so a later bench can run again.
    fn finish(&mut self) -> Option<core_proto::WireLatencyReport> {
        if !self.active {
            return None;
        }
        self.active = false;

        if self.metas == 0 {
            // Window opened but no usable meta landed — nothing to report.
            return None;
        }
        let n = self.metas as f64;
        let mean_ms = |sum_ns: u64| (sum_ns as f64 / n / 1.0e6) as f32;
        let compose_ms = mean_ms(self.compose_sum_ns);
        let encode_ms = mean_ms(self.encode_sum_ns);
        let host_send_ms = mean_ms(self.host_send_sum_ns);

        // Wire stage is RTT-derived: half the min RTT, with the burst spread as the
        // uncertainty. No frame join, no clock-offset arithmetic.
        let rtt_min_ns = self.rtt_min_ns.unwrap_or(0);
        let rtt_ms = (rtt_min_ns as f64 / 1.0e6) as f32;
        let nexus_ms = rtt_ms / 2.0;
        let nexus_uncertainty_ms =
            (self.rtt_max_ns.saturating_sub(rtt_min_ns) as f64 / 1.0e6 / 2.0) as f32;
        let clock_offset_ms = (self.clock_offset_ns as f64 / 1.0e6) as f32;

        // Client residency from the player taps. The submit→emerge time is the
        // in-flight buffer: ffmpeg's output is back-pressured by the 60 fps render,
        // so every AU waits the full buffer depth and the GPU decode kernel (~few
        // ms) is masked underneath — it cannot be isolated through an external
        // pipe-fed decoder. So `buffer_ms` = the mean residency (buffer + decode,
        // buffer-dominated) and `decode_ms` stays 0.0 (folded in, not separable).
        // `present_ms` = emerge→present mean. Absent taps → all 0.0.
        let (buffer_ms, decode_ms, present_ms) = match &self.taps {
            Some(t) => (t.decode_mean_ms(), 0.0, t.present_mean_ms()),
            None => (0.0, 0.0, 0.0),
        };

        let total_ms =
            compose_ms + encode_ms + host_send_ms + nexus_ms + buffer_ms + decode_ms + present_ms;

        let samples = self.metas;
        // Reset for a possible future window.
        self.metas = 0;
        self.compose_sum_ns = 0;
        self.encode_sum_ns = 0;
        self.host_send_sum_ns = 0;

        Some(core_proto::WireLatencyReport {
            codec_fourcc: *b"hevc",
            samples,
            rtt_ms,
            clock_offset_ms,
            compose_ms,
            encode_ms,
            host_send_ms,
            nexus_ms,
            nexus_uncertainty_ms,
            buffer_ms,
            decode_ms,
            present_ms,
            total_ms,
        })
    }
}

/// Fire a short burst of clock-sync probes (client = initiator). Each `t0_nanos`
/// is the send instant (UNIX nanos); the host echoes it in the pong.
async fn send_clock_burst(conn: &quinn::Connection) {
    for seq in 0..CLOCK_BURST {
        let ping = core_proto::ClockPing {
            seq,
            t0_nanos: unix_nanos(),
        };
        match core_proto::latency::encode_clock_ping(&ping) {
            Ok(b) => {
                if let Err(e) = conn.send_datagram(tagged_datagram(DG_CLOCK_PING, &b)) {
                    tracing::debug!("clock ping datagram dropped: {e}");
                }
            }
            Err(e) => tracing::debug!("encode clock ping: {e}"),
        }
    }
}

/// Send the final per-stage report back to the host (DG_LATENCY_REPORT).
async fn send_latency_report(conn: &quinn::Connection, report: &core_proto::WireLatencyReport) {
    match core_proto::latency::encode_wire_latency(report) {
        Ok(b) => {
            if let Err(e) = conn.send_datagram(tagged_datagram(DG_LATENCY_REPORT, &b)) {
                tracing::debug!("latency report datagram dropped: {e}");
            }
        }
        Err(e) => tracing::debug!("encode latency report: {e}"),
    }
}

/// Client-side mutual auth. Open a bidi stream and:
///   1. send our v2 prefix (magic + version) + nonce + pubkey + wants_gyro;
///   2. receive the host's signature + pubkey + challenge nonce, and verify
///      the host against the TLS exporter + the pinned key (when provided);
///   3. sign the host's challenge with our identity (zero sig if anonymous) so
///      an enforcing host can authenticate and then authorize us.
///
/// `wants_gyro` tells the host whether to relay raw IMU gyro samples. False
/// is the safe default — the host saves the IMU-reader thread's wakeups and
/// the per-frame datagram, and a v1 host (legacy) wouldn't have parsed the
/// flag anyway. The wire prefix (magic + version) lets a v2-aware host opt
/// in to reading the trailing byte without breaking v1 hosts; the trireme
/// client never talks to a v1 host in current builds (host and client ship
/// from the same tree), but the v2 wire is forward-compatible by design.
async fn do_video_auth(
    conn: &quinn::Connection,
    expected_pubkey: Option<&[u8; 32]>,
    client_identity: Option<&core_crypto::Identity>,
    wants_gyro: bool,
    wants_hdr: bool,
    requested_mode: core_proto::StreamMode,
) -> Result<()> {
    use rand::RngCore;
    let (mut send, mut recv) = conn.open_bi().await?;
    // v2 prefix — see AUTH_PROTO_MAGIC. A v2 host detects this and reads the
    // version u32 + trailing wants_gyro byte; a v1 host (none in-tree) would
    // misread the magic as the first 4 bytes of the nonce and fail the sig
    // check, which is the correct safety behaviour for a downgrade attempt.
    send.write_all(&AUTH_PROTO_MAGIC).await?;
    send.write_all(&AUTH_PROTO_VERSION.to_be_bytes()).await?;
    let mut nonce = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut nonce);
    send.write_all(&nonce).await?;
    // Our long-term pubkey, or zeros to connect anonymously. We keep the write
    // side open — we still owe the host our signature in step 3.
    let client_pub = client_identity
        .map(|id| id.public_bytes())
        .unwrap_or([0u8; 32]);
    send.write_all(&client_pub).await?;
    // wants_gyro flag — read by the host AFTER the pubkey so the v1 wire
    // shape (nonce → pubkey) is preserved verbatim at the front.
    let g: u8 = if wants_gyro { 1 } else { 0 };
    send.write_all(&[g]).await?;
    // v3: the 12-byte requested StreamMode (w, h, refresh_mhz) follows the gyro
    // byte. A v3 host reads it; the host accepts v2 too, so this is additive.
    send.write_all(&requested_mode.to_bytes()).await?;
    // v4: a trailing 1-byte wants_hdr flag after the StreamMode block. A v4 host
    // reads it; a v3 host rejects a v4 client at the version-range check (both
    // ship from one tree, so that skew is only a dev-build corner).
    let hdr: u8 = if wants_hdr { 1 } else { 0 };
    send.write_all(&[hdr]).await?;

    let mut sig = [0u8; 64];
    recv.read_exact(&mut sig).await?;
    let mut host_pub = [0u8; 32];
    recv.read_exact(&mut host_pub).await?;
    let mut host_nonce = [0u8; 32];
    recv.read_exact(&mut host_nonce).await?;

    let mut exporter = [0u8; 32];
    conn.export_keying_material(&mut exporter, VIDEO_AUTH_EXPORTER_LABEL, &[])
        .map_err(|e| anyhow::anyhow!("export_keying_material: {e:?}"))?;

    core_crypto::verify_video_auth(&nonce, &exporter, &sig, &host_pub, expected_pubkey)
        .map_err(|e| anyhow::anyhow!("video auth: {e}"))?;

    // Prove our own identity over the host's challenge. An anonymous client
    // sends a zero signature, which the host admits only if its authorizer
    // allows anonymous clients.
    let client_sig = match client_identity {
        Some(id) => core_crypto::sign_client_auth(id, &host_nonce, &exporter),
        None => [0u8; 64],
    };
    send.write_all(&client_sig).await?;
    send.finish().ok();
    Ok(())
}

/// A standalone bidirectional QUIC channel for the USB/IP relay.
///
/// Deliberately independent of the video stream: its own connection on its own
/// port, persistent, and async — the USB devices stay attached whether or not a
/// video session is up. The exporter (Trireme) dials in and opens the stream; the
/// importer (Pharos) accepts it. URBs flow both directions over `send`/`recv`.
pub struct UsbLink {
    pub send: quinn::SendStream,
    pub recv: quinn::RecvStream,
    conn: quinn::Connection,
    // Kept alive so the connection isn't torn down (client side owns its endpoint).
    _endpoint: Option<Endpoint>,
    /// Exporter side only: the importer's cert carried [`USB_V2_SAN`].
    importer_v2: bool,
}

impl UsbLink {
    /// The peer's address (the exporter as seen by Pharos, or vice versa).
    pub fn remote(&self) -> SocketAddr {
        self.conn.remote_address()
    }

    /// Exporter side: whether the importer advertised the v2 hello (so the
    /// descriptor block may be sent). Always `false` on the importer side —
    /// there the version is read off the wire, see [`read_usb_hello`].
    pub fn importer_v2(&self) -> bool {
        self.importer_v2
    }
}

/// Importer side: read the exporter's hello, v1 or v2, off the relay stream.
/// The first u16 disambiguates — [`core_usbip::hello::V2_SENTINEL`] (an
/// impossible name length, only ever sent to an importer that advertised v2
/// via [`USB_V2_SAN`]) introduces v2; anything else is a v1 name length.
pub async fn read_usb_hello(recv: &mut quinn::RecvStream) -> Result<core_usbip::hello::Hello> {
    use core_usbip::hello::{DeviceDesc, Hello, MAX_NAME_LEN, V2_SENTINEL};

    let mut u16b = [0u8; 2];
    recv.read_exact(&mut u16b).await?;
    let first = u16::from_be_bytes(u16b);
    let (v2, nlen) = if first == V2_SENTINEL {
        recv.read_exact(&mut u16b).await?;
        (true, u16::from_be_bytes(u16b) as usize)
    } else {
        (false, first as usize)
    };
    if nlen > MAX_NAME_LEN {
        anyhow::bail!("hello name length {nlen} exceeds {MAX_NAME_LEN} — garbage stream?");
    }
    let mut nb = vec![0u8; nlen];
    recv.read_exact(&mut nb).await?;
    let name = String::from_utf8_lossy(&nb).into_owned();
    let mut hs = [0u8; 8];
    recv.read_exact(&mut hs).await?;
    let devid = u32::from_be_bytes([hs[0], hs[1], hs[2], hs[3]]);
    let speed = u32::from_be_bytes([hs[4], hs[5], hs[6], hs[7]]);
    let desc = if v2 {
        let mut fixed = [0u8; DeviceDesc::FIXED_LEN];
        recv.read_exact(&mut fixed).await?;
        let (mut desc, n_ifaces) = DeviceDesc::parse_fixed(&fixed);
        let mut ib = vec![0u8; n_ifaces * 3];
        recv.read_exact(&mut ib).await?;
        desc.parse_interfaces(&ib);
        Some(desc)
    } else {
        None
    };
    Ok(Hello {
        name,
        devid,
        speed,
        desc,
    })
}

/// Pharos (import side): bind the persistent USB-relay listener. The endpoint
/// outlives individual device sessions — accept them with [`usb_import_accept`].
pub fn usb_import_endpoint(addr: SocketAddr) -> Result<Endpoint> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Ok(Endpoint::server(usb_server_config()?, addr)?)
}

/// Accept one exporter connection and its relay stream on the listener. Low
/// latency: the stream is used raw (no extra framing) so a URB hits the wire as
/// soon as it's written.
pub async fn usb_import_accept(endpoint: &Endpoint) -> Result<UsbLink> {
    let incoming = endpoint
        .accept()
        .await
        .ok_or_else(|| anyhow::anyhow!("USB relay endpoint closed"))?;
    let conn = incoming.await?;
    let (send, recv) = conn.accept_bi().await?;
    Ok(UsbLink {
        send,
        recv,
        conn,
        _endpoint: None,
        importer_v2: false,
    })
}

/// Trireme (export side): open a persistent USB-relay connection to Pharos and the
/// bidirectional stream the URBs ride on. Not contingent on any video session.
pub async fn usb_export_connect(server_addr: SocketAddr) -> Result<UsbLink> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut endpoint = Endpoint::client((Ipv4Addr::UNSPECIFIED, 0).into())?;
    // The verifier flags the importer's v2 capability during the handshake.
    let saw_v2 = Arc::new(AtomicBool::new(false));
    endpoint.set_default_client_config(usb_client_config(saw_v2.clone())?);
    let conn = endpoint.connect(server_addr, "localhost")?.await?;
    // The exporter opens the stream and speaks first (device-info handshake),
    // which is what makes the importer's accept_bi() resolve.
    let (send, recv) = conn.open_bi().await?;
    Ok(UsbLink {
        send,
        recv,
        conn,
        _endpoint: Some(endpoint),
        importer_v2: saw_v2.load(Ordering::Acquire),
    })
}

// --- Pairing channel --------------------------------------------------------
//
// A separate QUIC port (default 4432) carries the pairing handshake. Each
// pairing attempt is one QUIC connection + one bidi stream + a sequence of
// length-prefixed postcard messages defined in [`pair_proto`]. The actual
// crypto (SPAKE2 → HKDF → confirm MACs) lives in `core-crypto`; this module
// is purely the wire shape.
//
// We give pairing its own listener instead of multiplexing onto the video
// port so:
//   - pairing keeps working when no video session is up,
//   - the video acceptor doesn't have to demux,
//   - we can later swap pairing's cert verifier independently (still
//     accept-any today; a long-term pinned identity once the handshake
//     itself is the trust anchor).

/// Wire messages exchanged during pairing. Postcard-framed (length-prefix).
pub mod pair_proto {
    use serde::{Deserialize, Serialize};

    /// One step of the handshake. The order is fixed:
    ///   client → host: HelloPair
    ///   host   → client: HelloAck
    ///   client → host: Spake2Reply
    ///   client → host: ClientConfirm
    ///   host   → client: HostConfirm
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub enum PairMsg {
        /// Client kicks off; advertises its long-term pubkey + protocol version.
        HelloPair {
            version: u32,
            client_pubkey: [u8; 32],
            /// Optional alias the client suggests for itself; the host stores
            /// it alongside the pubkey in its peers list.
            alias: Option<String>,
        },
        /// Host replies with its identity + its outbound SPAKE2 message.
        HelloAck {
            host_pubkey: [u8; 32],
            spake2_msg_b: Vec<u8>,
            /// The host's friendly name for itself (e.g. hostname). The
            /// client may use it to update the tile.
            host_alias: Option<String>,
        },
        /// Client returns its SPAKE2 outbound message.
        Spake2Reply { spake2_msg_a: Vec<u8> },
        /// Client confirms it computed the same shared secret.
        ClientConfirm { mac: [u8; 32] },
        /// Host confirms in turn. Receipt = paired.
        HostConfirm { mac: [u8; 32] },
        /// Either side may bail; the other should close.
        Abort { reason: String },
    }
}

use pair_proto::PairMsg;

/// One pairing-side bidi stream. Mirrors [`UsbLink`] in shape so the
/// listener-accept-relay structure is familiar.
pub struct PairLink {
    pub send: quinn::SendStream,
    pub recv: quinn::RecvStream,
    conn: quinn::Connection,
    _endpoint: Option<Endpoint>,
}

impl PairLink {
    pub fn remote(&self) -> SocketAddr {
        self.conn.remote_address()
    }

    /// Postcard-encode + length-prefix + send a single pairing message.
    pub async fn send_msg(&mut self, msg: &PairMsg) -> Result<()> {
        let bytes = postcard::to_allocvec(msg)?;
        let len = u32::try_from(bytes.len())?.to_be_bytes();
        self.send.write_all(&len).await?;
        self.send.write_all(&bytes).await?;
        Ok(())
    }

    /// Read one length-prefixed postcard pairing message. Caps frame size
    /// to 64 KiB — handshake messages are small.
    pub async fn recv_msg(&mut self) -> Result<PairMsg> {
        let mut len_buf = [0u8; 4];
        self.recv.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;
        anyhow::ensure!(
            len <= 64 * 1024,
            "pairing frame {len} bytes exceeds 64KiB cap"
        );
        let mut buf = vec![0u8; len];
        self.recv.read_exact(&mut buf).await?;
        let msg: PairMsg = postcard::from_bytes(&buf)?;
        Ok(msg)
    }

    /// Tidy close — graceful finish + give the peer a moment to drain.
    pub async fn close(mut self) -> Result<()> {
        self.send.finish().ok();
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            self.recv.read_to_end(0),
        )
        .await;
        self.conn.close(0u32.into(), b"pair done");
        Ok(())
    }
}

/// Pharos (host): bind the persistent pairing listener.
pub fn pair_listen(addr: SocketAddr) -> Result<Endpoint> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Ok(Endpoint::server(server_config()?, addr)?)
}

/// Accept one incoming pairing attempt on a previously-bound listener.
pub async fn pair_accept(endpoint: &Endpoint) -> Result<PairLink> {
    let incoming = endpoint
        .accept()
        .await
        .ok_or_else(|| anyhow::anyhow!("pair endpoint closed"))?;
    let conn = incoming.await?;
    let (send, recv) = conn.accept_bi().await?;
    Ok(PairLink {
        send,
        recv,
        conn,
        _endpoint: None,
    })
}

/// Trireme (client): dial a Pharos pairing port and open the bidi stream.
pub async fn pair_connect(server_addrs: &[SocketAddr]) -> Result<PairLink> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut endpoint = Endpoint::client((Ipv4Addr::UNSPECIFIED, 0).into())?;
    endpoint.set_default_client_config(client_config()?);
    let conn = connect_first(&endpoint, server_addrs).await?;
    let (send, recv) = conn.open_bi().await?;
    Ok(PairLink {
        send,
        recv,
        conn,
        _endpoint: Some(endpoint),
    })
}

/// Aborts a spawned task when the guard drops, so an inner task tied to one
/// connection never leaks across a reconnect. Used for the input send task: when
/// `run_video_client_with_input` returns (session over), the guard drops and the
/// coalescer/writer are aborted rather than lingering on a dead connection.
struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// --- Client input coalescing (issue #13) -----------------------------------
//
// The client's input send task rides a RELIABLE ordered bidi QUIC stream. If the
// client's present/decode wedges or the network stalls, a naive `while recv() {
// write_all() }` blocks in `write_all`, the unbounded `input_rx` grows for
// minutes, and old motion/clicks later replay IN ORDER into the LIVE host session
// (stale input landing under the cursor NOW; interfering with the physical
// console user). That is a correctness AND safety bug.
//
// Fix: a COALESCER/WRITER split sharing a `Staging` buffer. The coalescer owns
// `input_rx`, drains it PROMPTLY (so memory stays bounded), and folds each event
// into `Staging`; consecutive motion is merged in place. The writer pops the
// compacted front item and `write_all`s it UNINTERRUPTED (never cancelled
// mid-write; quinn's `write_all` is not cancel-safe). On a genuinely stuck link
// (too many un-drained transitions, or the staging aging past a bound) we tear
// the session down so it reconnects, rather than delivering minutes-late input.

/// Max un-written TRANSITIONS (Key/Button/Connect/etc.) we let pile up before we
/// treat the link as dead and tear the session down. Transitions are never
/// coalesced (each is a discrete state change the host must see), so an unbounded
/// backlog here is the real "stuck writer" signal. 256 is generous for any human
/// burst yet trips well before a minutes-long queue can form.
const INPUT_MAX_PENDING_TRANSITIONS: usize = 256;

/// If the oldest staged item has waited longer than this without the writer
/// making progress, the link is stuck (a wedged present blocks `write_all`
/// indefinitely). Tear down rather than let input age into a stale replay. A
/// couple seconds is far beyond any healthy write latency but far below the
/// minutes-long queue #13 describes.
const INPUT_MAX_STAGE_AGE: std::time::Duration = std::time::Duration::from_secs(2);

/// True for the discrete state-change variants that must NEVER be dropped or
/// coalesced (invariant 1). Everything else is motion, which is safe to coalesce.
fn input_is_transition(ev: &core_proto::InputEvent) -> bool {
    use core_proto::InputEvent::*;
    matches!(
        ev,
        Key { .. }
            | PointerButton { .. }
            | GamepadButton { .. }
            | GamepadConnect { .. }
            | GamepadDisconnect { .. }
            | ControllerTuning { .. }
    )
}

/// The ordered, already-compacted queue of pending input the writer drains. Only
/// CONSECUTIVE same-kind motion (no transition in between) is merged, which
/// preserves position-before-click ordering (invariant 3) automatically: a
/// transition seals the trailing motion run, so a later motion sample can never
/// hop ahead of an earlier click.
struct Staging {
    /// Front = oldest = next to write. Motion within the trailing run (the tail
    /// after the last transition) is folded in place; transitions are appended
    /// verbatim and never merged.
    items: std::collections::VecDeque<core_proto::InputEvent>,
    /// Count of TRANSITIONS currently queued (the teardown trigger; motion is
    /// bounded by coalescing so it can't be the thing that grows).
    pending_transitions: usize,
    /// When the current front item was first staged, for the age bound. `None`
    /// when the queue is empty. Set when we push onto an empty queue; cleared
    /// when the queue drains empty.
    oldest_at: Option<std::time::Instant>,
    /// Set when the coalescer decides the link is dead (transition cap or age
    /// bound). The writer observes it and returns an error so the session tears
    /// down and reconnects.
    shutdown: bool,
    /// Set when the coalescer's `input_rx` closes (client teardown). The writer
    /// drains whatever is still staged, then `finish()`es the stream with a real
    /// FIN instead of letting the dropped `SendStream` emit RESET_STREAM and
    /// silently drop the remainder (#13 clean-close nicety).
    closing: bool,
}

impl Staging {
    fn new() -> Self {
        Staging {
            items: std::collections::VecDeque::new(),
            pending_transitions: 0,
            oldest_at: None,
            shutdown: false,
            closing: false,
        }
    }

    /// Fold one event into the queue. Pure (no I/O) so it is unit-testable.
    ///
    /// - Transitions: appended verbatim (never merged, never dropped).
    /// - `PointerAbs`: latest wins; replaces any `PointerAbs` in the trailing run.
    /// - `GamepadAxis{slot,axis}`: latest wins per distinct channel in the run.
    /// - `GamepadMotion{slot}`: gyro rate fields (gx/gy/gz) are SUMMED per slot in
    ///   the run; the accelerometer fields (ax/ay/az) are latest-wins. gx/gy/gz are
    ///   angular-RATE samples the host integrates into relative cursor motion with a
    ///   per-sample delta LINEAR in the rate and no dt term (`GyroMouse::delta` =
    ///   `-gz*sensitivity` per sample), so summing N coalesced rates yields the exact
    ///   same integrated pan as N separate samples — no aim rotation is lost. ax/ay/az
    ///   are an absolute orientation/gravity reading (not a rate), so summing them
    ///   would be wrong; latest-wins is correct (the host uses them only for tilt).
    /// - `PointerRel` / `Scroll`: accumulative; deltas are SUMMED into the run's
    ///   existing sample so no cursor distance / scroll amount is lost.
    fn fold(&mut self, ev: core_proto::InputEvent) {
        use core_proto::InputEvent::*;
        if self.oldest_at.is_none() {
            self.oldest_at = Some(std::time::Instant::now());
        }
        if input_is_transition(&ev) {
            self.pending_transitions += 1;
            self.items.push_back(ev);
            return;
        }
        // Motion: try to merge into the TRAILING run (walk back from the tail,
        // stopping at the first transition — never cross it, that would reorder).
        for slot in self.items.iter_mut().rev() {
            if input_is_transition(slot) {
                break; // sealed run; the new sample starts a fresh trailing item
            }
            match (&mut *slot, &ev) {
                (PointerAbs { x, y }, PointerAbs { x: nx, y: ny }) => {
                    *x = *nx;
                    *y = *ny;
                    return;
                }
                (PointerRel { dx, dy }, PointerRel { dx: ndx, dy: ndy }) => {
                    *dx += *ndx;
                    *dy += *ndy;
                    return;
                }
                (Scroll { dx, dy }, Scroll { dx: ndx, dy: ndy }) => {
                    *dx += *ndx;
                    *dy += *ndy;
                    return;
                }
                (
                    GamepadAxis {
                        slot: s,
                        axis: a,
                        value,
                    },
                    GamepadAxis {
                        slot: ns,
                        axis: na,
                        value: nv,
                    },
                ) if *s == *ns && *a == *na => {
                    *value = *nv;
                    return;
                }
                (
                    GamepadMotion {
                        slot: s,
                        gx,
                        gy,
                        gz,
                        ax,
                        ay,
                        az,
                    },
                    GamepadMotion {
                        slot: ns,
                        gx: ngx,
                        gy: ngy,
                        gz: ngz,
                        ax: nax,
                        ay: nay,
                        az: naz,
                    },
                ) if *s == *ns => {
                    // Gyro rates SUM: the host integrates them into cursor pan with a
                    // per-sample delta linear in the rate and no dt, so the sum of the
                    // coalesced rates integrates to the identical rotation.
                    *gx += *ngx;
                    *gy += *ngy;
                    *gz += *ngz;
                    // Accel is an absolute orientation reading, not a rate: latest wins.
                    *ax = *nax;
                    *ay = *nay;
                    *az = *naz;
                    return;
                }
                _ => {}
            }
        }
        // No mergeable same-kind sample in the trailing run: append fresh.
        self.items.push_back(ev);
    }

    /// Pop the next compacted item to write. Refreshes the age clock for the new
    /// front (the writer is making progress) and clears it when the queue empties.
    fn pop_front(&mut self) -> Option<core_proto::InputEvent> {
        let ev = self.items.pop_front()?;
        if input_is_transition(&ev) {
            self.pending_transitions = self.pending_transitions.saturating_sub(1);
        }
        self.oldest_at = if self.items.is_empty() {
            None
        } else {
            Some(std::time::Instant::now())
        };
        Some(ev)
    }

    /// Trip the teardown flag if the writer is genuinely stuck: too many pending
    /// transitions, or the front item has aged past the bound. Returns true when
    /// it (newly or already) wants shutdown. Called by the coalescer after each
    /// fold so a stall is caught even while `write_all` is blocked.
    fn check_stuck(&mut self) -> bool {
        if self.pending_transitions > INPUT_MAX_PENDING_TRANSITIONS {
            self.shutdown = true;
        } else if let Some(t) = self.oldest_at {
            if t.elapsed() > INPUT_MAX_STAGE_AGE {
                self.shutdown = true;
            }
        }
        self.shutdown
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    /// The adaptive receive-window sizing: ~250ms at the measured rate, clamped
    /// to [floor, cap]. The floor keeps an idle→busy burst unthrottled until the
    /// next 1s resize; the cap bounds worst-case buffered latency at any rate.
    #[test]
    fn adaptive_window_target_tracks_rate_within_clamps() {
        use super::{adaptive_window_target, WINDOW_CAP_BYTES, WINDOW_FLOOR_BYTES};
        // Idle / tiny rates clamp to the floor.
        assert_eq!(adaptive_window_target(0.0, 0), WINDOW_FLOOR_BYTES);
        assert_eq!(adaptive_window_target(100_000.0, 0), WINDOW_FLOOR_BYTES);
        // A braked 2 Mbps stream (250 KB/s) sits just at/above the floor — the
        // live regression: a 512KB floor here was 2s of standing buffer (~5s
        // pointer lag measured); the floor must track the ~250ms budget.
        assert_eq!(adaptive_window_target(250_000.0, 0), WINDOW_FLOOR_BYTES);
        assert!(u64::from(WINDOW_FLOOR_BYTES) * 4 <= 1_000_000); // ≤ ~1s at 2 Mbps
                                                                 // …but a keyframe must always fit: with 40KB IDRs in flight the window
                                                                 // is 3× the IDR, not the bare rate budget (the floor-rate IDR limit
                                                                 // cycle: window ≈ one IDR → every keyframe stalls the sender → shed →
                                                                 // another forced IDR → parked at the floor forever).
        assert_eq!(adaptive_window_target(250_000.0, 40_000), 120_000);
        // 10 Mbps couch stream = 1.25 MB/s → ~312KB of 250ms budget.
        assert_eq!(adaptive_window_target(1_250_000.0, 0), 1_250_000 / 4);
        // 60 Mbps (the hosts' encode ceiling) = 7.5 MB/s → ~1.9MB of 250ms
        // budget, which dominates 3× even a VBV-capped 500KB IDR.
        let at_ceiling = adaptive_window_target(7_500_000.0, 500_000);
        assert_eq!(at_ceiling, 7_500_000 / 4);
        assert!(at_ceiling > WINDOW_FLOOR_BYTES && at_ceiling < WINDOW_CAP_BYTES);
        // Absurd rates clamp to the memory cap.
        assert_eq!(adaptive_window_target(1e9, 0), WINDOW_CAP_BYTES);
    }

    /// Issue #13: the pure input-coalescing logic. Under backpressure motion is
    /// coalesced (absolute=latest, accumulative=summed) but transitions are never
    /// dropped or reordered, and the latest position always lands BEFORE a
    /// following click. Also proves the transition cap trips a teardown rather
    /// than growing unbounded.
    #[test]
    fn input_coalescing_drops_motion_keeps_transitions() {
        use core_proto::InputEvent::*;
        use core_proto::Modifiers;

        let mods = Modifiers::default();
        let mut s = super::Staging::new();

        // A burst of absolute motion collapses to ONE item (latest wins).
        for i in 0..100u32 {
            s.fold(PointerAbs {
                x: i as f32 / 100.0,
                y: i as f32 / 100.0,
            });
        }
        assert_eq!(s.items.len(), 1, "PointerAbs burst must coalesce to one");
        assert_eq!(
            s.items[0],
            PointerAbs { x: 0.99, y: 0.99 },
            "only the latest absolute sample survives"
        );

        // A click after the aimed position: the position must be staged BEFORE
        // the button (position-before-click), and the button must survive.
        s.fold(PointerButton {
            button: 0,
            pressed: true,
        });
        assert_eq!(s.items.len(), 2);
        assert_eq!(s.items[0], PointerAbs { x: 0.99, y: 0.99 });
        assert_eq!(
            s.items[1],
            PointerButton {
                button: 0,
                pressed: true
            }
        );

        // Motion AFTER the transition starts a fresh trailing run — it must NOT
        // merge back into the pre-click PointerAbs (that would reorder past the
        // click). So we now have abs, button, abs.
        s.fold(PointerAbs { x: 0.5, y: 0.5 });
        assert_eq!(s.items.len(), 3);
        assert_eq!(s.items[2], PointerAbs { x: 0.5, y: 0.5 });

        // Accumulative motion SUMS (no cursor distance / scroll lost).
        s.fold(PointerRel { dx: 1.0, dy: 2.0 });
        s.fold(PointerRel { dx: 3.0, dy: -1.0 });
        s.fold(Scroll { dx: 0.0, dy: 1.0 });
        s.fold(Scroll { dx: 0.0, dy: 2.0 });
        // items now: abs, button, abs, rel(4,1), scroll(0,3)
        assert_eq!(s.items.len(), 5);
        assert_eq!(s.items[3], PointerRel { dx: 4.0, dy: 1.0 });
        assert_eq!(s.items[4], Scroll { dx: 0.0, dy: 3.0 });

        // Distinct gamepad axes stay separate; same axis coalesces to latest.
        s.fold(GamepadAxis {
            slot: 0,
            axis: 0,
            value: 0.1,
        });
        s.fold(GamepadAxis {
            slot: 0,
            axis: 1,
            value: 0.2,
        });
        s.fold(GamepadAxis {
            slot: 0,
            axis: 0,
            value: 0.9,
        });
        // axis0 replaced to 0.9, axis1 kept: two new items.
        assert_eq!(s.items.len(), 7);
        assert_eq!(
            s.items[5],
            GamepadAxis {
                slot: 0,
                axis: 0,
                value: 0.9
            }
        );
        assert_eq!(
            s.items[6],
            GamepadAxis {
                slot: 0,
                axis: 1,
                value: 0.2
            }
        );

        // Gyro motion coalesces the RATE fields (gx/gy/gz) by SUMMING so the host's
        // dt-free per-sample integration reproduces the same total pan, while the
        // accelerometer (an absolute orientation reading) stays latest-wins.
        for _ in 0..10 {
            s.fold(GamepadMotion {
                slot: 0,
                gx: 0.0,
                gy: 0.0,
                gz: 0.1,
                ax: 0.0,
                ay: 0.0,
                az: 9.8,
            });
        }
        // Ten 0.1 rate samples collapse to ONE item with gz summed to 1.0; accel
        // is the latest reading, not a sum.
        assert_eq!(s.items.len(), 8, "gyro burst coalesces to one item");
        match s.items[7] {
            GamepadMotion {
                slot,
                gx,
                gy,
                gz,
                ax,
                ay,
                az,
            } => {
                assert_eq!(slot, 0);
                assert_eq!(gx, 0.0);
                assert_eq!(gy, 0.0);
                assert!(
                    (gz - 1.0).abs() < 1e-6,
                    "gyro rate must SUM (0.1 x10 -> 1.0), got {gz}"
                );
                assert_eq!(ax, 0.0);
                assert_eq!(ay, 0.0);
                assert!(
                    (az - 9.8).abs() < 1e-6,
                    "accel must be latest-wins, not summed, got {az}"
                );
            }
            ref other => panic!("expected GamepadMotion, got {other:?}"),
        }
        // Drop the coalesced gyro so the FIFO assertions below stay unchanged.
        s.items.pop_back();

        // Every transition folded so far (one) is still counted; draining pops
        // them in FIFO order and the position lands before the click.
        assert_eq!(s.pending_transitions, 1);
        assert_eq!(s.pop_front(), Some(PointerAbs { x: 0.99, y: 0.99 }));
        assert_eq!(
            s.pop_front(),
            Some(PointerButton {
                button: 0,
                pressed: true
            })
        );
        assert_eq!(s.pending_transitions, 0);

        // The transition CAP trips a shutdown rather than growing unbounded: a
        // wedged writer that never drains would otherwise queue forever.
        let mut s2 = super::Staging::new();
        for i in 0..(super::INPUT_MAX_PENDING_TRANSITIONS + 1) {
            s2.fold(Key {
                scancode: i as u32,
                pressed: true,
                mods,
            });
        }
        assert!(
            s2.check_stuck(),
            "exceeding the transition cap must request teardown"
        );
        assert!(s2.shutdown, "shutdown flag set for the writer to observe");
    }

    /// Issue #13 stuck detection (pure state machine). The transition CAP is a
    /// strict `>` bound (exactly at the cap is fine; one past it trips), and the
    /// AGE bound trips once the oldest staged item ages past `INPUT_MAX_STAGE_AGE`.
    /// When either trips, the coalescer closes the connection with
    /// `INPUT_STUCK_CLOSE_CODE`; because that code is NOT one of the host's stop
    /// codes, `app_close_error` maps it to `None`, so the read loops surface an
    /// ordinary error and the client reconnects (see the network-chain reasoning
    /// in the commit message).
    #[test]
    fn input_stuck_trips_at_cap_and_age_bounds() {
        use core_proto::InputEvent::*;
        use core_proto::Modifiers;
        let mods = Modifiers::default();

        // Exactly AT the cap is not stuck; one past it is.
        let mut at_cap = super::Staging::new();
        for i in 0..super::INPUT_MAX_PENDING_TRANSITIONS {
            at_cap.fold(Key {
                scancode: i as u32,
                pressed: true,
                mods,
            });
        }
        assert!(!at_cap.check_stuck(), "exactly at the cap must NOT trip");
        at_cap.fold(Key {
            scancode: super::INPUT_MAX_PENDING_TRANSITIONS as u32,
            pressed: true,
            mods,
        });
        assert!(at_cap.check_stuck(), "one past the cap must trip");

        // The AGE bound: force the oldest-staged clock past the bound and confirm a
        // single staged (non-transition) item trips shutdown even under the cap.
        let mut aged = super::Staging::new();
        aged.fold(PointerRel { dx: 1.0, dy: 0.0 });
        assert!(!aged.check_stuck(), "fresh staged item is not stuck");
        aged.oldest_at = Some(
            std::time::Instant::now()
                - super::INPUT_MAX_STAGE_AGE
                - std::time::Duration::from_millis(1),
        );
        assert!(aged.check_stuck(), "an item aged past the bound must trip");
        assert!(aged.shutdown, "shutdown flag set for the writer to observe");

        // The stuck close code must NOT collide with the host's special stop codes,
        // so the client treats it as an ordinary drop → reconnect (not a refusal).
        assert_ne!(super::INPUT_STUCK_CLOSE_CODE, super::RESERVED_CLOSE_CODE);
        assert_ne!(super::INPUT_STUCK_CLOSE_CODE, super::UNPAIRED_CLOSE_CODE);
        assert_ne!(super::INPUT_STUCK_CLOSE_CODE, super::PREEMPT_CLOSE_CODE);
    }

    /// The client-side `DG_STREAM_HDR` colorimetry state (issue #16): unknown until
    /// the first announce, then SDR/HDR, representable as three distinct states so
    /// the decoder can default to plain decode while unknown. Also proves `reset()`
    /// does NOT clear it — colorimetry is session state, matching the geometry's
    /// lifetime exactly.
    #[test]
    fn stream_hdr_unknown_sdr_hdr_transitions() {
        let taps = super::LatencyTaps::new();
        // Fresh taps: not yet announced → None (the crash-safe "plain decode" state).
        assert_eq!(taps.stream_hdr(), None);
        // First announce says SDR.
        taps.set_stream_hdr(false);
        assert_eq!(taps.stream_hdr(), Some(false));
        // Flip to HDR.
        taps.set_stream_hdr(true);
        assert_eq!(taps.stream_hdr(), Some(true));
        // Flip back to SDR (a respawn-boundary HDR→SDR downgrade).
        taps.set_stream_hdr(false);
        assert_eq!(taps.stream_hdr(), Some(false));
        // reset() clears the bench accumulators but MUST leave colorimetry latched
        // (same as geometry) — otherwise a bench window would wrongly revert the
        // decoder to "unknown" mid-session.
        taps.set_stream_hdr(true);
        taps.set_geometry(1920, 1080);
        taps.reset();
        assert_eq!(taps.stream_hdr(), Some(true));
        assert_eq!(taps.geometry(), Some((1920, 1080)));
    }

    /// The client-side `DG_CURSOR_MODE` cursor-capture state: unknown until the first
    /// announce, then FREE/CAPTURED, three distinct states so an older host (never
    /// announces) leaves the client at "unknown" and thus on manual grab only. Like
    /// the colorimetry, `reset()` MUST leave it latched (session state, not a bench
    /// stat) so a bench window can't spuriously revert the cursor to "unknown".
    #[test]
    fn cursor_captured_unknown_free_captured_transitions() {
        let taps = super::LatencyTaps::new();
        // Fresh taps: not yet announced → None (older host / not-yet-learned; the
        // client leaves its cursor grab to the manual hotkey only).
        assert_eq!(taps.cursor_captured(), None);
        // First announce says FREE (a menu / desktop; absolute pointer).
        taps.set_cursor_captured(false);
        assert_eq!(taps.cursor_captured(), Some(false));
        // Flip to CAPTURED (the game grabbed the mouse; relative mouselook).
        taps.set_cursor_captured(true);
        assert_eq!(taps.cursor_captured(), Some(true));
        // Flip back to FREE (returned to a menu).
        taps.set_cursor_captured(false);
        assert_eq!(taps.cursor_captured(), Some(false));
        // reset() must leave the capture state latched (same as HDR/geometry).
        taps.set_cursor_captured(true);
        taps.reset();
        assert_eq!(taps.cursor_captured(), Some(true));
    }

    /// IDNA + host:port splitting. The IDN case is the whole point — a Unicode
    /// domain must ToASCII to punycode before it ever reaches the resolver.
    #[test]
    fn resolve_addr_idn_and_literals() {
        // IP literals pass straight through (no DNS).
        assert_eq!(
            super::resolve_addr("192.168.1.100:4433")
                .unwrap()
                .to_string(),
            "192.168.1.100:4433",
        );
        // Bracketed IPv6 literal splits on `]:`.
        assert!(super::resolve_addr("[::1]:4433").is_ok());
        // Malformed input (no port) is rejected, not panicked.
        assert!(super::resolve_addr("nope").is_err());
        // The IDN path: the Unicode host must IDNA-encode to punycode. We can't
        // assert DNS success offline, but the punycode form of the user's domain
        // must equal the expected `xn--` label (proves ToASCII ran, not raw UTF-8).
        assert_eq!(idna::domain_to_ascii("bücher.example").unwrap(), "xn--bcher-kva.example");
    }

    #[test]
    fn hevc_bytes_roundtrip_over_quic() {
        // Stand-in for an encoded HEVC chunk.
        let data: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
        let received = super::quic_loopback(data.clone()).expect("loopback");
        assert_eq!(received, data);
    }

    /// The USB relay channel must carry bytes both ways on one bidi stream
    /// (exporter→host = RET_SUBMIT, host→exporter = CMD_SUBMIT).
    #[test]
    fn usb_bidi_roundtrip() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let endpoint = super::usb_import_endpoint((Ipv4Addr::LOCALHOST, 0).into()).unwrap();
            let addr = endpoint.local_addr().unwrap();

            // Pharos side: read the exporter's hello, reply (the URB direction).
            let server = tokio::spawn(async move {
                let mut link = super::usb_import_accept(&endpoint).await.unwrap();
                let mut hello = [0u8; 4];
                link.recv.read_exact(&mut hello).await.unwrap();
                link.send.write_all(b"CMD0").await.unwrap();
                link.send.finish().unwrap();
                link.recv.read_to_end(0).await.ok(); // drain
                hello
            });

            // Exporter side.
            let mut link = super::usb_export_connect(addr).await.unwrap();
            link.send.write_all(b"DEV0").await.unwrap();
            let mut reply = [0u8; 4];
            link.recv.read_exact(&mut reply).await.unwrap();
            assert_eq!(&reply, b"CMD0");
            link.send.finish().unwrap();

            assert_eq!(&server.await.unwrap(), b"DEV0");
        });
    }

    /// Hello-version negotiation on the USB relay, all four quadrants. The
    /// importer advertises v2 via the cert SAN; the exporter detects it and
    /// picks the hello version; `read_usb_hello` disambiguates on the wire.
    /// No quadrant may fail the QUIC handshake or misparse the hello.
    #[test]
    fn usb_hello_version_negotiation() {
        use core_usbip::hello::{DeviceDesc, Hello};
        let _ = rustls::crypto::ring::default_provider().install_default();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let hello = Hello {
            name: "DualSense".into(),
            devid: (1 << 16) | 5,
            speed: 3,
            desc: Some(DeviceDesc {
                busnum: 1,
                devnum: 5,
                id_vendor: 0x054c,
                id_product: 0x0ce6,
                interfaces: vec![core_usbip::op::Interface {
                    class: 3,
                    subclass: 0,
                    protocol: 0,
                }],
                ..Default::default()
            }),
        };
        rt.block_on(async {
            let endpoint = super::usb_import_endpoint((Ipv4Addr::LOCALHOST, 0).into()).unwrap();
            let addr = endpoint.local_addr().unwrap();
            let expect = hello.clone();
            let server = tokio::spawn(async move {
                // New exporter: the importer must read back a v2 hello intact.
                let mut a = super::usb_import_accept(&endpoint).await.unwrap();
                let got = super::read_usb_hello(&mut a.recv).await.unwrap();
                assert_eq!(got, expect, "v2 hello must round-trip");
                // Legacy exporter: raw v1 bytes must parse with desc == None.
                let mut b = super::usb_import_accept(&endpoint).await.unwrap();
                let got = super::read_usb_hello(&mut b.recv).await.unwrap();
                assert_eq!(got.name, "Old");
                assert_eq!((got.devid, got.speed), (7, 3));
                assert!(got.desc.is_none(), "legacy exporter must read as v1");
            });

            // New exporter sees the SAN and sends v2. (Await the write so the
            // two connections reach the accept loop in order.)
            let mut l1 = super::usb_export_connect(addr).await.unwrap();
            assert!(l1.importer_v2(), "new importer must advertise v2");
            l1.send.write_all(&hello.to_bytes_v2()).await.unwrap();

            // Legacy exporter: the old code path — plain client config sending
            // the v1 wire format directly.
            let mut legacy = quinn::Endpoint::client((Ipv4Addr::LOCALHOST, 0).into()).unwrap();
            legacy.set_default_client_config(super::client_config().unwrap());
            let conn = legacy.connect(addr, "localhost").unwrap().await.unwrap();
            let (mut send, _recv) = conn.open_bi().await.unwrap();
            let old_hello = Hello {
                name: "Old".into(),
                devid: 7,
                speed: 3,
                desc: None,
            };
            send.write_all(&old_hello.to_bytes_v1()).await.unwrap();
            server.await.unwrap();

            // Legacy importer: the old code path — plain server config, no SAN.
            // The new exporter must detect nothing and stay on v1.
            let old_srv = quinn::Endpoint::server(
                super::server_config().unwrap(),
                (Ipv4Addr::LOCALHOST, 0).into(),
            )
            .unwrap();
            let old_addr = old_srv.local_addr().unwrap();
            let old_accept = tokio::spawn(async move {
                let conn = old_srv.accept().await.unwrap().await.unwrap();
                let (_s, mut r) = conn.accept_bi().await.unwrap();
                let mut one = [0u8; 1];
                r.read_exact(&mut one).await.unwrap();
            });
            let mut l3 = super::usb_export_connect(old_addr).await.unwrap();
            assert!(!l3.importer_v2(), "legacy importer must not read as v2");
            l3.send.write_all(b"z").await.unwrap();
            old_accept.await.unwrap();
        });
    }

    /// The pairing QUIC path (`pair_listen`/`pair_accept` ↔ `pair_connect`) must
    /// complete a handshake and carry a bidi exchange on loopback — isolates the
    /// QUIC layer from the Windows host's no-response symptom.
    #[test]
    fn pair_quic_handshake_loopback() {
        use std::net::Ipv4Addr;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let endpoint = super::pair_listen((Ipv4Addr::LOCALHOST, 0).into()).unwrap();
            let addr = endpoint.local_addr().unwrap();
            // Server: accept the connection + the client's bidi stream, read the
            // client's first bytes (mirrors `handle_one` reading HelloPair first).
            let server = tokio::spawn(async move {
                let mut link = super::pair_accept(&endpoint).await.unwrap();
                let mut buf = [0u8; 4];
                link.recv.read_exact(&mut buf).await.unwrap();
                buf
            });
            // Client: connect + open the bidi stream + send. Keep the link (and
            // thus the connection) alive until the server has read.
            let mut link = super::pair_connect(&[addr]).await.unwrap();
            link.send.write_all(b"PING").await.unwrap();
            let got = server.await.unwrap();
            assert_eq!(&got, b"PING");
        });
    }

    /// A client NIC/IP change mid-stream must NOT drop the connection — QUIC
    /// migrates it to the new path (keyed by connection ID, not the 4-tuple). We
    /// simulate the change by rebinding the client endpoint to a fresh local
    /// socket while a uni stream is in flight, then assert the rest still arrives.
    #[test]
    fn connection_survives_client_rebind() {
        use std::net::{Ipv4Addr, UdpSocket};
        use std::time::Duration;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let server = super::Endpoint::server(
                super::server_config().unwrap(),
                (Ipv4Addr::LOCALHOST, 0).into(),
            )
            .unwrap();
            let addr = server.local_addr().unwrap();
            let srv = tokio::spawn(async move {
                let conn = server.accept().await.unwrap().await.unwrap();
                let mut s = conn.open_uni().await.unwrap();
                for i in 0..20u8 {
                    s.write_all(&[i]).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(40)).await;
                }
                s.finish().ok();
                // Hold the connection open so the client can drain post-migration.
                tokio::time::sleep(Duration::from_millis(300)).await;
            });

            let mut client = super::Endpoint::client((Ipv4Addr::LOCALHOST, 0).into()).unwrap();
            client.set_default_client_config(super::client_config().unwrap());
            let conn = client.connect(addr, "localhost").unwrap().await.unwrap();
            let mut r = conn.accept_uni().await.unwrap();

            // Read a little, then yank the socket out from under the connection —
            // this is what a WiFi→Ethernet switch / DHCP renew looks like to quinn.
            let mut first = [0u8; 3];
            r.read_exact(&mut first).await.unwrap();
            client
                .rebind(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap())
                .unwrap();

            // The rest must still arrive over the migrated path.
            let rest = r.read_to_end(64).await.unwrap();
            let mut all = first.to_vec();
            all.extend(rest);
            assert_eq!(
                all,
                (0..20u8).collect::<Vec<u8>>(),
                "data lost across client rebind/migration"
            );
            srv.await.unwrap();
        });
    }

    /// The whole point of M2 pairing hardening: a host with an enforcing
    /// authorizer streams to a paired client and rejects an unpaired one.
    #[test]
    fn mutual_auth_admits_paired_rejects_unpaired() {
        use std::net::{SocketAddr, UdpSocket};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::Duration;
        let _ = rustls::crypto::ring::default_provider().install_default();

        let host_id = Arc::new(core_crypto::Identity::generate());
        let client_a = core_crypto::Identity::generate(); // paired → admitted
        let client_b = core_crypto::Identity::generate(); // unpaired → rejected
        let a_pub = client_a.public_bytes();

        // Grab a free UDP port, then bind the streamer to it.
        let probe = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let addr: SocketAddr = (Ipv4Addr::LOCALHOST, port).into();

        // Authorizer admits only client A's *verified* pubkey.
        let authorizer: super::ClientAuthorizer =
            Arc::new(move |c: Option<[u8; 32]>| c == Some(a_pub));
        let streamer = Arc::new(
            super::QuicStreamer::start_with_authorizer(
                addr,
                host_id,
                authorizer,
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
                false, // dual_stack
            )
            .unwrap(),
        );

        // Keep frames flowing so an admitted client has something to receive.
        let stop = Arc::new(AtomicBool::new(false));
        let pusher = {
            let s = streamer.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    s.send(vec![1, 2, 3, 4]);
                    std::thread::sleep(Duration::from_millis(10));
                }
            })
        };

        // Unpaired client B: the host closes the connection after auth, so the
        // video client errors out and never sees a frame. Run it to completion
        // first (the host accepts one connection at a time).
        let res_b = std::thread::spawn(move || {
            super::run_video_client(addr, None, Some(&client_b), |_| {}, None, None, None)
        })
        .join()
        .unwrap();
        assert!(res_b.is_err(), "unpaired client must be rejected");

        // Paired client A: admitted, receives at least one frame.
        let got = Arc::new(AtomicBool::new(false));
        {
            let got = got.clone();
            std::thread::spawn(move || {
                let _ = super::run_video_client(
                    addr,
                    None,
                    Some(&client_a),
                    move |_| {
                        got.store(true, Ordering::Release);
                    },
                    None,
                    None,
                    None,
                );
            });
        }
        let mut admitted = false;
        for _ in 0..300 {
            if got.load(Ordering::Acquire) {
                admitted = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        let _ = pusher.join();
        assert!(
            admitted,
            "paired client must be admitted and receive a frame"
        );
    }

    /// Issue #19 primary-client policy, end to end on loopback:
    ///   1. while the PRIMARY streams, a different paired client is REFUSED
    ///      with the stable reserved-by-primary error — and the primary's
    ///      frames keep flowing (its session was never yanked);
    ///   2. the primary ITSELF redialing (a stale incumbent session) frees the
    ///      slot: the incumbent is preempted, and the redial lands.
    #[test]
    fn primary_client_reserves_the_slot() {
        use std::net::{SocketAddr, UdpSocket};
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::sync::Arc;
        use std::time::Duration;
        let _ = rustls::crypto::ring::default_provider().install_default();

        let host_id = Arc::new(core_crypto::Identity::generate());
        // Arc because the primary identity dials three times from three
        // threads (`Identity` isn't Clone).
        let primary = Arc::new(core_crypto::Identity::generate());
        let other = core_crypto::Identity::generate();
        let p_pub = primary.public_bytes();
        let o_pub = other.public_bytes();

        let probe = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let addr: SocketAddr = (Ipv4Addr::LOCALHOST, port).into();

        // Both clients are paired; only `primary` is the primary. The global
        // is keyed to this test's unique pubkey, so concurrently-running tests
        // (whose incumbents hold other keys) never trip the policy.
        let authorizer: super::ClientAuthorizer =
            Arc::new(move |c: Option<[u8; 32]>| c == Some(p_pub) || c == Some(o_pub));
        super::set_primary_peer(Some(p_pub));
        let streamer = Arc::new(
            super::QuicStreamer::start_with_authorizer(
                addr,
                host_id,
                authorizer,
                Arc::new(AtomicBool::new(false)),
                false, // dual_stack
            )
            .unwrap(),
        );

        let stop = Arc::new(AtomicBool::new(false));
        let pusher = {
            let s = streamer.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    s.send(vec![1, 2, 3, 4]);
                    std::thread::sleep(Duration::from_millis(10));
                }
            })
        };

        // 1. The primary connects and receives frames.
        let p_frames = Arc::new(AtomicU64::new(0));
        let p_result = {
            let p_frames = p_frames.clone();
            let primary = primary.clone();
            std::thread::spawn(move || {
                super::run_video_client(
                    addr,
                    None,
                    Some(primary.as_ref()),
                    move |_| {
                        p_frames.fetch_add(1, Ordering::Release);
                    },
                    None,
                    None,
                    None,
                )
            })
        };
        let mut connected = false;
        for _ in 0..500 {
            if p_frames.load(Ordering::Acquire) > 0 {
                connected = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(connected, "primary must connect and receive frames");

        // 2. A different paired client tries to take the slot → refused with
        //    the stable reserved-by-primary message, nobody preempted.
        let res_other = std::thread::spawn(move || {
            super::run_video_client(addr, None, Some(&other), |_| {}, None, None, None)
        })
        .join()
        .unwrap();
        let err = format!(
            "{:#}",
            res_other.expect_err("non-primary must be refused while primary streams")
        );
        assert!(
            err.contains(super::RESERVED_BY_PRIMARY_MSG),
            "refusal must carry the stable reserved message, got: {err}"
        );

        // The primary's stream must have survived the challenge: its frame
        // count keeps growing after the refusal.
        let before = p_frames.load(Ordering::Acquire);
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            p_frames.load(Ordering::Acquire) > before,
            "primary's stream must keep flowing after a refused challenger"
        );

        // 3. The primary redials over its own (still-live) session — the stale
        //    incumbent is preempted so the device isn't locked out of its own
        //    host. The redial itself is closed plain ("redial"), and the NEXT
        //    attempt gets the freed slot.
        let redial = std::thread::spawn({
            let primary = primary.clone();
            move || {
                super::run_video_client(
                    addr,
                    None,
                    Some(primary.as_ref()),
                    |_| {},
                    None,
                    None,
                    None,
                )
            }
        });
        let incumbent_end = p_result.join().unwrap();
        let inc_err = format!(
            "{:#}",
            incumbent_end.expect_err("stale incumbent must be preempted by the primary's redial")
        );
        assert!(
            inc_err.contains("preempted"),
            "incumbent must end as preempted, got: {inc_err}"
        );
        let _ = redial.join().unwrap(); // closed plain → ordinary end, either Ok or Err

        let p2_frames = Arc::new(AtomicU64::new(0));
        {
            let p2_frames = p2_frames.clone();
            std::thread::spawn(move || {
                let _ = super::run_video_client(
                    addr,
                    None,
                    Some(primary.as_ref()),
                    move |_| {
                        p2_frames.fetch_add(1, Ordering::Release);
                    },
                    None,
                    None,
                    None,
                );
            });
        }
        let mut reconnected = false;
        for _ in 0..500 {
            if p2_frames.load(Ordering::Acquire) > 0 {
                reconnected = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        let _ = pusher.join();
        super::set_primary_peer(None);
        assert!(
            reconnected,
            "primary must be able to reclaim the slot after its redial"
        );
    }

    /// Issue #19 forget-peer teardown: `disconnect_peer` closes the live
    /// session with the UNPAIRED close code, which the client surfaces as the
    /// stable unpaired message (its cue to stop redialing).
    #[test]
    fn disconnect_peer_closes_live_session_as_unpaired() {
        use std::net::{SocketAddr, UdpSocket};
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        use std::sync::Arc;
        use std::time::Duration;
        let _ = rustls::crypto::ring::default_provider().install_default();

        let host_id = Arc::new(core_crypto::Identity::generate());
        let client = core_crypto::Identity::generate();
        let c_pub = client.public_bytes();

        let probe = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let addr: SocketAddr = (Ipv4Addr::LOCALHOST, port).into();

        let authorizer: super::ClientAuthorizer =
            Arc::new(move |c: Option<[u8; 32]>| c == Some(c_pub));
        let streamer = Arc::new(
            super::QuicStreamer::start_with_authorizer(
                addr,
                host_id,
                authorizer,
                Arc::new(AtomicBool::new(false)),
                false, // dual_stack
            )
            .unwrap(),
        );

        let stop = Arc::new(AtomicBool::new(false));
        let pusher = {
            let s = streamer.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    s.send(vec![9, 9, 9]);
                    std::thread::sleep(Duration::from_millis(10));
                }
            })
        };

        let frames = Arc::new(AtomicU64::new(0));
        let client_res = {
            let frames = frames.clone();
            std::thread::spawn(move || {
                super::run_video_client(
                    addr,
                    None,
                    Some(&client),
                    move |_| {
                        frames.fetch_add(1, Ordering::Release);
                    },
                    None,
                    None,
                    None,
                )
            })
        };
        let mut connected = false;
        for _ in 0..500 {
            if frames.load(Ordering::Acquire) > 0 {
                connected = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(connected, "client must connect and receive frames");

        // The operator forgets the peer → its registered live session closes.
        assert_eq!(
            super::disconnect_peer(c_pub),
            1,
            "one live session must be closed"
        );
        let err = format!(
            "{:#}",
            client_res
                .join()
                .unwrap()
                .expect_err("client must see its session end as unpaired")
        );
        stop.store(true, Ordering::Relaxed);
        let _ = pusher.join();
        assert!(
            err.contains(super::UNPAIRED_MSG),
            "close must map to the stable unpaired message, got: {err}"
        );
        // An unknown key closes nothing.
        assert_eq!(super::disconnect_peer([0x42; 32]), 0);
    }

    /// Audio rides the same authenticated session as unreliable QUIC datagrams:
    /// `send_audio` on the host → `on_audio` on the client.
    #[test]
    fn audio_datagrams_reach_client() {
        use std::net::{SocketAddr, UdpSocket};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::Duration;
        let _ = rustls::crypto::ring::default_provider().install_default();

        let host_id = Arc::new(core_crypto::Identity::generate());
        let probe = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let addr: SocketAddr = (Ipv4Addr::LOCALHOST, port).into();

        let streamer = Arc::new(super::QuicStreamer::start(addr, host_id).unwrap());

        // Push audio packets continuously (only transmitted once a client is up).
        let stop = Arc::new(AtomicBool::new(false));
        let pusher = {
            let s = streamer.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    s.send_audio(vec![7, 7, 7, 7]);
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
        };

        let got_audio = Arc::new(AtomicBool::new(false));
        {
            let got_audio = got_audio.clone();
            std::thread::spawn(move || {
                let on_audio: Box<dyn FnMut(Vec<u8>) + Send> = Box::new(move |pkt: Vec<u8>| {
                    if !pkt.is_empty() {
                        got_audio.store(true, Ordering::Release);
                    }
                });
                // No video is pushed here, so the video stream just idles; the
                // audio datagrams are what we're checking.
                let _ = super::run_video_client_with_input(
                    vec![addr],
                    None,
                    None,
                    |_| {},
                    None,
                    Some(on_audio),
                    None,
                    None,
                    None,
                    None, // on_catalog
                    None, // on_app_notice
                    None, // app_launch_rx
                    false,
                    false, // wants_hdr
                    None,
                    None,
                    core_proto::StreamMode::default(),
                    None,
                    None,
                    None,
                    None, // transport_hint
                    None, // on_clipboard
                    None, // clipboard_rx
                );
            });
        }

        let mut received = false;
        for _ in 0..300 {
            if got_audio.load(Ordering::Acquire) {
                received = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        let _ = pusher.join();
        assert!(received, "client must receive audio datagrams");
    }

    /// The app-launcher relay's client→host direction, under simulated video
    /// load: the host pushes a catalog (so the app stream opens) + a steady video
    /// stream, the client sends a launch via its app channel, and the host's
    /// `take_app_launch_rx` must deliver it. Reproduces the "tap does nothing"
    /// bug seen on the TV without needing the device.
    #[test]
    fn app_launch_relays_client_to_host_under_load() {
        use std::net::{SocketAddr, UdpSocket};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::Duration;
        let _ = rustls::crypto::ring::default_provider().install_default();

        let host_id = Arc::new(core_crypto::Identity::generate());
        let probe = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let addr: SocketAddr = (Ipv4Addr::LOCALHOST, port).into();

        let streamer = Arc::new(super::QuicStreamer::start(addr, host_id).unwrap());
        let mut launch_rx = streamer.take_app_launch_rx().expect("app launch rx once");
        // Publish a catalog so the host opens the app relay stream on connect.
        streamer.send_app_catalog(vec![core_proto::AppInfo {
            id: "x".into(),
            name: "X".into(),
            kind: 0,
            art: Vec::new(),
            running: false,
        }]);

        // Simulate the live stream so the client's video loop is busy (the
        // condition under which a starved app task would never send).
        let stop = Arc::new(AtomicBool::new(false));
        let pusher = {
            let s = streamer.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    s.send(vec![0u8; 4096]);
                    std::thread::sleep(Duration::from_millis(8));
                }
            })
        };

        let (app_tx, app_rx) = tokio::sync::mpsc::unbounded_channel::<core_proto::AppMsg>();
        // Match the real client exactly: it opens the input bidi stream AND
        // registers an on_catalog callback, both alongside the app launch channel.
        let (_input_tx, input_rx) =
            tokio::sync::mpsc::unbounded_channel::<core_proto::InputEvent>();
        let on_catalog: Option<Box<dyn FnMut(Vec<core_proto::AppInfo>) + Send>> =
            Some(Box::new(|_apps| {}));
        std::thread::spawn(move || {
            let _ = super::run_video_client_with_input(
                vec![addr],
                None,
                None,
                |_| {},
                Some(input_rx),
                None,
                None,
                None,
                None,
                on_catalog,
                None, // on_app_notice: tests don't assert toasts
                Some(app_rx),
                false,
                false, // wants_hdr
                None,
                None,
                core_proto::StreamMode::default(),
                None,
                None,
                None,
                None, // transport_hint
                None, // on_clipboard
                None, // clipboard_rx
            );
        });

        // Wait for the client to connect + the app stream to establish.
        for _ in 0..300 {
            if streamer.is_connected() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(300));
        app_tx
            .send(core_proto::AppMsg::Launch {
                id: "crimson-desert".into(),
            })
            .expect("send launch");

        let mut got = None;
        for _ in 0..300 {
            if let Ok(core_proto::AppMsg::Launch { id }) = launch_rx.try_recv() {
                got = Some(id);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        let _ = pusher.join();
        assert_eq!(
            got.as_deref(),
            Some("crimson-desert"),
            "host must receive the client's launch request"
        );
    }

    /// The clipboard relay, both directions on loopback, under simulated video
    /// load. The host opens the app relay (idx0) then the clipboard relay (idx1);
    /// the client demuxes them by accept order. host→client: `send_clipboard` must
    /// surface at the client's `on_clipboard`. client→host: text pushed on the
    /// client's `clipboard_rx` must surface at the host's `take_clipboard_in_rx`.
    #[test]
    fn clipboard_relay_roundtrips_both_ways() {
        use std::net::{SocketAddr, UdpSocket};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        use std::time::Duration;
        let _ = rustls::crypto::ring::default_provider().install_default();

        let host_id = Arc::new(core_crypto::Identity::generate());
        let probe = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let addr: SocketAddr = (Ipv4Addr::LOCALHOST, port).into();

        let streamer = Arc::new(super::QuicStreamer::start(addr, host_id).unwrap());
        let mut clip_in_rx = streamer
            .take_clipboard_in_rx()
            .expect("clipboard in rx once");
        // Publish a catalog so the host opens the app relay (idx0); the clipboard
        // relay (idx1) opens right after it in the same task.
        streamer.send_app_catalog(vec![core_proto::AppInfo {
            id: "x".into(),
            name: "X".into(),
            kind: 0,
            art: Vec::new(),
            running: false,
        }]);

        // Keep the video loop busy — the realistic condition for a starved relay.
        let stop = Arc::new(AtomicBool::new(false));
        let pusher = {
            let s = streamer.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    s.send(vec![0u8; 4096]);
                    std::thread::sleep(Duration::from_millis(8));
                }
            })
        };

        // Client: register an on_catalog (so it accepts idx0) AND a clipboard sink +
        // clipboard_rx (so it accepts idx1), matching a real clipboard-capable client.
        let got_host_clip = Arc::new(Mutex::new(None::<String>));
        let got_host_clip_cb = got_host_clip.clone();
        let (clip_tx, clip_rx) = tokio::sync::mpsc::unbounded_channel::<core_proto::ClipboardMsg>();
        let (_input_tx, input_rx) =
            tokio::sync::mpsc::unbounded_channel::<core_proto::InputEvent>();
        let on_catalog: Option<Box<dyn FnMut(Vec<core_proto::AppInfo>) + Send>> =
            Some(Box::new(|_apps| {}));
        let on_clipboard: Option<Box<dyn FnMut(String) + Send>> = Some(Box::new(move |text| {
            *got_host_clip_cb.lock().unwrap() = Some(text);
        }));
        std::thread::spawn(move || {
            let _ = super::run_video_client_with_input(
                vec![addr],
                None,
                None,
                |_| {},
                Some(input_rx),
                None,
                None,
                None,
                None,
                on_catalog,
                None, // on_app_notice: tests don't assert toasts
                None,
                false,
                false, // wants_hdr
                None,
                None,
                core_proto::StreamMode::default(),
                None,
                None,
                None,
                None, // transport_hint
                on_clipboard,
                Some(clip_rx),
            );
        });

        // Wait for connect + the relays to establish.
        for _ in 0..300 {
            if streamer.is_connected() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(300));

        // host → client: a MAX-size payload, to prove long text flows through the
        // relay (its frame exceeds the flow-control window AND the text cap, so it
        // exercises both the frame-cap headroom and reliable-stream chunking).
        let big = "L".repeat(core_proto::MAX_CLIPBOARD_BYTES);
        streamer.send_clipboard(big.clone());
        // client → host
        clip_tx
            .send(core_proto::ClipboardMsg::Text(
                "copied on the client".into(),
            ))
            .expect("send client clipboard");

        let mut host_to_client = None;
        let mut client_to_host = None;
        for _ in 0..300 {
            if host_to_client.is_none() {
                if let Some(t) = got_host_clip.lock().unwrap().clone() {
                    host_to_client = Some(t);
                }
            }
            if client_to_host.is_none() {
                if let Ok(core_proto::ClipboardMsg::Text(t)) = clip_in_rx.try_recv() {
                    client_to_host = Some(t);
                }
            }
            if host_to_client.is_some() && client_to_host.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        let _ = pusher.join();
        assert_eq!(
            host_to_client.as_deref(),
            Some(big.as_str()),
            "client must receive the host's FULL {}-byte clipboard (host→client)",
            big.len()
        );
        assert_eq!(
            client_to_host.as_deref(),
            Some("copied on the client"),
            "host must receive the client's clipboard (client→host)"
        );
    }

    /// Regression: client→host clipboard must work even when the host NEVER copies
    /// first. The host opens the clipboard relay but an idle host clipboard means it
    /// has nothing to send; without the stream-open nudge the client's accept_bi()
    /// would park forever and its client→host writer would never start, so the copy
    /// below would never arrive. This test does NOT call send_clipboard on the host.
    #[test]
    fn clipboard_client_to_host_without_prior_host_copy() {
        use std::net::{SocketAddr, UdpSocket};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::Duration;
        let _ = rustls::crypto::ring::default_provider().install_default();

        let host_id = Arc::new(core_crypto::Identity::generate());
        let probe = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let addr: SocketAddr = (Ipv4Addr::LOCALHOST, port).into();

        let streamer = Arc::new(super::QuicStreamer::start(addr, host_id).unwrap());
        let mut clip_in_rx = streamer
            .take_clipboard_in_rx()
            .expect("clipboard in rx once");
        streamer.send_app_catalog(vec![core_proto::AppInfo {
            id: "x".into(),
            name: "X".into(),
            kind: 0,
            art: Vec::new(),
            running: false,
        }]);

        let stop = Arc::new(AtomicBool::new(false));
        let pusher = {
            let s = streamer.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    s.send(vec![0u8; 4096]);
                    std::thread::sleep(Duration::from_millis(8));
                }
            })
        };

        let (clip_tx, clip_rx) = tokio::sync::mpsc::unbounded_channel::<core_proto::ClipboardMsg>();
        let (_input_tx, input_rx) =
            tokio::sync::mpsc::unbounded_channel::<core_proto::InputEvent>();
        let on_catalog: Option<Box<dyn FnMut(Vec<core_proto::AppInfo>) + Send>> =
            Some(Box::new(|_apps| {}));
        // Clipboard-capable client (has a clipboard_rx), but NO on_clipboard sink and
        // the host never sends — so only the nudge can open the stream.
        std::thread::spawn(move || {
            let _ = super::run_video_client_with_input(
                vec![addr],
                None,
                None,
                |_| {},
                Some(input_rx),
                None,
                None,
                None,
                None,
                on_catalog,
                None, // on_app_notice: tests don't assert toasts
                None,
                false,
                false,
                None,
                None,
                core_proto::StreamMode::default(),
                None,
                None,
                None,
                None, // transport_hint
                None, // on_clipboard: intentionally none
                Some(clip_rx),
            );
        });

        for _ in 0..300 {
            if streamer.is_connected() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(300));
        // The host has NOT copied anything. The client copies; the host must get it.
        clip_tx
            .send(core_proto::ClipboardMsg::Text("client-first".into()))
            .expect("send client clipboard");

        let mut got = None;
        for _ in 0..300 {
            if let Ok(core_proto::ClipboardMsg::Text(t)) = clip_in_rx.try_recv() {
                got = Some(t);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        let _ = pusher.join();
        assert_eq!(
            got.as_deref(),
            Some("client-first"),
            "client→host clipboard must work without the host copying first (stream-open nudge)"
        );
    }

    /// Dual-stack: a `[::]` server endpoint (V6ONLY off) must accept BOTH an IPv6
    /// client (`::1`) and an IPv4 client (`127.0.0.1`, reached v4-mapped from the
    /// dual-stack client socket) on the ONE socket. This is the core of the
    /// `[network] ipv6` toggle — the host-side bind + the client-side dial helpers.
    #[test]
    fn dualstack_endpoint_accepts_ipv6_and_ipv4() {
        use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
        let _ = rustls::crypto::ring::default_provider().install_default();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let server =
                super::dualstack_server_endpoint(super::server_config().unwrap(), 0).unwrap();
            let port = server.local_addr().unwrap().port();
            // Server: accept two connections; each gets a "K" on a uni stream.
            let srv = tokio::spawn(async move {
                for _ in 0..2 {
                    let conn = server.accept().await.unwrap().await.unwrap();
                    let mut s = conn.open_uni().await.unwrap();
                    s.write_all(b"K").await.unwrap();
                    s.finish().unwrap();
                    conn.closed().await;
                }
            });
            let mut client = super::client_endpoint(true).unwrap();
            client.set_default_client_config(super::client_config().unwrap());
            for target in [
                SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
                super::to_dialable(SocketAddr::from((Ipv4Addr::LOCALHOST, port)), true),
            ] {
                let conn = client.connect(target, "localhost").unwrap().await.unwrap();
                let mut r = conn.accept_uni().await.unwrap();
                let mut buf = [0u8; 1];
                r.read_exact(&mut buf).await.unwrap();
                assert_eq!(&buf, b"K", "dual-stack server must serve {target}");
                conn.close(0u32.into(), b"done");
            }
            srv.await.unwrap();
        });
    }

    /// The on-demand latency benchmark, end to end on loopback. The host pushes a
    /// few video AUs via `send_with_meta` (= opens a bench window, the meta riding
    /// inline with each AU; the transport fills `wire_send_ns`). The client must
    /// auto-detect that, fire the clock-ping burst, the host must answer with pongs,
    /// and after the meta stops the client must send a `WireLatencyReport` back —
    /// which `take_latency_rx` delivers with `samples > 0` and all-finite numbers.
    /// Loopback-only: no decoder, taps None (so decode/buffer/present are 0.0); we
    /// only exercise the host + wire path.
    #[test]
    fn latency_bench_roundtrips_report() {
        use std::net::{SocketAddr, UdpSocket};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::Duration;
        let _ = rustls::crypto::ring::default_provider().install_default();

        let host_id = Arc::new(core_crypto::Identity::generate());
        let probe = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let addr: SocketAddr = (Ipv4Addr::LOCALHOST, port).into();

        let streamer = Arc::new(super::QuicStreamer::start(addr, host_id).unwrap());
        let mut latency_rx = streamer.take_latency_rx().expect("latency rx once");

        // Host pusher: once a client is connected, send a burst of video AUs each
        // paired with a FrameStageMeta (UNIX-nanos stamps with realistic gaps), then
        // stop emitting meta so the client's idle timer closes the window.
        let stop = Arc::new(AtomicBool::new(false));
        let pusher = {
            let s = streamer.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                // Wait for the client to be connected before benching.
                while !s.is_connected() && !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(5));
                }
                let now = || {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos() as u64
                };
                for idx in 0..20u64 {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    // Back-date the stamps so the *real* wire_send_ns the transport
                    // takes at write_all (≈now) lands AFTER encode_done — i.e. the
                    // simulated compose+encode happened in the immediate past, just
                    // like a real host where the AU is encoded before it's sent.
                    let encode_done = now();
                    let encode_start = encode_done - 2_000_000; // 2ms encode
                    let capture = encode_start - 800_000; // 0.8ms compose
                    let meta = core_proto::FrameStageMeta {
                        frame_idx: idx,
                        capture_ns: capture,
                        encode_start_ns: encode_start,
                        encode_done_ns: encode_done,
                        wire_send_ns: 0, // filled by the transport at write_all
                    };
                    // One call carries the AU and its meta 1:1 down the same channel.
                    s.send_with_meta(vec![idx as u8, 1, 2, 3], false, meta);
                    std::thread::sleep(Duration::from_millis(16)); // ~60fps
                }
                // Stop emitting meta; the client's idle timer (~750ms) ends the
                // window and triggers the report. Keep the connection alive.
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(20));
                }
            })
        };

        // Client: participates in the bench automatically (taps None).
        let client_done = Arc::new(AtomicBool::new(false));
        {
            let client_done = client_done.clone();
            std::thread::spawn(move || {
                let _ = super::run_video_client_with_input(
                    vec![addr],
                    None,
                    None,
                    |_| {},
                    None,
                    None,
                    None,
                    None,
                    None,  // latency_taps None → decode/present stay 0.0
                    None,  // on_catalog
                    None,  // on_app_notice
                    None,  // app_launch_rx
                    false, // bench client: no gyro
                    false, // bench client: no HDR
                    None,  // on_link_health
                    None,  // request_idr
                    core_proto::StreamMode::default(), // requested_mode
                    None,  // res_pref
                    None,  // bitrate_pref
                    None,  // fps_pref
                    None,  // transport_hint
                    None,  // on_clipboard
                    None,  // clipboard_rx
                );
                client_done.store(true, Ordering::Release);
            });
        }

        // The host should receive exactly one report once the window closes.
        let report = {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(10), latency_rx.recv())
                    .await
                    .ok()
                    .flatten()
            })
        };
        stop.store(true, Ordering::Relaxed);
        let _ = pusher.join();

        let report = report.expect("host must receive a WireLatencyReport");
        assert!(report.samples > 0, "report must have folded ≥1 meta");
        assert_eq!(&report.codec_fourcc, b"hevc");
        // Every numeric field must be finite (no NaN/inf from a bad clock calc).
        for v in [
            report.rtt_ms,
            report.clock_offset_ms,
            report.compose_ms,
            report.encode_ms,
            report.host_send_ms,
            report.nexus_ms,
            report.nexus_uncertainty_ms,
            report.buffer_ms,
            report.decode_ms,
            report.present_ms,
            report.total_ms,
        ] {
            assert!(v.is_finite(), "report field must be finite, got {v}");
        }
        // Taps None → the client-local stages are exactly 0 and non-negative.
        assert_eq!(report.decode_ms, 0.0);
        assert_eq!(report.buffer_ms, 0.0);
        assert_eq!(report.present_ms, 0.0);
        assert!(report.decode_ms >= 0.0);
        assert!(report.buffer_ms >= 0.0);
        // The host stamped a 0.8ms compose + 2ms encode gap; both should survive.
        assert!(
            report.compose_ms > 0.0,
            "compose stage should be nonzero (host stamped 0.8ms)"
        );
        assert!(
            report.encode_ms > 0.0,
            "encode stage should be nonzero (host stamped 2ms)"
        );
        // nexus is exactly half the (min) RTT.
        let half_rtt = report.rtt_ms / 2.0;
        assert!(
            (report.nexus_ms - half_rtt).abs() < 1.0e-3,
            "nexus_ms ({}) should equal rtt_ms/2 ({half_rtt})",
            report.nexus_ms
        );
    }
}
