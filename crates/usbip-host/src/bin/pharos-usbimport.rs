// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! `pharos-usbimport` — the Pharos USB/IP import bridge.
//!
//! Accepts a Trireme exporter over the standalone QUIC relay, reads the
//! device's hello, materializes the remote device as a real local USB device,
//! and pumps raw URB bytes between the local virtual host controller and the
//! QUIC stream.
//!
//! - **Linux** (run as root): hands a `socketpair` end to the kernel
//!   `vhci_hcd` via sysfs.
//! - **Windows**: drives the usbip-win2 vhci driver through a loopback USB/IP
//!   shim — see the [`win`] module (issue #3).
//!
//! Usage: `pharos-usbimport [listen-addr]`  (default 0.0.0.0:4434)

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    imp::run()
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    win::run()
}

/// On SIGTERM/SIGINT (Linux: `systemctl stop`, the host launcher's reap) or
/// Ctrl-C / console close (Windows), send every connected exporter a QUIC close
/// before exiting, so phones re-export to the next importer right away instead
/// of waiting out the 20s idle timeout. A second signal still kills at once.
#[cfg(any(target_os = "linux", windows))]
fn close_on_shutdown(endpoint: transport::UsbImportEndpoint) {
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        tracing::info!("shutting down: closing exporter connections");
        transport::usb_import_close(&endpoint).await;
        std::process::exit(0);
    });
}

#[cfg(target_os = "linux")]
async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        tracing::warn!("can't install signal handlers; stop will not close exporters cleanly");
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

#[cfg(windows)]
async fn wait_for_shutdown_signal() {
    use tokio::signal::windows::{ctrl_close, ctrl_shutdown};
    let (Ok(mut close), Ok(mut shut)) = (ctrl_close(), ctrl_shutdown()) else {
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = close.recv() => {}
        _ = shut.recv() => {}
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn main() {
    eprintln!(
        "pharos-usbimport needs a virtual host controller (Linux vhci_hcd or \
         Windows usbip-win2); unsupported on this platform."
    );
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
mod imp {
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream as StdUnixStream;

    use anyhow::{bail, Context, Result};
    use nix::sys::socket::{socketpair, AddressFamily, SockFlag, SockType};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const VHCI: &str = "/sys/devices/platform/vhci_hcd.0";

    #[tokio::main]
    pub async fn run() -> Result<()> {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "pharos_usbimport=info,info".into()),
            )
            .init();

        let addr: std::net::SocketAddr = match std::env::args().nth(1) {
            Some(a) => a.parse().context("parse listen addr")?,
            None => (std::net::Ipv4Addr::UNSPECIFIED, core_proto::ports::USBIP).into(),
        };
        if !std::path::Path::new(VHCI).exists() {
            bail!("{VHCI} missing — is the vhci_hcd module loaded? (modprobe vhci-hcd)");
        }
        let endpoint = transport::usb_import_endpoint(addr).context("bind USB relay")?;
        tracing::info!("Pharos USB import bridge listening on {addr}");
        crate::close_on_shutdown(endpoint.clone());

        loop {
            let link = match transport::usb_import_accept(&endpoint).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!("accept: {e:?}");
                    continue;
                }
            };
            tracing::info!("exporter connected from {}", link.remote());
            // Serve each device concurrently — Trireme can share several at once,
            // each its own connection + vhci port.
            tokio::spawn(async move {
                if let Err(e) = serve(link).await {
                    tracing::error!("session ended: {e:?}");
                }
            });
        }
    }

    /// Serialize port selection + attach so two concurrent sessions don't both
    /// claim the same free vhci port before the status reflects it.
    static ATTACH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    async fn serve(mut link: transport::UsbLink) -> Result<()> {
        // Hello, v1 or v2 — vhci only needs the v1 triplet (the v2 descriptor
        // block exists for the Windows bridge), but the endpoint advertises v2
        // so new exporters send it; parse it either way to stay in frame.
        let hello = transport::read_usb_hello(&mut link.recv)
            .await
            .context("read hello")?;
        let (name, devid, speed) = (hello.name, hello.devid, hello.speed);
        let (port, vhci_fd, relay_fd) = {
            let _guard = ATTACH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let port = find_free_port(speed >= 5)?;
            tracing::info!(
                "attaching '{name}' (devid={devid:#010x} speed={speed}) on vhci port {port}"
            );
            // socketpair: one end to the kernel (vhci), one end we relay.
            let (vhci_fd, relay_fd) = socketpair(
                AddressFamily::Unix,
                SockType::Stream,
                None,
                SockFlag::empty(),
            )
            .context("socketpair")?;
            // attach: "<port> <sockfd> <devid> <speed>". The fd must be live here.
            let line = format!("{port} {} {devid} {speed}", vhci_fd.as_raw_fd());
            std::fs::write(format!("{VHCI}/attach"), &line)
                .with_context(|| format!("vhci attach (need root): {line}"))?;
            (port, vhci_fd, relay_fd)
        };
        tracing::info!("vhci attached on port {port}; relaying URBs");

        let std_relay = StdUnixStream::from(relay_fd);
        std_relay.set_nonblocking(true)?;
        let unix = tokio::net::UnixStream::from_std(std_relay)?;
        let (mut ur, mut uw) = unix.into_split();
        let mut send = link.send;
        let mut recv = link.recv;

        // vhci → exporter (CMD_SUBMIT / CMD_UNLINK from the kernel).
        let a = tokio::spawn(async move {
            let mut buf = [0u8; 32 * 1024];
            loop {
                match ur.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if send.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        // exporter → vhci (RET_SUBMIT / RET_UNLINK back to the kernel).
        let b = tokio::spawn(async move {
            let mut buf = [0u8; 32 * 1024];
            while let Ok(Some(n)) = recv.read(&mut buf).await {
                if uw.write_all(&buf[..n]).await.is_err() {
                    break;
                }
            }
        });
        let _ = tokio::join!(a, b);

        // Tear the port down so it's reusable.
        let _ = std::fs::write(format!("{VHCI}/detach"), format!("{port}"));
        let _ = vhci_fd; // held until here
        tracing::info!("detached vhci port {port}");
        Ok(())
    }

    /// Find a free vhci port of the right speed class (hs ports for < SuperSpeed,
    /// ss ports otherwise). Status columns: hub port sta spd dev sockfd busid;
    /// `sta == 4` (VDEV_ST_NULL) means free.
    fn find_free_port(want_super: bool) -> Result<u32> {
        let status =
            std::fs::read_to_string(format!("{VHCI}/status")).context("read vhci status")?;
        for line in status.lines().skip(1) {
            let c: Vec<&str> = line.split_whitespace().collect();
            if c.len() < 3 {
                continue;
            }
            let is_super = c[0] == "ss";
            let (Ok(port), Ok(sta)) = (c[1].parse::<u32>(), c[2].parse::<u32>()) else {
                continue;
            };
            if sta == 4 && is_super == want_super {
                return Ok(port);
            }
        }
        bail!(
            "no free vhci port for {} device",
            if want_super {
                "super-speed"
            } else {
                "high-speed"
            }
        );
    }
}

#[cfg(windows)]
mod win {
    //! The Windows import bridge (issue #3).
    //!
    //! There is no fd handover on Windows: the usbip-win2 vhci driver dials TCP
    //! *itself* (WSK) and speaks the full USB/IP v1.1.1 import handshake. So
    //! this backend inverts the Linux flow — it runs a **loopback TCP shim
    //! speaking the USB/IP server side** and lets the driver import from it:
    //!
    //! 1. A Trireme exporter connects on the QUIC relay (4434, as on Linux) and
    //!    sends a v2 hello (descriptor block — required: the driver builds its
    //!    PDO from OP_REP_IMPORT's fields, so they can't be fabricated).
    //! 2. The session registers under a synthetic busid (`1-<slot>`) and runs
    //!    `usbip.exe attach -r 127.0.0.1 -b <busid>`.
    //! 3. The driver connects to the shim (fixed USB/IP port 3240 — win2's CLI
    //!    exposes no port knob), sends OP_REQ_IMPORT, gets OP_REP_IMPORT built
    //!    from the hello, and the shim hands the imported TCP socket to the
    //!    QUIC session.
    //! 4. The session byte-pumps URBs between that socket and the QUIC stream —
    //!    the same relay the Linux path runs on a socketpair.
    //!
    //! Teardown detaches the vhci port (best-effort, via `usbip.exe port`
    //! parsing) so a dead session doesn't leave a zombie device.
    //!
    //! Known limitation: the shim listens on loopback, so any *local* process
    //! could race the driver to OP_REQ_IMPORT and receive the device's URB
    //! stream (win2 validates busid as `bus-port` digits, leaving no room for
    //! a capability token). Equivalent to local access to a plugged-in device;
    //! acceptable on a single-user rig, revisit if multi-user hosts matter.

    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use anyhow::{bail, Context, Result};
    use core_usbip::hello::Hello;
    use core_usbip::op::{
        self, ExportedDevice, OpHeader, BUSID_LEN, OP_HEADER_LEN, OP_REQ_DEVLIST, OP_REQ_IMPORT,
        ST_NA,
    };
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::oneshot;

    /// The standard USB/IP TCP port. The win2 vhci driver always dials it and
    /// `usbip.exe` has no port option, so the shim must own it — a conflicting
    /// USB/IP server (usbipd-win) on the same box is a hard error at startup.
    const USBIP_TCP_PORT: u16 = 3240;

    /// Synthetic bus every imported device lives on; busid = `1-<slot>`.
    const SYNTH_BUSNUM: u32 = 1;

    /// devnum slots 1..=15 on the synthetic bus — more concurrent imported
    /// devices than vhci ports exist. Slot N ↔ busid `1-N`.
    const SLOT_COUNT: usize = 15;

    const INSTALL_HINT: &str = "usbip-win2 not detected — install the signed driver/CLI from \
         https://github.com/vadimgrn/usbip-win2/releases (the installer restarts USB hubs, \
         expect a consent prompt), or point PHAROS_USBIP_EXE at its usbip.exe";

    /// One registered-but-not-yet-imported (or re-arming) device session.
    /// `take` hands the imported TCP socket to the QUIC session exactly once;
    /// `None` while the device is attached (import requests then get ST_NA).
    struct Pending<T> {
        dev: ExportedDevice,
        take: Option<oneshot::Sender<T>>,
    }

    /// busid → pending session. Generic over the pumped socket type so the
    /// setup protocol is unit-testable on in-memory duplex streams.
    type Registry<T> = Arc<Mutex<HashMap<String, Pending<T>>>>;

    #[tokio::main]
    pub async fn run() -> Result<()> {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "pharos_usbimport=info,info".into()),
            )
            .init();

        let addr: std::net::SocketAddr = match std::env::args().nth(1) {
            Some(a) => a.parse().context("parse listen addr")?,
            None => (std::net::Ipv4Addr::UNSPECIFIED, core_proto::ports::USBIP).into(),
        };

        let usbip = find_usbip();
        probe_driver(&usbip);

        // The loopback shim the vhci driver imports from. Owning 3240 outright
        // (vs per-session listeners) lets the driver's own reconnect logic
        // always find us.
        let shim = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, USBIP_TCP_PORT))
            .await
            .with_context(|| {
                format!(
                    "bind 127.0.0.1:{USBIP_TCP_PORT} — another USB/IP server (usbipd-win?) is \
                     running; stop it, the vhci driver only dials the standard port"
                )
            })?;
        let registry: Registry<TcpStream> = Arc::new(Mutex::new(HashMap::new()));
        let slots = Arc::new(Mutex::new([false; SLOT_COUNT + 1]));
        {
            let registry = registry.clone();
            tokio::spawn(async move {
                loop {
                    match shim.accept().await {
                        Ok((sock, from)) => {
                            let registry = registry.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_shim_conn(sock, &registry).await {
                                    tracing::debug!("shim connection from {from}: {e:#}");
                                }
                            });
                        }
                        Err(e) => tracing::error!("shim accept: {e}"),
                    }
                }
            });
        }

        let endpoint = transport::usb_import_endpoint(addr).context("bind USB relay")?;
        tracing::info!(
            "Pharos USB import bridge (Windows) listening on {addr}; \
             shim on 127.0.0.1:{USBIP_TCP_PORT}; usbip.exe = {}",
            usbip.display()
        );
        crate::close_on_shutdown(endpoint.clone());

        loop {
            let link = match transport::usb_import_accept(&endpoint).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!("accept: {e:?}");
                    continue;
                }
            };
            tracing::info!("exporter connected from {}", link.remote());
            let registry = registry.clone();
            let slots = slots.clone();
            let usbip = usbip.clone();
            tokio::spawn(async move {
                if let Err(e) = serve(link, registry, slots, usbip).await {
                    tracing::error!("session ended: {e:#}");
                }
            });
        }
    }

    /// One QUIC exporter session: hello → register → attach → pump → teardown.
    async fn serve(
        mut link: transport::UsbLink,
        registry: Registry<TcpStream>,
        slots: Arc<Mutex<[bool; SLOT_COUNT + 1]>>,
        usbip: PathBuf,
    ) -> Result<()> {
        let hello = transport::read_usb_hello(&mut link.recv)
            .await
            .context("read hello")?;
        if hello.desc.is_none() {
            bail!(
                "exporter for '{}' sent a v1 hello — the Windows bridge needs the v2 \
                 descriptor block (OP_REP_IMPORT is built from it); update Trireme",
                hello.name
            );
        }
        let slot = alloc_slot(&slots)
            .with_context(|| format!("no free import slot (max {SLOT_COUNT} devices)"))?;
        let busid = format!("{SYNTH_BUSNUM}-{slot}");
        tracing::info!(
            "attaching '{}' (devid={:#010x} speed={}) as busid {busid}",
            hello.name,
            hello.devid,
            hello.speed
        );

        let result = session(link, &registry, &usbip, &busid, slot, &hello).await;

        registry.lock().unwrap().remove(&busid);
        detach_best_effort(&usbip, &busid).await;
        slots.lock().unwrap()[slot as usize] = false;
        tracing::info!("detached busid {busid} ('{}')", hello.name);
        result
    }

    /// The attach + pump body of a session; `serve` owns cleanup around it.
    async fn session(
        link: transport::UsbLink,
        registry: &Registry<TcpStream>,
        usbip: &std::path::Path,
        busid: &str,
        slot: u32,
        hello: &Hello,
    ) -> Result<()> {
        let dev = exported_device(busid, slot, hello);
        let mut send = link.send;
        let mut recv = link.recv;

        // `usbip attach` blocks until the device materializes (or errors), so
        // run it off-thread and watch it alongside the import handoff.
        let mut attach = {
            let usbip = usbip.to_path_buf();
            let busid = busid.to_string();
            Some(tokio::task::spawn_blocking(move || {
                std::process::Command::new(usbip)
                    .args(["attach", "-r", "127.0.0.1", "-b", &busid])
                    .output()
            }))
        };

        // The driver may drop + redial the TCP connection on its own (win2 has
        // in-kernel reconnect logic, and some CLI flows probe before the real
        // import). Re-arm the handoff a bounded number of times as long as no
        // URB traffic has flowed — a pump that moved bytes and then ended is a
        // real session end, not a probe artifact.
        let mut rearms = 0;
        loop {
            let (tx, rx) = oneshot::channel();
            registry.lock().unwrap().insert(
                busid.to_string(),
                Pending {
                    dev: dev.clone(),
                    take: Some(tx),
                },
            );
            let mut sock = await_socket(rx, &mut attach, busid).await?;
            let started = tokio::time::Instant::now();
            let moved = pump(&mut sock, &mut send, &mut recv).await;
            tracing::debug!("pump for {busid} ended after {moved} bytes");
            if moved == 0 && started.elapsed() < std::time::Duration::from_secs(2) && rearms < 3 {
                rearms += 1;
                continue;
            }
            return Ok(());
        }
    }

    /// Wait for the shim to hand over the imported TCP socket, failing fast if
    /// `usbip attach` errors first (driver not installed, busid rejected, …).
    async fn await_socket(
        mut rx: oneshot::Receiver<TcpStream>,
        attach: &mut Option<tokio::task::JoinHandle<std::io::Result<std::process::Output>>>,
        busid: &str,
    ) -> Result<TcpStream> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            tokio::select! {
                r = &mut rx => {
                    return r.with_context(|| format!("handoff channel died for {busid}"));
                }
                a = async { attach.as_mut().unwrap().await }, if attach.is_some() => {
                    let out = a.context("attach task")?
                        .with_context(|| format!("spawn usbip.exe — {INSTALL_HINT}"))?;
                    if !out.status.success() {
                        bail!(
                            "usbip attach -b {busid} failed ({}): {}{}\n{INSTALL_HINT}",
                            out.status,
                            String::from_utf8_lossy(&out.stdout).trim(),
                            String::from_utf8_lossy(&out.stderr).trim(),
                        );
                    }
                    // Success — the import handshake is done or imminent; keep
                    // waiting for the socket handoff.
                    *attach = None;
                }
                _ = tokio::time::sleep_until(deadline) => {
                    bail!("timed out waiting for the vhci driver to import {busid}");
                }
            }
        }
    }

    /// Byte-pump URBs between the driver's TCP socket and the QUIC stream
    /// until either side ends. Returns total bytes moved (both directions).
    /// Single task + select (rather than the Linux twin-task split) so the
    /// caller keeps ownership of the QUIC halves for re-arming. Generic over
    /// the QUIC halves (quinn streams implement tokio Async{Read,Write}).
    async fn pump<QW, QR>(sock: &mut TcpStream, send: &mut QW, recv: &mut QR) -> u64
    where
        QW: AsyncWrite + Unpin,
        QR: AsyncRead + Unpin,
    {
        let _ = sock.set_nodelay(true);
        let (mut tr, mut tw) = sock.split();
        let mut tbuf = vec![0u8; 32 * 1024];
        let mut qbuf = vec![0u8; 32 * 1024];
        let mut moved = 0u64;
        loop {
            tokio::select! {
                // vhci → exporter (CMD_SUBMIT / CMD_UNLINK from the driver).
                r = tr.read(&mut tbuf) => match r {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if send.write_all(&tbuf[..n]).await.is_err() {
                            break;
                        }
                        moved += n as u64;
                    }
                },
                // exporter → vhci (RET_SUBMIT / RET_UNLINK back to the driver).
                r = recv.read(&mut qbuf) => match r {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tw.write_all(&qbuf[..n]).await.is_err() {
                            break;
                        }
                        moved += n as u64;
                    }
                },
            }
        }
        moved
    }

    /// Serve one shim TCP connection: answer the USB/IP setup ops and, on a
    /// successful import, hand the socket to the owning QUIC session.
    async fn handle_shim_conn(mut sock: TcpStream, registry: &Registry<TcpStream>) -> Result<()> {
        let _ = sock.set_nodelay(true);
        match answer_setup(&mut sock, registry).await? {
            Some(sender) => {
                // OP_REP_IMPORT is already on the wire; the session pumps from here.
                let _ = sender.send(sock);
                Ok(())
            }
            None => Ok(()), // devlist / rejected import / noise — just close
        }
    }

    /// The USB/IP server-side setup protocol, IO-generic for tests. Reads one
    /// OP request; a successful OP_REQ_IMPORT atomically claims the pending
    /// session's handoff sender, writes OP_REP_IMPORT, and returns the sender.
    async fn answer_setup<S, T>(
        io: &mut S,
        registry: &Registry<T>,
    ) -> Result<Option<oneshot::Sender<T>>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut hdr = [0u8; OP_HEADER_LEN];
        io.read_exact(&mut hdr).await.context("read op header")?;
        let op = OpHeader::read(&hdr);
        // Version is informational (tools send 0x0111); don't enforce.
        match op.code {
            OP_REQ_DEVLIST => {
                let devs: Vec<ExportedDevice> = {
                    let reg = registry.lock().unwrap();
                    reg.values().map(|p| p.dev.clone()).collect()
                };
                io.write_all(&op::rep_devlist(&devs)).await?;
                Ok(None)
            }
            OP_REQ_IMPORT => {
                let mut bid = [0u8; BUSID_LEN];
                io.read_exact(&mut bid).await.context("read import busid")?;
                let busid = op::parse_busid(&bid);
                // Claim the sender under the lock; reply after releasing it.
                let claimed = {
                    let mut reg = registry.lock().unwrap();
                    reg.get_mut(&busid).map(|p| (p.dev.clone(), p.take.take()))
                };
                match claimed {
                    Some((dev, Some(sender))) => {
                        io.write_all(&op::rep_import(&dev)).await?;
                        Ok(Some(sender))
                    }
                    Some((_, None)) => {
                        tracing::warn!("import for {busid} while already attached");
                        io.write_all(&op::rep_import_err(ST_NA)).await?;
                        Ok(None)
                    }
                    None => {
                        tracing::warn!("import for unknown busid {busid:?}");
                        io.write_all(&op::rep_import_err(ST_NA)).await?;
                        Ok(None)
                    }
                }
            }
            other => bail!("unsupported USB/IP op {other:#06x}"),
        }
    }

    /// The OP_REP_IMPORT device block for a session: the exporter's real
    /// descriptors under our synthetic bus identity (the driver validates
    /// busid/busnum/devnum against each other, not against the remote's bus).
    fn exported_device(busid: &str, slot: u32, hello: &Hello) -> ExportedDevice {
        let desc = hello.desc.clone().unwrap_or_default();
        ExportedDevice {
            path: format!("/pharos/usb/{busid}"), // display-only
            busid: busid.to_string(),
            busnum: SYNTH_BUSNUM,
            devnum: slot,
            speed: hello.speed,
            id_vendor: desc.id_vendor,
            id_product: desc.id_product,
            bcd_device: desc.bcd_device,
            device_class: desc.device_class,
            device_subclass: desc.device_subclass,
            device_protocol: desc.device_protocol,
            configuration_value: desc.configuration_value,
            num_configurations: desc.num_configurations,
            interfaces: desc.interfaces,
        }
    }

    /// Claim the lowest free devnum slot (1-based).
    fn alloc_slot(slots: &Mutex<[bool; SLOT_COUNT + 1]>) -> Option<u32> {
        let mut s = slots.lock().unwrap();
        (1..=SLOT_COUNT).find(|&i| !s[i]).map(|i| {
            s[i] = true;
            i as u32
        })
    }

    /// Locate `usbip.exe`: explicit override, our own directory, the win2
    /// installer default, then PATH.
    fn find_usbip() -> PathBuf {
        if let Ok(p) = std::env::var("PHAROS_USBIP_EXE") {
            let p = p.trim();
            if !p.is_empty() {
                return p.into();
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let sib = dir.join("usbip.exe");
                if sib.exists() {
                    return sib;
                }
            }
        }
        if let Ok(pf) = std::env::var("ProgramFiles") {
            let inst = std::path::Path::new(&pf).join("USBip").join("usbip.exe");
            if inst.exists() {
                return inst;
            }
        }
        PathBuf::from("usbip.exe")
    }

    /// Startup health check: `usbip port` opens the vhci device, so it proves
    /// both the CLI and the driver in one shot. Non-fatal — sessions repeat
    /// the guidance on attach failure.
    fn probe_driver(usbip: &std::path::Path) {
        match std::process::Command::new(usbip).arg("port").output() {
            Ok(out) if out.status.success() => {
                tracing::info!("usbip-win2 vhci driver detected");
            }
            Ok(out) => tracing::warn!(
                "`usbip port` failed ({}): {} — vhci driver not installed? {INSTALL_HINT}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Err(e) => tracing::warn!("could not run {}: {e} — {INSTALL_HINT}", usbip.display()),
        }
    }

    /// Free the vhci port for `busid` (best-effort): find it in `usbip port`
    /// output, then `usbip detach -p <n>`. A busid that never imported simply
    /// won't be listed.
    async fn detach_best_effort(usbip: &std::path::Path, busid: &str) {
        let usbip = usbip.to_path_buf();
        let busid = busid.to_string();
        let _ = tokio::task::spawn_blocking(move || {
            let Ok(out) = std::process::Command::new(&usbip).arg("port").output() else {
                return;
            };
            let listing = String::from_utf8_lossy(&out.stdout);
            let Some(port) = port_for_busid(&listing, &busid) else {
                return;
            };
            match std::process::Command::new(&usbip)
                .args(["detach", "-p", &port.to_string()])
                .output()
            {
                Ok(o) if o.status.success() => {
                    tracing::debug!("detached vhci port {port} ({busid})")
                }
                Ok(o) => {
                    let err = String::from_utf8_lossy(&o.stderr);
                    let err = err.trim();
                    // Benign race, live-verified: when the exporter drops, the
                    // driver notices its dead TCP connection and detaches the
                    // port itself before this best-effort pass runs.
                    if err.contains("not connected") {
                        tracing::debug!("vhci port {port} ({busid}) already self-detached");
                    } else {
                        tracing::warn!("usbip detach -p {port} failed: {err}");
                    }
                }
                Err(e) => tracing::warn!("usbip detach: {e}"),
            }
        })
        .await;
    }

    /// Find the vhci port number bound to `busid` in `usbip port` output.
    /// Tolerant parse: remember the last `Port NN` heading, match the busid on
    /// a following line (`usbip://…/1-5` or a bare `1-5 -> …` mapping).
    /// NOTE: written against the Linux-style listing; verify against real
    /// usbip-win2 output on the rig and tighten if it differs.
    fn port_for_busid(listing: &str, busid: &str) -> Option<u32> {
        let mut current: Option<u32> = None;
        for line in listing.lines() {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix("Port ") {
                let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                current = digits.parse().ok();
            }
            let hits = t.ends_with(&format!("/{busid}"))
                || t.starts_with(&format!("{busid} ->"))
                || t.contains(&format!("/{busid} "));
            if hits {
                return current;
            }
        }
        None
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn test_registry(busid: &str) -> (Registry<()>, oneshot::Receiver<()>) {
            let dev = exported_device(
                busid,
                5,
                &Hello {
                    name: "Pad".into(),
                    devid: (1 << 16) | 5,
                    speed: 3,
                    desc: Some(core_usbip::hello::DeviceDesc {
                        id_vendor: 0x054c,
                        id_product: 0x0ce6,
                        interfaces: vec![core_usbip::op::Interface {
                            class: 3,
                            subclass: 0,
                            protocol: 0,
                        }],
                        ..Default::default()
                    }),
                },
            );
            let (tx, rx) = oneshot::channel();
            let registry: Registry<()> = Arc::new(Mutex::new(HashMap::new()));
            registry.lock().unwrap().insert(
                busid.to_string(),
                Pending {
                    dev,
                    take: Some(tx),
                },
            );
            (registry, rx)
        }

        /// Drive `answer_setup` with a scripted client over an in-memory duplex;
        /// returns (client-visible reply bytes, setup outcome).
        async fn roundtrip(
            request: &[u8],
            registry: &Registry<()>,
        ) -> (Vec<u8>, Option<oneshot::Sender<()>>) {
            let (mut client, mut server) = tokio::io::duplex(64 * 1024);
            client.write_all(request).await.unwrap();
            let outcome = answer_setup(&mut server, registry).await.unwrap();
            drop(server);
            let mut reply = Vec::new();
            client.read_to_end(&mut reply).await.unwrap();
            (reply, outcome)
        }

        #[tokio::test]
        async fn import_claims_session_and_replies_device() {
            let (registry, mut rx) = test_registry("1-5");
            let (reply, outcome) = roundtrip(&op::req_import("1-5"), &registry).await;
            let sender = outcome.expect("import must hand back the session sender");
            assert_eq!(reply.len(), OP_HEADER_LEN + op::DEVICE_LEN);
            let hdr = OpHeader::read(reply[..OP_HEADER_LEN].try_into().unwrap());
            assert_eq!((hdr.code, hdr.status), (op::OP_REP_IMPORT, op::ST_OK));
            // busid echoed at path[256].. and vid where it belongs.
            assert_eq!(&reply[OP_HEADER_LEN + 256..OP_HEADER_LEN + 259], b"1-5");
            assert_eq!(
                &reply[OP_HEADER_LEN + 300..OP_HEADER_LEN + 302],
                &[0x05, 0x4c]
            );
            // The handoff channel is live end-to-end.
            sender.send(()).unwrap();
            rx.try_recv().unwrap();
        }

        #[tokio::test]
        async fn second_import_gets_st_na() {
            let (registry, _rx) = test_registry("1-5");
            let (_, first) = roundtrip(&op::req_import("1-5"), &registry).await;
            assert!(first.is_some());
            let (reply, second) = roundtrip(&op::req_import("1-5"), &registry).await;
            assert!(second.is_none(), "attached busid must not import twice");
            assert_eq!(reply.len(), OP_HEADER_LEN);
            let hdr = OpHeader::read(reply[..OP_HEADER_LEN].try_into().unwrap());
            assert_eq!(hdr.status, ST_NA);
        }

        #[tokio::test]
        async fn unknown_busid_gets_st_na() {
            let (registry, _rx) = test_registry("1-5");
            let (reply, outcome) = roundtrip(&op::req_import("9-9"), &registry).await;
            assert!(outcome.is_none());
            let hdr = OpHeader::read(reply[..OP_HEADER_LEN].try_into().unwrap());
            assert_eq!(hdr.status, ST_NA);
        }

        #[tokio::test]
        async fn devlist_lists_registered_devices() {
            let (registry, _rx) = test_registry("1-5");
            let req = OpHeader {
                version: op::USBIP_VERSION,
                code: OP_REQ_DEVLIST,
                status: 0,
            }
            .to_bytes();
            let (reply, outcome) = roundtrip(&req, &registry).await;
            assert!(outcome.is_none());
            assert_eq!(
                reply.len(),
                OP_HEADER_LEN + 4 + op::DEVICE_LEN + op::INTERFACE_LEN
            );
            assert_eq!(&reply[OP_HEADER_LEN..OP_HEADER_LEN + 4], &[0, 0, 0, 1]);
        }

        #[test]
        fn port_listing_parse_finds_busid() {
            let listing = "\
Imported USB devices
====================
Port 00: <Port in Use> at High Speed(480Mbps)
       Sony Corp. : DualSense wireless controller (054c:0ce6)
       1-5 -> usbip://127.0.0.1:3240/1-5
Port 03: <Port in Use> at Full Speed(12Mbps)
       unknown vendor : unknown product (1234:5678)
       2-1 -> usbip://10.0.0.2:3240/2-1
";
            assert_eq!(port_for_busid(listing, "1-5"), Some(0));
            assert_eq!(port_for_busid(listing, "2-1"), Some(3));
            assert_eq!(port_for_busid(listing, "1-9"), None);
        }

        #[test]
        fn slot_allocation_reuses_freed() {
            let slots = Mutex::new([false; SLOT_COUNT + 1]);
            assert_eq!(alloc_slot(&slots), Some(1));
            assert_eq!(alloc_slot(&slots), Some(2));
            slots.lock().unwrap()[1] = false;
            assert_eq!(alloc_slot(&slots), Some(1));
        }
    }
}
