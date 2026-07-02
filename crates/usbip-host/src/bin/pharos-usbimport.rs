//! `pharos-usbimport` — the Pharos USB/IP import bridge.
//!
//! Accepts a Trireme exporter over the standalone QUIC relay, reads the device's
//! `{devid, speed}` handshake, hands a `socketpair` end to the kernel `vhci_hcd`
//! (which materializes the remote device as a real local USB device), and pumps
//! raw URB bytes between that socket and the QUIC stream. Run as root.
//!
//! Usage: `sudo pharos-usbimport [listen-addr]`  (default 0.0.0.0:4434)

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    imp::run()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("pharos-usbimport is Linux-only (requires the vhci_hcd kernel module).");
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
        // Handshake: name (u16 len + UTF-8 bytes), then devid (u32 BE) + speed (u32 BE).
        let mut nlen = [0u8; 2];
        link.recv
            .read_exact(&mut nlen)
            .await
            .context("read name len")?;
        let mut nb = vec![0u8; u16::from_be_bytes(nlen) as usize];
        link.recv.read_exact(&mut nb).await.context("read name")?;
        let name = String::from_utf8_lossy(&nb).into_owned();
        let mut hs = [0u8; 8];
        link.recv
            .read_exact(&mut hs)
            .await
            .context("read handshake")?;
        let devid = u32::from_be_bytes([hs[0], hs[1], hs[2], hs[3]]);
        let speed = u32::from_be_bytes([hs[4], hs[5], hs[6], hs[7]]);
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
