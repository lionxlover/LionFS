//! Real authenticated encryption for LionFS data blocks.
//!
//! Both algorithms are AEAD ciphers with a 96-bit (12-byte) nonce and a
//! 128-bit (16-byte) authentication tag. Nonces must be unique per
//! (key, block) and are generated fresh via the OS CSPRNG (`/dev/urandom`)
//! by the caller -- see `security::block_cipher`, which generates one per
//! block write and stores it alongside the ciphertext. The tag is returned
//! appended to the ciphertext, matching the standard RustCrypto
//! `Aead::encrypt` output layout (`ciphertext || 16-byte tag`).

use aes_gcm::{Aes256Gcm as RcAes256Gcm, Nonce as AesNonce};
use chacha20poly1305::{ChaCha20Poly1305 as RcChaCha20Poly1305, Nonce as ChaChaNonce};
// Both RustCrypto AEAD crates re-export the same underlying `aead` crate,
// so a single `Aead` / `KeyInit` import works for both cipher types below.
// Each cipher's own `Nonce` type alias (rather than routing through
// `aead::generic_array::GenericArray` generically) is used for
// constructing nonces, since that's the directly-exported, canonical path
// for these crates.
use aes_gcm::aead::{Aead, KeyInit};
use std::io::{Error, ErrorKind, Read, Result};

pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;

/// Fills `out` with cryptographically secure random bytes from the OS.
///
/// Uses `/dev/urandom` directly rather than pulling in a `rand` crate
/// dependency: on Linux (LionFS's only target) this is a well-established
/// correct CSPRNG source, and it keeps the dependency surface for
/// something as sensitive as key/nonce generation as small as possible.
/// Fills `out` from the OS CSPRNG. 2.0: routes through the PAL
/// (`pal::random`), which handles Linux getrandom, macOS/BSD
/// getentropy, and Windows ProcessPrng -- the 1.x version read
/// /dev/urandom directly, which does not exist on Windows.
pub fn fill_random(out: &mut [u8]) -> Result<()> {
    crate::pal::random::fill_random(out)
}

pub fn generate_nonce() -> Result<[u8; NONCE_LEN]> {
    let mut nonce = [0u8; NONCE_LEN];
    fill_random(&mut nonce)?;
    Ok(nonce)
}

pub trait EncryptionAlgorithm {
    fn id(&self) -> u8;
    /// Encrypts `data` under `key` (32 bytes) and `nonce` (12 bytes).
    /// Returns `ciphertext || 16-byte tag`, i.e. `data.len() + TAG_LEN` bytes.
    fn encrypt(&self, key: &[u8], data: &[u8], nonce: &[u8]) -> Result<Vec<u8>>;
    /// Reverses `encrypt`. `data` must be `ciphertext || tag`. Fails with
    /// `InvalidData` if the authentication tag does not verify -- a real
    /// cryptographic integrity check: tampered or corrupted ciphertext is
    /// rejected outright, never silently decoded into garbage.
    fn decrypt(&self, key: &[u8], data: &[u8], nonce: &[u8]) -> Result<Vec<u8>>;
}

fn check_lengths(key: &[u8], nonce: &[u8], algo: &str) -> Result<()> {
    if key.len() != 32 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("{algo} requires a 32-byte key"),
        ));
    }
    if nonce.len() != NONCE_LEN {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("{algo} requires a {NONCE_LEN}-byte nonce"),
        ));
    }
    Ok(())
}

fn map_aead_err(context: &str) -> Error {
    Error::new(
        ErrorKind::InvalidData,
        format!(
            "{context}: authentication failed (wrong key, wrong nonce, or corrupted/tampered data)"
        ),
    )
}

pub struct Aes256Gcm;
impl EncryptionAlgorithm for Aes256Gcm {
    fn id(&self) -> u8 {
        1
    }

    fn encrypt(&self, key: &[u8], data: &[u8], nonce: &[u8]) -> Result<Vec<u8>> {
        check_lengths(key, nonce, "AES-256-GCM")?;
        let cipher = RcAes256Gcm::new_from_slice(key)
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "invalid AES-256-GCM key"))?;
        cipher
            .encrypt(AesNonce::from_slice(nonce), data)
            .map_err(|_| map_aead_err("AES-256-GCM encrypt"))
    }

    fn decrypt(&self, key: &[u8], data: &[u8], nonce: &[u8]) -> Result<Vec<u8>> {
        check_lengths(key, nonce, "AES-256-GCM")?;
        let cipher = RcAes256Gcm::new_from_slice(key)
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "invalid AES-256-GCM key"))?;
        cipher
            .decrypt(AesNonce::from_slice(nonce), data)
            .map_err(|_| map_aead_err("AES-256-GCM decrypt"))
    }
}

pub struct ChaCha20Poly1305;
impl EncryptionAlgorithm for ChaCha20Poly1305 {
    fn id(&self) -> u8 {
        2
    }

    fn encrypt(&self, key: &[u8], data: &[u8], nonce: &[u8]) -> Result<Vec<u8>> {
        check_lengths(key, nonce, "ChaCha20-Poly1305")?;
        let cipher = RcChaCha20Poly1305::new_from_slice(key)
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "invalid ChaCha20-Poly1305 key"))?;
        cipher
            .encrypt(ChaChaNonce::from_slice(nonce), data)
            .map_err(|_| map_aead_err("ChaCha20-Poly1305 encrypt"))
    }

    fn decrypt(&self, key: &[u8], data: &[u8], nonce: &[u8]) -> Result<Vec<u8>> {
        check_lengths(key, nonce, "ChaCha20-Poly1305")?;
        let cipher = RcChaCha20Poly1305::new_from_slice(key)
            .map_err(|_| Error::new(ErrorKind::InvalidInput, "invalid ChaCha20-Poly1305 key"))?;
        cipher
            .decrypt(ChaChaNonce::from_slice(nonce), data)
            .map_err(|_| map_aead_err("ChaCha20-Poly1305 decrypt"))
    }
}

pub struct EncryptionManager;

impl EncryptionManager {
    pub fn get_algorithm(id: u8) -> Option<Box<dyn EncryptionAlgorithm>> {
        match id {
            1 => Some(Box::new(Aes256Gcm)),
            2 => Some(Box::new(ChaCha20Poly1305)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod real_crypto_tests {
    use super::*;

    #[test]
    fn aes_gcm_roundtrip() {
        let key = [7u8; 32];
        let nonce = generate_nonce().unwrap();
        let algo = Aes256Gcm;
        let pt = b"the quick brown fox jumps over the lazy dog";
        let ct = algo.encrypt(&key, pt, &nonce).unwrap();
        assert_ne!(&ct[..pt.len()], &pt[..]); // actually encrypted, not passthrough
        assert_eq!(ct.len(), pt.len() + TAG_LEN);
        let back = algo.decrypt(&key, &ct, &nonce).unwrap();
        assert_eq!(back, pt);
    }

    #[test]
    fn chacha_roundtrip() {
        let key = [9u8; 32];
        let nonce = generate_nonce().unwrap();
        let algo = ChaCha20Poly1305;
        let pt = b"another plaintext block of data";
        let ct = algo.encrypt(&key, pt, &nonce).unwrap();
        let back = algo.decrypt(&key, &ct, &nonce).unwrap();
        assert_eq!(back, pt);
    }

    #[test]
    fn tampering_is_detected() {
        let key = [1u8; 32];
        let nonce = generate_nonce().unwrap();
        let algo = Aes256Gcm;
        let mut ct = algo.encrypt(&key, b"secret", &nonce).unwrap();
        ct[0] ^= 0xFF; // flip a bit in the ciphertext
        assert!(algo.decrypt(&key, &ct, &nonce).is_err());
    }

    #[test]
    fn wrong_key_is_rejected() {
        let nonce = generate_nonce().unwrap();
        let algo = ChaCha20Poly1305;
        let ct = algo.encrypt(&[1u8; 32], b"secret data", &nonce).unwrap();
        assert!(algo.decrypt(&[2u8; 32], &ct, &nonce).is_err());
    }
}
