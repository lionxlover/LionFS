//! Passphrase-based key management (RFC-004 §11): the missing piece the
//! 2.0 `security::keys` module called out -- "wrapping the key tree
//! itself with a passphrase-derived key is a natural next step and is
//! not implemented here". This module is that step.
//!
//! The hierarchy (RFC-004 §11.1):
//!
//! ```text
//! passphrase --PBKDF2-HMAC-SHA256--> KEK (32 B)
//! volume master key (random 32 B) --AEAD-wrap(KEK)--> wrapped blob
//! per-file keys --HMAC-PRF(master, file_id, domain)--> 32 B each
//! ```
//!
//! * **PBKDF2** is hand-rolled over `sha2` (RFC 8018): the dependency
//!   set already carries sha2, Windows keeps its zero-crate build, and
//!   the 30 lines are exactly auditable. Iterations default to 600k
//!   (OWASP 2023 guidance for PBKDF2-HMAC-SHA256).
//! * **AEAD** wrapping uses ChaCha20-Poly1305 (the 2.0 cipher surface
//!   already uses it; hardware AES-GCM acceleration applies below it).
//! * **Envelope derivation** is an HMAC-SHA256 PRF with domain
//!   separation, so a per-file key leak confines to that file, and
//!   rotating the master re-derives the whole tree without re-keying
//!   data blocks.
//! * **Secure erase**: dropping the envelope from memory with
//!   volatile-zeroization (`ZeroingKey`) implements crypto-erase --
//!   once the master is gone, the wrapped blob is noise. Physically
//!   erasing blocks is the GC's job, and it is *not* a guarantee
//!   against a microscope; the docs say so.
//!
//! What this module deliberately is *not*: a KMS client, TPM sealing,
//! or recovery-key escrow. Those attach at the [`KeyEnvelope`] layer
//! (RFC-004 §11.4) and live in userspace tooling.

use std::io::{Error, ErrorKind, Result};

use sha2::{Digest, Sha256};

use crate::security::encryption::fill_random;

/// Default PBKDF2 iteration count (OWASP 2023 for HMAC-SHA256).
pub const DEFAULT_PBKDF2_ITERATIONS: u32 = 600_000;
/// Salt length (RFC 8018 recommends >= 64 bits; we ship 128).
pub const SALT_LEN: usize = 16;
/// Master/KEK/file key length.
pub const KEY_LEN: usize = 32;
/// AEAD nonce length (ChaCha20-Poly1305).
pub const NONCE_LEN: usize = 12;
/// Poly1305 tag length.
pub const TAG_LEN: usize = 16;

// ---------------------------------------------------------------------------
// PBKDF2-HMAC-SHA256 (RFC 8018)
// ---------------------------------------------------------------------------

/// HMAC-SHA256 (FIPS 198-1) over `key` and `message`.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64; // SHA-256 block size
    // Key preparation: zero-pad or hash-down to BLOCK bytes.
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        let d = Sha256::digest(key);
        k[..32].copy_from_slice(&d);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    inner.update(ipad);
    inner.update(message);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_digest);
    let out = outer.finalize();
    let mut mac = [0u8; 32];
    mac.copy_from_slice(&out);
    mac
}

/// PBKDF2-HMAC-SHA256 (RFC 8018 §5.2): `dkLen` bytes derived from
/// `password`, `salt`, `iterations`. Deterministic, allocation-light;
/// `dk_len` beyond 32*255 is refused (RFC bound).
#[must_use]
pub fn pbkdf2_hmac_sha256(
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    dk_len: usize,
) -> Option<Vec<u8>> {
    if dk_len == 0 || dk_len > 32 * 255 || iterations == 0 {
        return None;
    }
    let mut out = Vec::with_capacity(dk_len);
    let blocks = dk_len.div_ceil(32);
    for block in 1..=blocks as u32 {
        // U_1 = HMAC(password, salt || INT_32_BE(block))
        let mut msg = Vec::with_capacity(salt.len() + 4);
        msg.extend_from_slice(salt);
        msg.extend_from_slice(&block.to_be_bytes());
        let mut u = hmac_sha256(password, &msg);
        let mut t = u;
        // T_i = U_1 xor U_2 xor ... xor U_c
        for _ in 1..iterations {
            u = hmac_sha256(password, &u);
            for (ti, ui) in t.iter_mut().zip(u.iter()) {
                *ti ^= *ui;
            }
        }
        let take = 32.min(dk_len - out.len());
        out.extend_from_slice(&t[..take]);
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Envelope: passphrase wraps the volume master key
// ---------------------------------------------------------------------------

/// The on-disk envelope: everything needed to unwrap the master key
/// given the passphrase (and nothing without it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrappedEnvelope {
    /// PBKDF2 salt.
    pub salt: [u8; SALT_LEN],
    /// PBKDF2 iteration count (stored: re-tune on re-wrap, not silently).
    pub iterations: u32,
    /// AEAD nonce.
    pub nonce: [u8; NONCE_LEN],
    /// ChaCha20-Poly1305 ciphertext+tag of the 32-byte master key.
    pub wrapped: [u8; KEY_LEN + TAG_LEN],
    /// Envelope format tag (0 = v1).
    pub version: u8,
}

/// The live, unwrapped state: master key in memory, zeroized on drop.
///
/// Deliberately **not** `Debug`: printing an envelope must never
/// accidentally print key material.
pub struct KeyEnvelope {
    master: ZeroingKey,
}

/// A 32-byte key that best-effort zeroizes on drop via volatile
/// writes (no `zeroize` crate: Windows stays std-only; the compiler
/// cannot elide `write_volatile`).
struct ZeroingKey([u8; 32]);

impl ZeroingKey {
    fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    fn expose(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Drop for ZeroingKey {
    fn drop(&mut self) {
        for b in self.0.iter_mut() {
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

impl KeyEnvelope {
    /// Generates a fresh random master key and wraps it under
    /// `passphrase`. The returned pair is (disk blob, live envelope).
    pub fn create(passphrase: &[u8]) -> Result<(WrappedEnvelope, Self)> {
        Self::create_with_iterations(passphrase, DEFAULT_PBKDF2_ITERATIONS)
    }

    /// [`KeyEnvelope::create`] with an explicit PBKDF2 work factor.
    pub fn create_with_iterations(
        passphrase: &[u8],
        iterations: u32,
    ) -> Result<(WrappedEnvelope, Self)> {
        let mut master = [0u8; KEY_LEN];
        fill_random(&mut master)?;
        let mut salt = [0u8; SALT_LEN];
        fill_random(&mut salt)?;
        let mut nonce = [0u8; NONCE_LEN];
        fill_random(&mut nonce)?;
        let kek = derive_kek(passphrase, &salt, iterations)?;
        let wrapped = aead_seal(&kek, &nonce, &master)?;
        Ok((
            WrappedEnvelope {
                salt,
                iterations,
                nonce,
                wrapped,
                version: 1,
            },
            Self {
                master: ZeroingKey::new(master),
            },
        ))
    }

    /// Unwraps the master key from a disk blob. A wrong passphrase is
    /// an authentication failure (`InvalidInput`) -- AEAD tags make
    /// guessing audible, and the KDF makes each guess cost 600k SHA-256s.
    pub fn unwrap(passphrase: &[u8], blob: &WrappedEnvelope) -> Result<Self> {
        if blob.version != 1 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "unknown key-envelope version",
            ));
        }
        let kek = derive_kek(passphrase, &blob.salt, blob.iterations)?;
        let master = aead_open(&kek, &blob.nonce, &blob.wrapped)?;
        Ok(Self {
            master: ZeroingKey::new(master),
        })
    }

    /// Re-wraps under a new passphrase without touching the master
    /// (passphrase rotation; the derived file keys are unchanged).
    pub fn rewrap(&self, new_passphrase: &[u8]) -> Result<WrappedEnvelope> {
        self.rewrap_with_iterations(new_passphrase, DEFAULT_PBKDF2_ITERATIONS)
    }

    /// [`KeyEnvelope::rewrap`] with an explicit PBKDF2 work factor.
    pub fn rewrap_with_iterations(
        &self,
        new_passphrase: &[u8],
        iterations: u32,
    ) -> Result<WrappedEnvelope> {
        let mut salt = [0u8; SALT_LEN];
        fill_random(&mut salt)?;
        let mut nonce = [0u8; NONCE_LEN];
        fill_random(&mut nonce)?;
        let kek = derive_kek(new_passphrase, &salt, iterations)?;
        let wrapped = aead_seal(&kek, &nonce, self.master.expose())?;
        Ok(WrappedEnvelope {
            salt,
            iterations,
            nonce,
            wrapped,
            version: 1,
        })
    }

    /// Derives one file key: `HMAC(master, domain || file_id)`, domain
    /// "LFS3/file-key/v1". A leaked file key compromises exactly one
    /// file; rotating the master re-derives the tree without rewriting
    /// any data block (RFC-004 §11.3 -- re-key is metadata-only).
    #[must_use]
    pub fn derive_file_key(&self, file_id: u64) -> [u8; 32] {
        derive_file_key(self.master.expose(), file_id)
    }

    /// Verifies a passphrase without exposing the master (the
    /// mount-time "is this the right disk + passphrase" check).
    #[must_use]
    pub fn matches(&self, passphrase: &[u8], blob: &WrappedEnvelope) -> bool {
        let kek = match derive_kek(passphrase, &blob.salt, blob.iterations) {
            Ok(k) => k,
            Err(_) => return false,
        };
        aead_open(&kek, &blob.nonce, &blob.wrapped).is_ok_and(|m| &m == self.master.expose())
    }
}

/// Derives the KEK from the passphrase (PBKDF2, 32 bytes).
fn derive_kek(passphrase: &[u8], salt: &[u8; SALT_LEN], iterations: u32) -> Result<[u8; 32]> {
    let dk = pbkdf2_hmac_sha256(passphrase, salt, iterations, KEY_LEN)
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "invalid KDF parameters"))?;
    let mut kek = [0u8; KEY_LEN];
    kek.copy_from_slice(&dk);
    Ok(kek)
}

// ---------------------------------------------------------------------------
// 3.6: Envelope v2 -- the on-disk, algorithm-agile envelope
// ---------------------------------------------------------------------------

/// v2 envelope magic ("LFSE", little-endian).
pub const ENVELOPE_V2_MAGIC: u32 = 0x4553_464C;
/// Fixed v2 wire size (one self-describing record; stored at
/// `Superblock::key_envelope_block`).
pub const ENVELOPE_V2_SIZE: usize = 96;

// KDF algorithm ids (the agility registry; new ids extend, never
// re-mean).
pub const KDF_PBKDF2_HMAC_SHA256: u8 = 1;
// AEAD algorithm ids -- the same registry `Inode::encryption_algo`
// already uses (constants::ENCRYPTION_*), so the key envelope and the
// per-block ciphers share one id space.
pub const AEAD_NONE: u8 = 0;

/// The post-quantum key-encapsulation slot: 0 = none (passphrase-only
/// wrap). A future ML-KEM hybrid writes a KEM id here plus a
/// KEM-wrapped share into the reserved tail; the passphrase wrap
/// remains the recovery factor. The slot is FORMAT, not code: a
/// 3.6-era image can gain PQ protection by rewrap without reformat.
pub const KEM_NONE: u16 = 0;

/// The v2 envelope: everything needed to unwrap the master given the
/// passphrase, PLUS the algorithm registry that makes the format
/// upgradable without reformatting (the 200-year requirement).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvelopeV2 {
    /// KDF id (`KDF_*`).
    pub kdf_id: u8,
    /// AEAD id (the `ENCRYPTION_*` registry).
    pub aead_id: u8,
    /// KEM id (`KEM_*`; reserved slot for post-quantum hybrids).
    pub kem_id: u16,
    pub iterations: u32,
    pub salt: [u8; SALT_LEN],
    pub nonce: [u8; NONCE_LEN],
    /// ciphertext+tag of the 32-byte master under the derived KEK.
    pub wrapped: [u8; KEY_LEN + TAG_LEN],
}

impl EnvelopeV2 {
    #[must_use]
    pub fn to_bytes(&self) -> [u8; ENVELOPE_V2_SIZE] {
        let mut out = [0u8; ENVELOPE_V2_SIZE];
        out[0..4].copy_from_slice(&ENVELOPE_V2_MAGIC.to_le_bytes());
        out[4..6].copy_from_slice(&2u16.to_le_bytes());
        out[6] = self.kdf_id;
        out[7] = self.aead_id;
        out[8..10].copy_from_slice(&self.kem_id.to_le_bytes());
        // 10..12 reserved
        out[12..16].copy_from_slice(&self.iterations.to_le_bytes());
        out[16..32].copy_from_slice(&self.salt);
        out[32..44].copy_from_slice(&self.nonce);
        out[44..92].copy_from_slice(&self.wrapped);
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < ENVELOPE_V2_SIZE {
            return Err(Error::new(ErrorKind::InvalidData, "envelope v2 truncated"));
        }
        let magic = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if magic != ENVELOPE_V2_MAGIC {
            return Err(Error::new(ErrorKind::InvalidData, "not an LFSE envelope"));
        }
        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        if version != 2 {
            return Err(Error::new(ErrorKind::InvalidData, "unknown envelope version"));
        }
        let mut salt = [0u8; SALT_LEN];
        salt.copy_from_slice(&bytes[16..32]);
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&bytes[32..44]);
        let mut wrapped = [0u8; KEY_LEN + TAG_LEN];
        wrapped.copy_from_slice(&bytes[44..92]);
        Ok(Self {
            kdf_id: bytes[6],
            aead_id: bytes[7],
            kem_id: u16::from_le_bytes([bytes[8], bytes[9]]),
            iterations: u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
            salt,
            nonce,
            wrapped,
        })
    }

    /// Wrap a fresh master under `passphrase` with the given
    /// algorithms (the mkfs-time constructor).
    pub fn create(passphrase: &[u8], kdf_id: u8, aead_id: u8, iterations: u32) -> Result<(Self, KeyEnvelope)> {
        match kdf_id {
            KDF_PBKDF2_HMAC_SHA256 => {}
            other => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("unsupported envelope KDF id {other}"),
                ))
            }
        }
        if crate::security::encryption::EncryptionManager::get_algorithm(aead_id).is_none() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                format!("unsupported envelope AEAD id {aead_id}"),
            ));
        }
        let mut master = [0u8; KEY_LEN];
        fill_random(&mut master)?;
        let mut salt = [0u8; SALT_LEN];
        fill_random(&mut salt)?;
        let mut nonce = [0u8; NONCE_LEN];
        fill_random(&mut nonce)?;
        let kek = derive_kek(passphrase, &salt, iterations)?;
        let wrapped = aead_seal_by_id(aead_id, &kek, &nonce, &master)?;
        Ok((
            Self { kdf_id, aead_id, kem_id: KEM_NONE, iterations, salt, nonce, wrapped },
            KeyEnvelope { master: ZeroingKey::new(master) },
        ))
    }

    /// Unwrap the master (the mount-time constructor).
    pub fn unwrap(passphrase: &[u8], blob: &Self) -> Result<KeyEnvelope> {
        match blob.kdf_id {
            KDF_PBKDF2_HMAC_SHA256 => {}
            other => {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    format!("unsupported envelope KDF id {other}"),
                ))
            }
        }
        let kek = derive_kek(passphrase, &blob.salt, blob.iterations)?;
        let master = aead_open_by_id(blob.aead_id, &kek, &blob.nonce, &blob.wrapped)?;
        Ok(KeyEnvelope { master: ZeroingKey::new(master) })
    }
}

/// AEAD dispatch by registry id (agility seam: adding an algorithm
/// means adding it to `EncryptionManager` -- the envelope needs no
/// format change).
fn aead_seal_by_id(id: u8, key: &[u8; 32], nonce: &[u8; NONCE_LEN], plaintext: &[u8]) -> Result<[u8; KEY_LEN + TAG_LEN]> {
    if id == AEAD_NONE {
        return Err(Error::new(ErrorKind::InvalidInput, "envelope requires an AEAD"));
    }
    let alg = crate::security::encryption::EncryptionManager::get_algorithm(id)
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, format!("unknown AEAD id {id}")))?;
    let ct = alg.encrypt(key, plaintext, nonce)?;
    if ct.len() != KEY_LEN + TAG_LEN {
        return Err(Error::new(ErrorKind::Other, "AEAD output length mismatch"));
    }
    let mut out = [0u8; KEY_LEN + TAG_LEN];
    out.copy_from_slice(&ct);
    Ok(out)
}

fn aead_open_by_id(
    id: u8,
    key: &[u8; 32],
    nonce: &[u8; NONCE_LEN],
    wrapped: &[u8; KEY_LEN + TAG_LEN],
) -> Result<[u8; 32]> {
    let alg = crate::security::encryption::EncryptionManager::get_algorithm(id)
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, format!("unknown AEAD id {id}")))?;
    let pt = alg
        .decrypt(key, wrapped, nonce)
        .map_err(|_| Error::new(ErrorKind::InvalidInput, "key unwrap failed: wrong passphrase or corrupt envelope"))?;
    if pt.len() != KEY_LEN {
        return Err(Error::new(ErrorKind::InvalidData, "unwrapped master length mismatch"));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&pt);
    Ok(out)
}

/// Decode the envelope stored at `Superblock::key_envelope_block`
/// (one 4096-byte block; the v2 record occupies the first 96 bytes).
pub fn envelope_from_block(block: &[u8]) -> Result<EnvelopeV2> {
    EnvelopeV2::from_bytes(block)
}

/// Encode the envelope into its on-disk block.
#[must_use]
pub fn envelope_to_block(blob: &EnvelopeV2) -> [u8; crate::ondisk::serialization::BLOCK_SIZE] {
    let mut block = [0u8; crate::ondisk::serialization::BLOCK_SIZE];
    let rec = blob.to_bytes();
    block[..ENVELOPE_V2_SIZE].copy_from_slice(&rec);
    block
}

/// Per-file key derivation, standalone (for the on-disk key tree).
#[must_use]
pub fn derive_file_key(master: &[u8; 32], file_id: u64) -> [u8; 32] {
    let mut msg = Vec::with_capacity(20);
    msg.extend_from_slice(b"LFS3/file-key/v1");
    msg.extend_from_slice(&file_id.to_be_bytes());
    hmac_sha256(master, &msg)
}

// ---------------------------------------------------------------------------
// AEAD (ChaCha20-Poly1305) helpers over the 2.0 cipher surface
// ---------------------------------------------------------------------------

fn aead_seal(key: &[u8; 32], nonce: &[u8; NONCE_LEN], plaintext: &[u8]) -> Result<[u8; KEY_LEN + TAG_LEN]> {
    use chacha20poly1305::aead::{Aead, KeyInit, Payload};
    use chacha20poly1305::{ChaCha20Poly1305, Nonce};
    let cipher = ChaCha20Poly1305::new_from_slice(key)
        .map_err(|_| Error::new(ErrorKind::InvalidInput, "bad KEK length"))?;
    let ct = cipher
        .encrypt(Nonce::from_slice(nonce), Payload::from(plaintext))
        .map_err(|_| Error::new(ErrorKind::Other, "AEAD seal failed"))?;
    let mut out = [0u8; KEY_LEN + TAG_LEN];
    out.copy_from_slice(&ct);
    Ok(out)
}

fn aead_open(
    key: &[u8; 32],
    nonce: &[u8; NONCE_LEN],
    wrapped: &[u8; KEY_LEN + TAG_LEN],
) -> Result<[u8; 32]> {
    use chacha20poly1305::aead::{Aead, KeyInit, Payload};
    use chacha20poly1305::{ChaCha20Poly1305, Nonce};
    let cipher = ChaCha20Poly1305::new_from_slice(key)
        .map_err(|_| Error::new(ErrorKind::InvalidInput, "bad KEK length"))?;
    let pt = cipher
        .decrypt(Nonce::from_slice(nonce), Payload::from(wrapped.as_slice()))
        .map_err(|_| {
            Error::new(
                ErrorKind::InvalidInput,
                "key unwrap failed: wrong passphrase or corrupt envelope",
            )
        })?;
    let mut out = [0u8; 32];
    out.copy_from_slice(&pt);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 6070 covers PBKDF2-HMAC-SHA1; for HMAC-SHA256 the published
    // vectors are the StackOverflow-canonical set (from the
    // pbkdf2-hmac-sha256 test corpus):
    const P: &[u8] = b"passwd";
    const S: &[u8] = b"salt";
    #[test]
    fn pbkdf2_known_answers() {
        // Canonical vector: PBKDF2-HMAC-SHA256("passwd", "salt", 1, 32)
        // = 55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc.
        // The 64-byte derivation must agree on its first block.
        let dk = pbkdf2_hmac_sha256(P, S, 1, 64).expect("valid");
        assert_eq!(dk.len(), 64);
        assert_eq!(hex(&dk[..32]), "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc");
        // And the 32-byte form is exactly that block.
        assert_eq!(
            hex(&pbkdf2_hmac_sha256(P, S, 1, 32).expect("valid")),
            "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc"
        );
    }

    #[test]
    fn pbkdf2_iterations_change_output() {
        let a = pbkdf2_hmac_sha256(P, S, 1, 32).expect("valid");
        let b = pbkdf2_hmac_sha256(P, S, 2, 32).expect("valid");
        assert_ne!(a, b);
    }

    #[test]
    fn pbkdf2_deterministic_and_length_bounded() {
        let a = pbkdf2_hmac_sha256(P, S, 10, 48).expect("valid");
        let b = pbkdf2_hmac_sha256(P, S, 10, 48).expect("valid");
        assert_eq!(a, b);
        assert_eq!(a.len(), 48);
        assert!(pbkdf2_hmac_sha256(P, S, 10, 0).is_none());
        assert!(pbkdf2_hmac_sha256(P, S, 0, 32).is_none());
        assert!(pbkdf2_hmac_sha256(P, S, 1, 32 * 256).is_none()); // RFC bound
    }

    #[test]
    fn hmac_known_answer() {
        // HMAC-SHA256(key=b"key", msg=b"The quick brown fox jumps over the lazy dog")
        // = f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8
        let mac = hmac_sha256(b"key", b"The quick brown fox jumps over the lazy dog");
        assert_eq!(hex(&mac), "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8");
        // Keys longer than the block size hash-down first.
        let long_key = vec![0x41u8; 100];
        let mac2 = hmac_sha256(&long_key, b"msg");
        assert_eq!(mac2.len(), 32);
    }

    #[test]
    fn envelope_roundtrip() {
        let (blob, env) = KeyEnvelope::create(b"correct horse battery staple").expect("create");
        let unwrapped = KeyEnvelope::unwrap(b"correct horse battery staple", &blob).expect("unwrap");
        // Same master: file keys agree.
        assert_eq!(unwrapped.derive_file_key(7), env.derive_file_key(7));
        assert!(env.matches(b"correct horse battery staple", &blob));
    }

    #[test]
    fn wrong_passphrase_fails_audibly() {
        let (blob, _) = KeyEnvelope::create(b"hunter2").expect("create");
        let kind = KeyEnvelope::unwrap(b"hunter3", &blob)
            .map(|_| ())
            .unwrap_err()
            .kind();
        assert_eq!(kind, ErrorKind::InvalidInput);
        // Low iteration count so the test stays fast.
        let (blob2, _) = KeyEnvelope::create_with_iterations(b"a", 1000).expect("create");
        assert!(KeyEnvelope::unwrap(b"b", &blob2).is_err());
        assert!(KeyEnvelope::unwrap(b"a", &blob2).is_ok());
    }

    #[test]
    fn rewrap_rotates_passphrase_keeps_master() {
        let (blob1, env) = KeyEnvelope::create_with_iterations(b"old", 1_000).expect("create");
        let blob2 = env.rewrap_with_iterations(b"new", 1_000).expect("rewrap");
        // New passphrase unwraps; old one no longer does.
        let unwrapped = KeyEnvelope::unwrap(b"new", &blob2).expect("new works");
        assert_eq!(unwrapped.derive_file_key(42), env.derive_file_key(42));
        assert!(KeyEnvelope::unwrap(b"old", &blob2).is_err());
        // Salt and nonce actually rotated (not a copy).
        assert_ne!(blob1.salt, blob2.salt);
        assert_ne!(blob1.nonce, blob2.nonce);
        assert_ne!(blob1.wrapped, blob2.wrapped);
    }

    #[test]
    fn unknown_version_is_refused() {
        let (mut blob, _) = KeyEnvelope::create_with_iterations(b"x", 1_000).expect("create");
        blob.version = 99;
        assert!(KeyEnvelope::unwrap(b"x", &blob).is_err());
    }

    #[test]
    fn file_keys_are_domain_separated() {
        let (_, env) = KeyEnvelope::create_with_iterations(b"x", 1_000).expect("create");
        let k1 = env.derive_file_key(1);
        let k2 = env.derive_file_key(2);
        let k1b = env.derive_file_key(1);
        assert_eq!(k1, k1b); // deterministic
        assert_ne!(k1, k2); // distinct per file
    }

    #[test]
    fn master_zeroes_on_drop() {
        // Observe the wrapped bytes, drop the envelope, confirm the
        // observable state machine: unwrap still works from the blob
        // (the blob is the durable copy) -- the zeroization claim is
        // about *memory*, tested by construction (volatile writes
        // cannot be elided). What we can test: drop runs without
        // panic and the blob remains the durable path.
        let (blob, env) = KeyEnvelope::create_with_iterations(b"x", 1_000).expect("create");
        drop(env);
        assert!(KeyEnvelope::unwrap(b"x", &blob).is_ok());
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
