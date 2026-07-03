// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos/Trireme authors

//! Pairing + identity for Pharos/Trireme.
//!
//! Shared by host and client (via `core-ffi` on Android/iOS, directly on
//! desktop) so pairing has exactly one implementation and one test suite.
//! The thing that mattered, per `lesson-pairing-once-pake`: GameStream's
//! salted-PIN-AES + cert-signing dance is replaced with a real PAKE, written
//! once, used identically on both sides.
//!
//! ## Pieces
//!
//! * [`Identity`] — long-term Ed25519 keypair, persisted to TOML
//!   (`identity.toml`) so the same key is reused across runs. Each peer
//!   generates its own, then pinning happens at pairing time.
//!
//! * [`PairingSession`] — drives one side of a PAKE handshake (SPAKE2 over
//!   Ed25519, RFC 9382). Both sides feed in their roles and the PIN; if the
//!   PINs match, both end up with the same 32-byte shared secret. If they
//!   don't, the secrets diverge silently — the confirmation MACs in
//!   [`ConfirmMacs`] are how we *detect* the mismatch and bail.
//!
//! * [`ConfirmMacs`] — HKDF-derived "I really do know K" tokens that bind
//!   the handshake to both peers' Ed25519 identities. They're the part of
//!   the protocol that turns SPAKE2's "we maybe both know the same K" into
//!   "we definitely both know K AND we mean these specific identities."
//!
//! The wire protocol that carries these messages lives in `transport`
//! (`pair_listen` / `pair_connect`).

use std::path::Path;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity as SpakeId, Password, Spake2};
use subtle::ConstantTimeEq;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PairingError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml parse: {0}")]
    TomlParse(#[from] toml::de::Error),
    #[error("toml serialize: {0}")]
    TomlSer(#[from] toml::ser::Error),
    #[error("hex decode: {0}")]
    Hex(#[from] hex::FromHexError),
    #[error("invalid ed25519 key")]
    BadKey,
    #[error("spake2: {0}")]
    Spake(String),
    #[error("PIN/identity mismatch (confirmation MAC didn't verify)")]
    ConfirmationFailed,
    #[error("session called in the wrong order: {0}")]
    WrongState(&'static str),
    #[error("host signature failed to verify (relay attack or wrong pubkey)")]
    VideoAuthFailed,
    #[error("client signature failed to verify (not the paired device)")]
    ClientAuthFailed,
    #[error("host pubkey doesn't match pinned: expected {expected}, got {actual}")]
    HostPubkeyMismatch { expected: String, actual: String },
}

// ---------- Identity ---------------------------------------------------

/// 32-byte secret seed for a long-term Ed25519 keypair. Persisted as the
/// only thing in `identity.toml`; the public key derives from it.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IdentityFile {
    secret: String, // hex-encoded 32-byte seed
}

/// A peer's long-term identity. Generated once, pinned by every other peer
/// that completes pairing with this one.
pub struct Identity {
    signing: SigningKey,
}

impl Identity {
    /// Fresh random keypair from the OS CSPRNG.
    pub fn generate() -> Self {
        let mut rng = rand_core::OsRng;
        Self {
            signing: SigningKey::generate(&mut rng),
        }
    }

    /// Load from disk if present, otherwise generate + save.
    pub fn load_or_create(path: &Path) -> Result<Self, PairingError> {
        if path.exists() {
            return Self::load(path);
        }
        let id = Self::generate();
        id.save(path)?;
        Ok(id)
    }

    pub fn load(path: &Path) -> Result<Self, PairingError> {
        let text = std::fs::read_to_string(path)?;
        let file: IdentityFile = toml::from_str(&text)?;
        let bytes = hex::decode(file.secret.trim())?;
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| PairingError::BadKey)?;
        Ok(Self {
            signing: SigningKey::from_bytes(&arr),
        })
    }

    pub fn save(&self, path: &Path) -> Result<(), PairingError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = IdentityFile {
            secret: hex::encode(self.signing.to_bytes()),
        };
        let text = toml::to_string_pretty(&file)?;
        // Restrict the file to 0600 on Unix — the secret is sensitive.
        write_secret(path, text.as_bytes())?;
        Ok(())
    }

    /// 32-byte public key bytes — what the peer pins.
    pub fn public_bytes(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing.verifying_key()
    }

    /// Hex display of the public key (helpful for fingerprinting).
    pub fn public_hex(&self) -> String {
        hex::encode(self.public_bytes())
    }
}

#[cfg(unix)]
fn write_secret(path: &Path, data: &[u8]) -> Result<(), PairingError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_secret(path: &Path, data: &[u8]) -> Result<(), PairingError> {
    std::fs::write(path, data).map_err(Into::into)
}

// ---------- Pairing handshake -----------------------------------------

/// Protocol version. Bumped if the wire format or HKDF context changes.
pub const PAIRING_PROTOCOL_VERSION: u32 = 1;

/// Which side of the handshake we're driving. SPAKE2 calls these "a" (the
/// initiator — i.e. the client/Trireme) and "b" (the responder — Pharos).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingRole {
    Client,
    Host,
}

/// 32-byte secret produced by SPAKE2 + HKDF. The MAC keys are derived from
/// it; the secret itself is also handed to higher layers (future: pinning,
/// session key for control channel).
#[derive(Clone)]
pub struct SharedSecret(pub [u8; 32]);

/// HKDF-derived confirmation tokens. Both sides compute these from the
/// shared secret + both pubkeys; they're how each side proves to the other
/// "I really know the same K, and I see you as exactly this pubkey."
#[derive(Clone)]
pub struct ConfirmMacs {
    pub client_mac: [u8; 32],
    pub host_mac: [u8; 32],
    pub session_key: [u8; 32],
}

impl ConfirmMacs {
    /// Derive from the SPAKE2 shared output + both peers' public keys.
    /// Identical inputs on both sides → identical outputs.
    pub fn derive(secret: &SharedSecret, client_pub: &[u8; 32], host_pub: &[u8; 32]) -> Self {
        // Bind the derivation to both identities so a man-in-the-middle who
        // swapped pubkeys mid-flight gets a different MAC.
        let mut salt = Vec::with_capacity(64);
        salt.extend_from_slice(client_pub);
        salt.extend_from_slice(host_pub);
        let hk = Hkdf::<Sha256>::new(Some(&salt), &secret.0);

        let mut client_mac = [0u8; 32];
        hk.expand(b"trireme-pair-v1 client-confirm", &mut client_mac)
            .expect("hkdf expand client_mac");
        let mut host_mac = [0u8; 32];
        hk.expand(b"trireme-pair-v1 host-confirm", &mut host_mac)
            .expect("hkdf expand host_mac");
        let mut session_key = [0u8; 32];
        hk.expand(b"trireme-pair-v1 session-key", &mut session_key)
            .expect("hkdf expand session_key");

        Self {
            client_mac,
            host_mac,
            session_key,
        }
    }
}

/// One side's view of an in-progress handshake. Designed to be driven by an
/// outer protocol shell that does the wire I/O (`transport::pair_*`).
///
/// ```text
///   Client                                 Host
///   ──────                                 ────
///   new(Client, pin)
///       (sets client_pub, peer set later)
///                          HelloPair(client_pub) ─────▶
///                                                 new(Host, pin)
///                                                    (sets host_pub)
///                                                 set_peer_client(client_pub)
///                                                 spake_msg_host = start()
///                          ◀───── HelloAck(host_pub, spake_msg_host)
///   set_peer_host(host_pub)
///   spake_msg_client = start()
///                          Spake2Reply(spake_msg_client) ─▶
///   K = finish(spake_msg_host)               K = finish(spake_msg_client)
///   confirm = MACs::derive(K, ...)           confirm = MACs::derive(K, ...)
///                          ClientConfirm(confirm.client_mac) ─▶
///                                                 verify(confirm.client_mac)
///                          ◀──── HostConfirm(confirm.host_mac)
///   verify(confirm.host_mac)
///   → paired
/// ```
pub struct PairingSession {
    role: PairingRole,
    pin: String,
    our_pub: [u8; 32],
    peer_pub: Option<[u8; 32]>,
    spake: Option<Spake2<Ed25519Group>>,
    secret: Option<SharedSecret>,
}

impl PairingSession {
    pub fn new(role: PairingRole, pin: String, our_pub: [u8; 32]) -> Self {
        Self {
            role,
            pin,
            our_pub,
            peer_pub: None,
            spake: None,
            secret: None,
        }
    }

    pub fn role(&self) -> PairingRole {
        self.role
    }

    pub fn our_pubkey(&self) -> [u8; 32] {
        self.our_pub
    }

    pub fn peer_pubkey(&self) -> Option<[u8; 32]> {
        self.peer_pub
    }

    /// Record the peer's long-term pubkey. Must be called before [`start`].
    pub fn set_peer_pubkey(&mut self, peer: [u8; 32]) {
        self.peer_pub = Some(peer);
    }

    /// Generate this side's SPAKE2 outbound message. Both sides include
    /// both pubkeys in the identities so a swap is detected.
    pub fn start(&mut self) -> Result<Vec<u8>, PairingError> {
        if self.spake.is_some() {
            return Err(PairingError::WrongState("start called twice"));
        }
        let peer = self
            .peer_pub
            .ok_or(PairingError::WrongState("peer pubkey not set"))?;
        let (client_pub, host_pub) = self.id_pair(peer);
        let id_client = SpakeId::new(&client_pub);
        let id_host = SpakeId::new(&host_pub);
        let password = Password::new(self.pin.as_bytes());
        let (state, msg) = match self.role {
            PairingRole::Client => Spake2::<Ed25519Group>::start_a(&password, &id_client, &id_host),
            PairingRole::Host => Spake2::<Ed25519Group>::start_b(&password, &id_client, &id_host),
        };
        self.spake = Some(state);
        Ok(msg)
    }

    /// Compute the shared secret by feeding in the peer's outbound message.
    /// Both sides will yield the same secret iff the PINs matched.
    pub fn finish(&mut self, peer_msg: &[u8]) -> Result<SharedSecret, PairingError> {
        let spake = self
            .spake
            .take()
            .ok_or(PairingError::WrongState("finish before start"))?;
        let k = spake
            .finish(peer_msg)
            .map_err(|e| PairingError::Spake(format!("{e:?}")))?;
        // SPAKE2 returns a Vec<u8> of length ≥ 32 (Ed25519 group hash output).
        let mut secret = [0u8; 32];
        let take = k.len().min(32);
        secret[..take].copy_from_slice(&k[..take]);
        let s = SharedSecret(secret);
        self.secret = Some(s.clone());
        Ok(s)
    }

    /// Derive the confirmation MACs + session key. Stable after `finish`.
    pub fn confirm_macs(&self) -> Result<ConfirmMacs, PairingError> {
        let secret = self
            .secret
            .as_ref()
            .ok_or(PairingError::WrongState("confirm before finish"))?;
        let peer = self
            .peer_pub
            .ok_or(PairingError::WrongState("peer pubkey lost"))?;
        let (c, h) = self.id_pair(peer);
        Ok(ConfirmMacs::derive(secret, &c, &h))
    }

    /// Verify the peer-supplied MAC matches the one we computed. Constant-
    /// time comparison.
    pub fn verify_peer_mac(&self, peer_mac: &[u8; 32]) -> Result<(), PairingError> {
        let macs = self.confirm_macs()?;
        let ours = match self.role {
            PairingRole::Client => macs.host_mac,
            PairingRole::Host => macs.client_mac,
        };
        if ours.ct_eq(peer_mac).into() {
            Ok(())
        } else {
            Err(PairingError::ConfirmationFailed)
        }
    }

    /// The MAC *we* should send to the peer.
    pub fn our_mac(&self) -> Result<[u8; 32], PairingError> {
        let macs = self.confirm_macs()?;
        Ok(match self.role {
            PairingRole::Client => macs.client_mac,
            PairingRole::Host => macs.host_mac,
        })
    }

    /// Sort our_pub and peer_pub into (client_pub, host_pub) regardless of
    /// the local role, so both sides build the same SPAKE2 identity strings.
    fn id_pair(&self, peer: [u8; 32]) -> ([u8; 32], [u8; 32]) {
        match self.role {
            PairingRole::Client => (self.our_pub, peer),
            PairingRole::Host => (peer, self.our_pub),
        }
    }
}

// ---------- Per-connection video auth -------------------------------------
//
// After pairing, the client knows the host's long-term Ed25519 pubkey. At
// the start of every video session we run a short challenge-response: the
// client sends a 32-byte nonce, the host signs `(label || nonce || tls_exporter)`
// with its identity, the client verifies. Binding the signed payload to the
// TLS exporter neutralizes a relay-style MITM — the exporter values on the
// two TLS legs differ, so a forwarded signature won't verify on the client
// side. Domain-separation via the label keeps this signature from being
// usable elsewhere.

/// Domain-separation label for the video auth challenge.
const VIDEO_AUTH_LABEL: &[u8] = b"trireme-video-auth-v1";

fn video_auth_payload(nonce: &[u8; 32], exporter: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(VIDEO_AUTH_LABEL);
    h.update([0x00]); // separator
    h.update(nonce);
    h.update(exporter);
    let out = h.finalize();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    arr
}

/// Host side: sign the auth challenge with the long-term identity.
pub fn sign_video_auth(identity: &Identity, nonce: &[u8; 32], exporter: &[u8; 32]) -> [u8; 64] {
    let payload = video_auth_payload(nonce, exporter);
    let sig: Signature = identity.signing.sign(&payload);
    sig.to_bytes()
}

/// Client side: verify the host's signature against the *expected* pubkey
/// and reject mismatches. `host_pub_claimed` is whatever pubkey the host
/// actually sent us, and `expected` (when `Some`) is the one we pinned at
/// pairing time. The function fails fast on either a bad signature or a
/// pubkey mismatch — see [`PairingError::VideoAuthFailed`] /
/// [`PairingError::HostPubkeyMismatch`].
pub fn verify_video_auth(
    nonce: &[u8; 32],
    exporter: &[u8; 32],
    sig_bytes: &[u8; 64],
    host_pub_claimed: &[u8; 32],
    expected: Option<&[u8; 32]>,
) -> Result<(), PairingError> {
    if let Some(want) = expected {
        if want.ct_eq(host_pub_claimed).unwrap_u8() == 0 {
            return Err(PairingError::HostPubkeyMismatch {
                expected: hex::encode(want),
                actual: hex::encode(host_pub_claimed),
            });
        }
    }
    let payload = video_auth_payload(nonce, exporter);
    let vk = VerifyingKey::from_bytes(host_pub_claimed).map_err(|_| PairingError::BadKey)?;
    let sig = Signature::from_bytes(sig_bytes);
    vk.verify(&payload, &sig)
        .map_err(|_| PairingError::VideoAuthFailed)
}

// ---------- Per-connection client auth ------------------------------------
//
// The mirror image of the video auth above, in the other direction: it lets
// the *host* authenticate the *client*. After the host has signed its own
// challenge, it sends the client a fresh nonce; the client signs
// `(label || host_nonce || tls_exporter)` with its long-term identity. The
// host verifies that signature against the pubkey the client claims, then
// separately *authorizes* that pubkey (is it in peers.toml?). Binding to the
// exporter defeats the same relay-MITM as the host direction. Trust is in the
// Ed25519 identities, never in the (throwaway, opaque) TLS cert.

/// Domain-separation label for the client→host auth challenge.
const CLIENT_AUTH_LABEL: &[u8] = b"trireme-client-auth-v1";

fn client_auth_payload(host_nonce: &[u8; 32], exporter: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(CLIENT_AUTH_LABEL);
    h.update([0x00]); // separator
    h.update(host_nonce);
    h.update(exporter);
    let out = h.finalize();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    arr
}

/// Client side: prove possession of the long-term identity by signing the
/// host's challenge nonce, bound to the TLS exporter.
pub fn sign_client_auth(
    identity: &Identity,
    host_nonce: &[u8; 32],
    exporter: &[u8; 32],
) -> [u8; 64] {
    let payload = client_auth_payload(host_nonce, exporter);
    let sig: Signature = identity.signing.sign(&payload);
    sig.to_bytes()
}

/// Host side: verify the client's signature against the pubkey it claims.
/// This proves the client holds the private key for `client_pub`; *whether*
/// that pubkey is allowed (paired) is a separate, caller-side authorization
/// check, deliberately kept out of the crypto layer.
pub fn verify_client_auth(
    host_nonce: &[u8; 32],
    exporter: &[u8; 32],
    sig_bytes: &[u8; 64],
    client_pub: &[u8; 32],
) -> Result<(), PairingError> {
    let payload = client_auth_payload(host_nonce, exporter);
    let vk = VerifyingKey::from_bytes(client_pub).map_err(|_| PairingError::BadKey)?;
    let sig = Signature::from_bytes(sig_bytes);
    vk.verify(&payload, &sig)
        .map_err(|_| PairingError::ClientAuthFailed)
}

/// Strictly-used HMAC-SHA256 helper, exposed so the wire protocol can sign
/// the post-pairing session establishment without re-deriving HKDF.
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut h = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("hmac accepts any key length");
    h.update(msg);
    let out = h.finalize().into_bytes();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    arr
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_handshake(
        client_pin: &str,
        host_pin: &str,
    ) -> Result<(ConfirmMacs, ConfirmMacs), PairingError> {
        let client_id = Identity::generate();
        let host_id = Identity::generate();

        let mut client = PairingSession::new(
            PairingRole::Client,
            client_pin.to_string(),
            client_id.public_bytes(),
        );
        let mut host = PairingSession::new(
            PairingRole::Host,
            host_pin.to_string(),
            host_id.public_bytes(),
        );

        // Identity exchange.
        client.set_peer_pubkey(host_id.public_bytes());
        host.set_peer_pubkey(client_id.public_bytes());

        // Each side computes its outbound SPAKE2 message.
        let msg_client = client.start()?;
        let msg_host = host.start()?;

        // Exchange.
        let _ = client.finish(&msg_host)?;
        let _ = host.finish(&msg_client)?;

        // Confirmation MACs.
        let macs_client = client.confirm_macs()?;
        let macs_host = host.confirm_macs()?;
        Ok((macs_client, macs_host))
    }

    #[test]
    fn round_trip_matching_pin_yields_equal_macs() {
        let (a, b) = run_handshake("424242", "424242").unwrap();
        assert_eq!(a.client_mac, b.client_mac);
        assert_eq!(a.host_mac, b.host_mac);
        assert_eq!(a.session_key, b.session_key);
    }

    #[test]
    fn mismatched_pin_yields_different_macs() {
        let (a, b) = run_handshake("424242", "424243").unwrap();
        // The whole point: a mismatched PIN must produce diverging derived
        // material. The verify step then catches it.
        assert_ne!(a.client_mac, b.client_mac);
        assert_ne!(a.host_mac, b.host_mac);
        assert_ne!(a.session_key, b.session_key);
    }

    #[test]
    fn verify_peer_mac_accepts_matching_pin() {
        let client_id = Identity::generate();
        let host_id = Identity::generate();
        let mut client = PairingSession::new(
            PairingRole::Client,
            "424242".into(),
            client_id.public_bytes(),
        );
        let mut host =
            PairingSession::new(PairingRole::Host, "424242".into(), host_id.public_bytes());
        client.set_peer_pubkey(host_id.public_bytes());
        host.set_peer_pubkey(client_id.public_bytes());
        let mc = client.start().unwrap();
        let mh = host.start().unwrap();
        client.finish(&mh).unwrap();
        host.finish(&mc).unwrap();

        // Each side sends its MAC; each side verifies the peer's.
        let client_mac = client.our_mac().unwrap();
        let host_mac = host.our_mac().unwrap();
        host.verify_peer_mac(&client_mac)
            .expect("host verifies client");
        client
            .verify_peer_mac(&host_mac)
            .expect("client verifies host");
    }

    #[test]
    fn verify_peer_mac_rejects_mismatched_pin() {
        let client_id = Identity::generate();
        let host_id = Identity::generate();
        let mut client = PairingSession::new(
            PairingRole::Client,
            "111111".into(),
            client_id.public_bytes(),
        );
        let mut host =
            PairingSession::new(PairingRole::Host, "999999".into(), host_id.public_bytes());
        client.set_peer_pubkey(host_id.public_bytes());
        host.set_peer_pubkey(client_id.public_bytes());
        let mc = client.start().unwrap();
        let mh = host.start().unwrap();
        client.finish(&mh).unwrap();
        host.finish(&mc).unwrap();

        let client_mac = client.our_mac().unwrap();
        let host_mac = host.our_mac().unwrap();
        assert!(matches!(
            host.verify_peer_mac(&client_mac),
            Err(PairingError::ConfirmationFailed)
        ));
        assert!(matches!(
            client.verify_peer_mac(&host_mac),
            Err(PairingError::ConfirmationFailed)
        ));
    }

    #[test]
    fn video_auth_round_trip() {
        let host = Identity::generate();
        let nonce = [7u8; 32];
        let exporter = [42u8; 32];
        let sig = sign_video_auth(&host, &nonce, &exporter);
        let host_pub = host.public_bytes();
        verify_video_auth(&nonce, &exporter, &sig, &host_pub, Some(&host_pub))
            .expect("valid signature verifies");
    }

    #[test]
    fn video_auth_rejects_wrong_pubkey_pin() {
        let host = Identity::generate();
        let nonce = [1u8; 32];
        let exporter = [2u8; 32];
        let sig = sign_video_auth(&host, &nonce, &exporter);
        let wrong = Identity::generate().public_bytes();
        // Mismatch caught BEFORE the signature is checked.
        let err = verify_video_auth(&nonce, &exporter, &sig, &host.public_bytes(), Some(&wrong))
            .expect_err("must reject");
        assert!(matches!(err, PairingError::HostPubkeyMismatch { .. }));
    }

    #[test]
    fn video_auth_rejects_wrong_exporter() {
        let host = Identity::generate();
        let nonce = [9u8; 32];
        let sig = sign_video_auth(&host, &nonce, &[1u8; 32]);
        // Client computes a different exporter (relay attack); sig won't verify.
        let host_pub = host.public_bytes();
        let err = verify_video_auth(&nonce, &[2u8; 32], &sig, &host_pub, Some(&host_pub))
            .expect_err("relay attack must fail");
        assert!(matches!(err, PairingError::VideoAuthFailed));
    }

    #[test]
    fn video_auth_rejects_wrong_nonce() {
        let host = Identity::generate();
        let exporter = [3u8; 32];
        let sig = sign_video_auth(&host, &[1u8; 32], &exporter);
        let host_pub = host.public_bytes();
        let err = verify_video_auth(&[2u8; 32], &exporter, &sig, &host_pub, Some(&host_pub))
            .expect_err("replay must fail");
        assert!(matches!(err, PairingError::VideoAuthFailed));
    }

    #[test]
    fn video_auth_tofu_when_no_pin() {
        let host = Identity::generate();
        let nonce = [0u8; 32];
        let exporter = [0u8; 32];
        let sig = sign_video_auth(&host, &nonce, &exporter);
        // expected = None → trust on first use; still requires a valid signature.
        verify_video_auth(&nonce, &exporter, &sig, &host.public_bytes(), None)
            .expect("TOFU still verifies sig");
    }

    #[test]
    fn client_auth_round_trip() {
        let client = Identity::generate();
        let nonce = [5u8; 32];
        let exporter = [9u8; 32];
        let sig = sign_client_auth(&client, &nonce, &exporter);
        verify_client_auth(&nonce, &exporter, &sig, &client.public_bytes())
            .expect("client signature verifies");
    }

    #[test]
    fn client_auth_rejects_wrong_key() {
        let client = Identity::generate();
        let nonce = [1u8; 32];
        let exporter = [2u8; 32];
        let sig = sign_client_auth(&client, &nonce, &exporter);
        let other = Identity::generate().public_bytes();
        let err = verify_client_auth(&nonce, &exporter, &sig, &other).expect_err("must reject");
        assert!(matches!(err, PairingError::ClientAuthFailed));
    }

    #[test]
    fn client_auth_rejects_wrong_exporter() {
        let client = Identity::generate();
        // Client signed over exporter [1;32]; host (relay) checks [2;32].
        let sig = sign_client_auth(&client, &[1u8; 32], &[1u8; 32]);
        let err = verify_client_auth(&[1u8; 32], &[2u8; 32], &sig, &client.public_bytes())
            .expect_err("relay attack must fail");
        assert!(matches!(err, PairingError::ClientAuthFailed));
    }

    #[test]
    fn identity_persistence_round_trips() {
        let tmp = std::env::temp_dir().join("core-crypto-identity-test.toml");
        let _ = std::fs::remove_file(&tmp);
        let a = Identity::load_or_create(&tmp).unwrap();
        let b = Identity::load_or_create(&tmp).unwrap();
        assert_eq!(a.public_bytes(), b.public_bytes(), "key reused");
        std::fs::remove_file(&tmp).ok();
    }
}
