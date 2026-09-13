//! Cryptography for LionFS-cluster (spec §15), with the audit's C2/C6 fixes.
//!
//! **Key scoping (audit C2 fix).** Spec §15 derives *per-file* keys from a
//! per-directory master; spec §6/§10 dedup on plaintext hashes *cluster-wide*.
//! Those two designs contradict each other: a chunk shared by two files is
//! stored once, encrypted under *whose* key? This crate resolves it the only
//! consistent way: keys are scoped to a **dedup domain** (tenant), and the
//! dedup index is domain-scoped too. Within a domain, extent keys are
//! *convergent* — derived from the domain key and the content hash — so
//! identical plaintext chunks encrypt identically and dedup works. The
//! equality side channel this creates is explicit and documented: cross-tenant
//! dedup is impossible by construction (audit C6).
//!
//! Layout of the key tree:
//! ```text
//! master (HSM/TPM-sealed)
//!   └── HKDF(master, "hfs/domain/" + domain_id) → domain key
//!         └── HKDF(domain, "hfs/ext/" + content_hash) → extent key
//!               └── AEAD nonce = first 96 bits of BLAKE3(domain || content_hash)
//! ```
//! Nonce derivation from (domain, content) is collision-safe *within* the
//! design because equal content ⇒ equal key: the only repeated (key, nonce)
//! pair encrypts identical plaintext — the one case AES-GCM tolerates.
//!
//! **Macaroons (spec §15).** Capability tokens with caveats chained by
//! HMAC; verification re-derives the chain and checks every attenuating
//! caveat (path prefix, expiry, operation set).

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use chacha20poly1305::ChaCha20Poly1305;
use crate::core::Hash256;
use hkdf::Hkdf;
use hmac::{Mac, SimpleHmac};
use rand::RngCore;
use sha2::Sha256;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("encryption failed: {0}")]
    Encryption(String),
    #[error("decryption failed: {0}")]
    Decryption(String),
    #[error("macaroon rejected: {0}")]
    MacaroonRejected(String),
}

type HmacSha256 = SimpleHmac<Sha256>;

const DOMAIN_INFO_PREFIX: &[u8] = b"hfs/domain/";
const EXTENT_INFO_PREFIX: &[u8] = b"hfs/ext/";

/// Cipher selector (spec §15: AES-256-GCM, or ChaCha20-Poly1305 on
/// non-AESNI hardware).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Cipher {
    Aes256Gcm,
    ChaCha20Poly1305,
}

/// A domain-scoped key tree rooted at a master key.
pub struct KeyTree {
    master: [u8; 32],
}

impl KeyTree {
    /// Seal a new random master key.
    pub fn generate() -> Self {
        let mut master = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut master);
        Self { master }
    }

    pub fn from_master(master: [u8; 32]) -> Self {
        Self { master }
    }

    pub fn master(&self) -> &[u8; 32] {
        &self.master
    }

    /// Derive the domain (tenant) key.
    pub fn domain_key(&self, domain_id: &[u8]) -> [u8; 32] {
        let mut info = Vec::with_capacity(DOMAIN_INFO_PREFIX.len() + domain_id.len());
        info.extend_from_slice(DOMAIN_INFO_PREFIX);
        info.extend_from_slice(domain_id);
        let mut out = [0u8; 32];
        let hk: Hkdf<Sha256> = Hkdf::new(None, &self.master);
        hk.expand(&info, &mut out).expect("hkdf expand 32 bytes");
        out
    }

    /// Derive the convergent extent key for (domain, content).
    fn extent_key(domain_key: &[u8; 32], content_hash: &Hash256) -> ([u8; 32], [u8; 12]) {
        let mut info = Vec::with_capacity(EXTENT_INFO_PREFIX.len() + 32);
        info.extend_from_slice(EXTENT_INFO_PREFIX);
        info.extend_from_slice(content_hash.as_bytes());
        let mut key = [0u8; 32];
        let hk: Hkdf<Sha256> = Hkdf::new(None, domain_key);
        hk.expand(&info, &mut key).expect("hkdf expand 32 bytes");

        // Nonce = 96 bits of BLAKE3(domain_key || content_hash): stable for
        // identical (domain, content), unique across different content.
        let mut nonce_input = Vec::with_capacity(64);
        nonce_input.extend_from_slice(domain_key);
        nonce_input.extend_from_slice(content_hash.as_bytes());
        let h = Hash256::of(&nonce_input);
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&h.as_bytes()[..12]);
        (key, nonce)
    }

    /// Encrypt a payload for a given domain + content hash (convergent).
    pub fn encrypt_extent(
        &self,
        domain_key: &[u8; 32],
        content_hash: &Hash256,
        plaintext: &[u8],
        cipher: Cipher,
    ) -> Result<Vec<u8>, CryptoError> {
        let (key, nonce) = Self::extent_key(domain_key, content_hash);
        let payload = Payload { msg: plaintext, aad: content_hash.as_bytes() };
        let ct = match cipher {
            Cipher::Aes256Gcm => Aes256Gcm::new((&key).into())
                .encrypt(Nonce::from_slice(&nonce), payload)
                .map_err(|e| CryptoError::Encryption(format!("aes-gcm: {e:?}")))?,
            Cipher::ChaCha20Poly1305 => ChaCha20Poly1305::new((&key).into())
                .encrypt(Nonce::from_slice(&nonce), payload)
                .map_err(|e| CryptoError::Encryption(format!("chacha20: {e:?}")))?,
        };
        Ok(ct)
    }

    /// Decrypt a convergent extent.
    pub fn decrypt_extent(
        &self,
        domain_key: &[u8; 32],
        content_hash: &Hash256,
        ciphertext: &[u8],
        cipher: Cipher,
    ) -> Result<Vec<u8>, CryptoError> {
        let (key, nonce) = Self::extent_key(domain_key, content_hash);
        let payload = Payload { msg: ciphertext, aad: content_hash.as_bytes() };
        let pt = match cipher {
            Cipher::Aes256Gcm => Aes256Gcm::new((&key).into())
                .decrypt(Nonce::from_slice(&nonce), payload)
                .map_err(|e| CryptoError::Decryption(format!("aes-gcm: {e:?}")))?,
            Cipher::ChaCha20Poly1305 => ChaCha20Poly1305::new((&key).into())
                .decrypt(Nonce::from_slice(&nonce), payload)
                .map_err(|e| CryptoError::Decryption(format!("chacha20: {e:?}")))?,
        };
        Ok(pt)
    }
}

// ---------------------------------------------------------------------------
// Macaroons
// ---------------------------------------------------------------------------

/// One attenuating caveat on a [`Macaroon`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Caveat {
    /// Token may only touch paths under this prefix.
    PathPrefix(String),
    /// Token expires at this UNIX timestamp (seconds).
    ExpiresAt(u64),
    /// Token allows exactly these operations (subset of read/write/admin).
    Operations(Vec<Op>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Read,
    Write,
    Admin,
}

impl Op {
    pub fn as_str(&self) -> &'static str {
        match self {
            Op::Read => "read",
            Op::Write => "write",
            Op::Admin => "admin",
        }
    }
}

/// A Macaroon-style capability token (spec §15):
///
/// ```text
/// Macaroon {
///   identifier: nonce,
///   location: helixfs://cluster/path,
///   caveats: [path ⊆ /home/alice/*, expires < t+1h, ops ⊆ {read}],
///   signature: HMAC_chain(caveats, root_key)
/// }
/// ```
///
/// `sig_0 = HMAC(root, identifier)`; `sig_i = HMAC(sig_{i-1}, caveat_i)`.
/// Verification recomputes the chain and then applies the caveats to the
/// request context.
#[derive(Clone, Debug)]
pub struct Macaroon {
    pub identifier: String,
    pub location: String,
    pub caveats: Vec<Caveat>,
    signature: [u8; 32],
}

impl Macaroon {
    /// Mint a token bound to `root_key` with no caveats (full power).
    pub fn mint(root_key: &[u8], identifier: &str, location: &str) -> Self {
        let mut mac = Self {
            identifier: identifier.to_string(),
            location: location.to_string(),
            caveats: Vec::new(),
            signature: [0u8; 32],
        };
        mac.signature = mac.chain_hmac(root_key, &[]);
        mac
    }

    /// Return a *new* token with one more caveat (attenuation never widens
    /// the original — it derives a fresh, strictly-weaker token).
    pub fn add_caveat(&self, root_key: &[u8], caveat: Caveat) -> Self {
        let mut mac = self.clone();
        mac.caveats.push(caveat);
        mac.signature = mac.chain_hmac(root_key, &mac.serialize_caveats());
        mac
    }

    fn serialize_caveats(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for c in &self.caveats {
            match c {
                Caveat::PathPrefix(p) => {
                    out.extend_from_slice(b"path:");
                    out.extend_from_slice(p.as_bytes());
                }
                Caveat::ExpiresAt(t) => out.extend_from_slice(format!("exp:{t}").as_bytes()),
                Caveat::Operations(ops) => {
                    out.extend_from_slice(b"ops:");
                    for op in ops {
                        out.extend_from_slice(op.as_str().as_bytes());
                        out.push(b',');
                    }
                }
            }
            out.push(b'\n');
        }
        out
    }

    /// sig_0 = HMAC(root, identifier); sig_i = HMAC(sig_{i-1}, caveat_i).
    fn chain_hmac(&self, root_key: &[u8], _caveat_bytes: &[u8]) -> [u8; 32] {
        let mut mac = <HmacSha256 as Mac>::new_from_slice(root_key).expect("hmac key");
        mac.update(self.identifier.as_bytes());
        let mut sig: [u8; 32] = mac.finalize().into_bytes().into();
        for c in &self.caveats {
            let mut m = <HmacSha256 as Mac>::new_from_slice(&sig).expect("hmac key");
            m.update(&serialize_caveat(c));
            sig = m.finalize().into_bytes().into();
        }
        sig
    }

    /// Verify signature and all caveats against the request context.
    pub fn verify(
        &self,
        root_key: &[u8],
        path: &str,
        op: Op,
        now_unix: u64,
    ) -> Result<(), CryptoError> {
        let expect = self.chain_hmac(root_key, &[]);
        if expect != self.signature {
            return Err(CryptoError::MacaroonRejected("bad signature".into()));
        }
        for c in &self.caveats {
            match c {
                Caveat::PathPrefix(prefix) => {
                    if !path.starts_with(prefix.as_str()) {
                        return Err(CryptoError::MacaroonRejected(format!(
                            "path {path} outside prefix {prefix}"
                        )));
                    }
                }
                Caveat::ExpiresAt(t) => {
                    if now_unix >= *t {
                        return Err(CryptoError::MacaroonRejected("token expired".into()));
                    }
                }
                Caveat::Operations(allowed) => {
                    if !allowed.contains(&op) {
                        return Err(CryptoError::MacaroonRejected(format!(
                            "op {} not allowed",
                            op.as_str()
                        )));
                    }
                }
            }
        }
        Ok(())
    }
}

fn serialize_caveat(c: &Caveat) -> Vec<u8> {
    let mut out = Vec::new();
    match c {
        Caveat::PathPrefix(p) => {
            out.extend_from_slice(b"path:");
            out.extend_from_slice(p.as_bytes());
        }
        Caveat::ExpiresAt(t) => out.extend_from_slice(format!("exp:{t}").as_bytes()),
        Caveat::Operations(ops) => {
            out.extend_from_slice(b"ops:");
            for op in ops {
                out.extend_from_slice(op.as_str().as_bytes());
                out.push(b',');
            }
        }
    }
    out
}

/// Signature scheme slot (spec §15 post-quantum readiness): default
/// Ed25519-shaped API with a pluggable backend. The prototype ships a
/// deterministic HMAC-SHA256 "stand-in" signature so the VersionNode
/// signing path is exercised end-to-end; swapping in ed25519-dalek or
/// Dilithium is a drop-in `impl`.
pub trait SignatureScheme {
    fn name(&self) -> &'static str;
    fn keypair_from_seed(&self, seed: &[u8]) -> (Vec<u8>, Vec<u8>);
    fn sign(&self, secret: &[u8], message: &[u8]) -> Vec<u8>;
    fn verify(&self, public: &[u8], message: &[u8], signature: &[u8]) -> bool;
}

/// HMAC-SHA256 stand-in (NOT production post-quantum — see docs).
pub struct HmacSignature;

impl SignatureScheme for HmacSignature {
    fn name(&self) -> &'static str {
        "hmac-sha256-standin"
    }

    fn keypair_from_seed(&self, seed: &[u8]) -> (Vec<u8>, Vec<u8>) {
        // Stand-in ONLY: "public" equals the secret, so verify can re-derive
        // the tag. This is NOT a signature scheme — it exists to exercise the
        // API shape; production swaps in Ed25519/Dilithium unchanged.
        (seed.to_vec(), seed.to_vec())
    }

    fn sign(&self, secret: &[u8], message: &[u8]) -> Vec<u8> {
        let mut mac = <HmacSha256 as Mac>::new_from_slice(secret).expect("hmac key");
        mac.update(message);
        mac.finalize().into_bytes().to_vec()
    }

    fn verify(&self, public: &[u8], message: &[u8], signature: &[u8]) -> bool {
        // Stand-in: re-derive secret-side check via keyed hash of (public||msg).
        // A real scheme verifies against the public key algebraically.
        let mut mac = <HmacSha256 as Mac>::new_from_slice(public).expect("hmac key");
        mac.update(message);
        let expect: Vec<u8> = mac.finalize().into_bytes().to_vec();
        expect == signature
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content(data: &[u8]) -> Hash256 {
        Hash256::of(data)
    }

    #[test]
    fn convergent_encryption_roundtrip_both_ciphers() {
        let tree = KeyTree::generate();
        let domain = tree.domain_key(b"tenant-a");
        let h = content(b"hello world");
        for cipher in [Cipher::Aes256Gcm, Cipher::ChaCha20Poly1305] {
            let ct = tree.encrypt_extent(&domain, &h, b"hello world", cipher).unwrap();
            let pt = tree.decrypt_extent(&domain, &h, &ct, cipher).unwrap();
            assert_eq!(pt, b"hello world");
        }
    }

    #[test]
    fn identical_content_encrypts_identically_within_domain() {
        // Convergent property that makes dedup possible (audit C2 fix).
        let tree = KeyTree::generate();
        let domain = tree.domain_key(b"tenant-a");
        let h = content(b"same bytes");
        let ct1 = tree.encrypt_extent(&domain, &h, b"same bytes", Cipher::Aes256Gcm).unwrap();
        let ct2 = tree.encrypt_extent(&domain, &h, b"same bytes", Cipher::Aes256Gcm).unwrap();
        assert_eq!(ct1, ct2);
    }

    #[test]
    fn cross_domain_dedup_is_impossible() {
        // Audit C6: ciphertexts from different tenants never match, so the
        // global dedup index cannot leak cross-tenant equality — and the
        // plaintext-hash index is domain-scoped by construction.
        let tree = KeyTree::generate();
        let dom_a = tree.domain_key(b"tenant-a");
        let dom_b = tree.domain_key(b"tenant-b");
        let h = content(b"shared backup chunk");
        let ct_a = tree.encrypt_extent(&dom_a, &h, b"shared backup chunk", Cipher::Aes256Gcm).unwrap();
        let ct_b = tree.encrypt_extent(&dom_b, &h, b"shared backup chunk", Cipher::Aes256Gcm).unwrap();
        assert_ne!(ct_a, ct_b);
        // And each side can only decrypt its own.
        assert!(tree.decrypt_extent(&dom_b, &h, &ct_a, Cipher::Aes256Gcm).is_err());
    }

    #[test]
    fn aad_binds_content_hash() {
        let tree = KeyTree::generate();
        let domain = tree.domain_key(b"t");
        let h1 = content(b"data v1");
        let h2 = content(b"data v2");
        let ct = tree.encrypt_extent(&domain, &h1, b"data v1", Cipher::Aes256Gcm).unwrap();
        // Presenting the wrong content hash must fail (AAD mismatch).
        assert!(tree.decrypt_extent(&domain, &h2, &ct, Cipher::Aes256Gcm).is_err());
    }

    #[test]
    fn macaroon_attenuation_and_verification() {
        let root = [7u8; 32];
        let base = Macaroon::mint(&root, "tok-1", "helixfs://cluster/home/alice");

        // Full-power token: any path, any op.
        assert!(base.verify(&root, "/home/alice/notes.txt", Op::Write, 1000).is_ok());
        assert!(base.verify(&root, "/etc/passwd", Op::Admin, 1000).is_ok());

        // Attenuate to read-only under /home/alice/, expiring at t=2000.
        let scoped = base
            .add_caveat(&root, Caveat::PathPrefix("/home/alice/".into()))
            .add_caveat(&root, Caveat::Operations(vec![Op::Read]))
            .add_caveat(&root, Caveat::ExpiresAt(2000));

        assert!(scoped.verify(&root, "/home/alice/notes.txt", Op::Read, 1500).is_ok());
        assert!(scoped.verify(&root, "/home/alice/notes.txt", Op::Write, 1500).is_err());
        assert!(scoped.verify(&root, "/home/bob/secret", Op::Read, 1500).is_err());
        assert!(scoped.verify(&root, "/home/alice/notes.txt", Op::Read, 2500).is_err());

        // Tampering with caveats breaks the chain.
        let mut forged = scoped.clone();
        forged.caveats.clear();
        assert!(forged.verify(&root, "/etc/passwd", Op::Admin, 1000).is_err());

        // Wrong root key fails.
        let other = [8u8; 32];
        assert!(scoped.verify(&other, "/home/alice/notes.txt", Op::Read, 1500).is_err());
    }

    #[test]
    fn signature_scheme_slot_works() {
        let scheme = HmacSignature;
        let (secret, public) = scheme.keypair_from_seed(b"seed-material");
        let sig = scheme.sign(&secret, b"version-node-bytes");
        assert!(scheme.verify(&public, b"version-node-bytes", &sig));
        assert!(!scheme.verify(&public, b"tampered-node-bytes", &sig));
    }

    #[test]
    fn domain_key_derivation_is_deterministic_and_separated() {
        let tree = KeyTree::from_master([9u8; 32]);
        assert_eq!(tree.domain_key(b"tenant-a"), tree.domain_key(b"tenant-a"));
        assert_ne!(tree.domain_key(b"tenant-a"), tree.domain_key(b"tenant-b"));
    }
}
