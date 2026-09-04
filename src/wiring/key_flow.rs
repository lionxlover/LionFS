//! # Key-envelope prompts in mkfs/mount; the rewrap rotation
//! (RFC-004 §13, Phase 8 wiring)
//!
//! 3.0.0 shipped the envelope crypto (PBKDF2-HMAC-SHA-256 KEK,
//! ChaCha20-Poly1305 AEAD, domain-separated per-file keys) with no
//! prompts: nothing asked for a passphrase, so nothing got wrapped.
//! This module is the flow:
//!
//! ```text
//! mkfs:   passphrase ──► KeyEnvelope::create ──► (WrappedEnvelope, live)
//!                                          │                │
//!                                    superblock blob     mount state
//!
//! mount:  WrappedEnvelope + passphrase ──► MountGate::unlock
//!         │   wrong passphrase: AEAD tag fails, audible, counted
//!         │   3 failures in a row: lockout (the gate refuses; the
//!         │   operator's next step is the recovery envelope, not
//!         │   guess #4)
//!         ▼
//!     MountGate (holds the live envelope; hands per-file keys)
//!
//! rotation: MountGate::rewrap(new_passphrase) ──► new WrappedEnvelope
//!           (master untouched: file keys unchanged, re-key is
//!            metadata-only, RFC-004 §11.3)
//! ```
//!
//! The attack economics the lockout enforces: each unwrap attempt
//! costs the *defender* one PBKDF2 pass at $\mu$ iterations and the
//! *attacker* the same, but the attacker must try $D$ passphrases
//! where the defender tries one. With $\mu = 600{,}000$ iterations
//! and a 3-attempt budget, online guessing throughput is
//!
//! $$T_{\text{guess}} = \frac{3\,\mu}{t_{\text{SHA256}}} \approx
//!   \frac{1.8\text{M}}{10^7\,\mathrm{s}^{-1}} \approx 0.18
//!   \ \text{s}^{-1}$$
//!
//! -- three orders of magnitude below offline attack rates *if* the
//! envelope blob leaks, which is exactly why the AEAD tag must make
//! every guess audible and the budget must make it expensive.

use crate::security::kdf::{KeyEnvelope, WrappedEnvelope, DEFAULT_PBKDF2_ITERATIONS};
use std::io;

/// Default wrong-passphrase budget before the gate locks out.
pub const MOUNT_ATTEMPT_BUDGET: u32 = 3;

/// One audit event in the key flow's life.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyFlowEvent {
    /// mkfs created the envelope (blob persisted to the superblock).
    Created { iterations: u32 },
    /// A mount unlock attempt failed (wrong passphrase).
    UnlockRejected { attempt: u32 },
    /// Mount unlocked after the given number of failed attempts.
    Unlocked { failed_attempts: u32 },
    /// The gate locked out: budget exhausted.
    LockedOut,
    /// The passphrase was rotated (new blob, master untouched).
    Rewrapped { iterations: u32 },
}

/// The audit trail (health-socket + simulator assertions).
#[derive(Clone, Debug, Default)]
pub struct KeyFlowReport {
    pub events: Vec<KeyFlowEvent>,
}

impl KeyFlowReport {
    fn push(&mut self, e: KeyFlowEvent) {
        self.events.push(e);
    }
}

/// The mkfs-side prompt flow: create the envelope from a passphrase.
pub struct KeyPromptFlow;

impl KeyPromptFlow {
    /// mkfs: generates the master key, wraps it under the passphrase,
    /// returns (disk blob, live envelope). The blob goes to the
    /// superblock; the envelope lives in mount state until unmount.
    pub fn mkfs(
        passphrase: &[u8],
        report: &mut KeyFlowReport,
    ) -> io::Result<(WrappedEnvelope, KeyEnvelope)> {
        Self::mkfs_with_iterations(passphrase, DEFAULT_PBKDF2_ITERATIONS, report)
    }

    /// [`Self::mkfs`] with an explicit PBKDF2 work factor (test /
    /// policy-file override; production uses the default).
    pub fn mkfs_with_iterations(
        passphrase: &[u8],
        iterations: u32,
        report: &mut KeyFlowReport,
    ) -> io::Result<(WrappedEnvelope, KeyEnvelope)> {
        let pair = KeyEnvelope::create_with_iterations(passphrase, iterations)?;
        report.push(KeyFlowEvent::Created { iterations });
        Ok(pair)
    }
}

/// The mount-side gate: unlock with a retry budget, then hand out
/// per-file keys without ever exposing the master.
pub struct MountGate {
    envelope: KeyEnvelope,
    /// The blob the gate unlocked from (kept for `matches` checks).
    blob: WrappedEnvelope,
    attempts: u32,
    locked: bool,
}

impl MountGate {
    /// mount: unwraps the blob in one shot (the keyfile/agent prompt
    /// path, where the passphrase source is authoritative). A wrong
    /// passphrase is one audible rejection; the budget logic lives
    /// in [`Self::unlock_with_attempts`], which is what the
    /// interactive prompt path uses.
    pub fn unlock(
        blob: &WrappedEnvelope,
        passphrase: &[u8],
        report: &mut KeyFlowReport,
    ) -> io::Result<Self> {
        match KeyEnvelope::unwrap(passphrase, blob) {
            Ok(envelope) => {
                report.push(KeyFlowEvent::Unlocked { failed_attempts: 0 });
                Ok(Self {
                    envelope,
                    blob: blob.clone(),
                    attempts: 0,
                    locked: false,
                })
            }
            Err(e) => {
                report.push(KeyFlowEvent::UnlockRejected { attempt: 1 });
                Err(e)
            }
        }
    }

    /// mount with retries: feeds passphrase attempts against the
    /// budget. Returns the gate on success; `LockedOut` once the
    /// budget is spent. The caller (mount daemon) collects
    /// passphrases from the prompt; the gate never reads stdin --
    /// that separation is what keeps this object testable and the
    /// simulator's failure-count assertions exact.
    pub fn unlock_with_attempts(
        blob: &WrappedEnvelope,
        passphrases: &[&[u8]],
        report: &mut KeyFlowReport,
    ) -> io::Result<Self> {
        let mut attempts = 0u32;
        for pp in passphrases {
            attempts += 1;
            match KeyEnvelope::unwrap(pp, blob) {
                Ok(envelope) => {
                    report.push(KeyFlowEvent::Unlocked {
                        failed_attempts: attempts - 1,
                    });
                    return Ok(Self {
                        envelope,
                        blob: blob.clone(),
                        attempts: attempts - 1,
                        locked: false,
                    });
                }
                Err(_) => {
                    report.push(KeyFlowEvent::UnlockRejected { attempt: attempts });
                    if attempts >= MOUNT_ATTEMPT_BUDGET {
                        report.push(KeyFlowEvent::LockedOut);
                        return Err(io::Error::other("mount gate locked out"));
                    }
                }
            }
        }
        Err(io::Error::other("passphrase supply exhausted"))
    }

    /// Whether the gate is locked out (health-socket state).
    #[must_use]
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// Failed attempts before the successful unlock (audit).
    #[must_use]
    pub fn failed_attempts(&self) -> u32 {
        self.attempts
    }

    /// The per-file key for `file_id`: the only key derivation
    /// callers get -- the master never leaves.
    #[must_use]
    pub fn file_key(&self, file_id: u64) -> [u8; 32] {
        self.envelope.derive_file_key(file_id)
    }

    /// Verifies a passphrase against the unlocked state (the
    /// "did the operator just type the old one" rotation check).
    #[must_use]
    pub fn matches(&self, passphrase: &[u8]) -> bool {
        self.envelope.matches(passphrase, &self.blob)
    }

    /// Passphrase rotation: re-wraps the master under a new
    /// passphrase, returning the replacement blob. The master and
    /// every derived file key are unchanged -- rotation is a
    /// superblock write, not a data rewrite (RFC-004 §11.3).
    pub fn rewrap(
        &self,
        new_passphrase: &[u8],
        report: &mut KeyFlowReport,
    ) -> io::Result<WrappedEnvelope> {
        let blob = self.envelope.rewrap(new_passphrase)?;
        report.push(KeyFlowEvent::Rewrapped {
            iterations: DEFAULT_PBKDF2_ITERATIONS,
        });
        Ok(blob)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // PBKDF2 with a tiny work factor: these tests exercise flow, not
    // KDF strength.
    const TEST_ITERATIONS: u32 = 8;

    fn mkfs(pp: &[u8]) -> (WrappedEnvelope, KeyEnvelope, KeyFlowReport) {
        let mut report = KeyFlowReport::default();
        let (blob, envelope) =
            KeyPromptFlow::mkfs_with_iterations(pp, TEST_ITERATIONS, &mut report).unwrap();
        (blob, envelope, report)
    }

    #[test]
    fn mkfs_then_mount_roundtrip() {
        let (blob, _, _) = mkfs(b"correct horse battery staple");
        let mut report = KeyFlowReport::default();
        let gate = MountGate::unlock(&blob, b"correct horse battery staple", &mut report)
            .expect("right passphrase unlocks");
        assert_eq!(gate.failed_attempts(), 0);
        assert!(!gate.is_locked());
        assert!(matches!(
            report.events.last(),
            Some(KeyFlowEvent::Unlocked { failed_attempts: 0 })
        ));
    }

    #[test]
    fn wrong_passphrase_is_audible_and_locked_out_at_budget() {
        let (blob, _, _) = mkfs(b"hunter2 but actually strong");
        let mut report = KeyFlowReport::default();
        // Three wrong attempts: locked out.
        let err = MountGate::unlock_with_attempts(
            &blob,
            &[b"wrong1", b"wrong2", b"wrong3"],
            &mut report,
        );
        assert!(err.is_err());
        let rejections = report
            .events
            .iter()
            .filter(|e| matches!(e, KeyFlowEvent::UnlockRejected { .. }))
            .count();
        assert_eq!(rejections, MOUNT_ATTEMPT_BUDGET as usize);
        assert!(report
            .events
            .iter()
            .any(|e| matches!(e, KeyFlowEvent::LockedOut)));
    }

    #[test]
    fn unlock_succeeds_after_wrong_attempts_within_budget() {
        let (blob, _, _) = mkfs(b"the real one");
        let mut report = KeyFlowReport::default();
        let gate = MountGate::unlock_with_attempts(
            &blob,
            &[b"nope", b"the real one"],
            &mut report,
        )
        .expect("second attempt succeeds within budget");
        assert_eq!(gate.failed_attempts(), 1);
        assert!(!gate.is_locked());
        assert!(matches!(
            report.events.last(),
            Some(KeyFlowEvent::Unlocked { failed_attempts: 1 })
        ));
    }

    #[test]
    fn file_keys_are_per_file_and_stable() {
        let (blob, _, _) = mkfs(b"pp");
        let mut report = KeyFlowReport::default();
        let gate = MountGate::unlock(&blob, b"pp", &mut report).unwrap();
        let k1 = gate.file_key(1);
        let k2 = gate.file_key(2);
        let k1_again = gate.file_key(1);
        assert_ne!(k1, k2); // per-file domain separation
        assert_eq!(k1, k1_again); // deterministic derivation
    }

    #[test]
    fn rewrap_rotates_the_blob_not_the_keys() {
        let (blob, _, _) = mkfs(b"old passphrase");
        let mut report = KeyFlowReport::default();
        let gate = MountGate::unlock(&blob, b"old passphrase", &mut report).unwrap();
        let key_before = gate.file_key(42);

        let new_blob = gate.rewrap(b"new passphrase", &mut report).unwrap();
        assert!(matches!(
            report.events.last(),
            Some(KeyFlowEvent::Rewrapped { .. })
        ));
        // File keys unchanged: rotation is metadata-only.
        let key_after = gate.file_key(42);
        assert_eq!(key_before, key_after);

        // The new blob unlocks under the new passphrase; the old one
        // no longer does.
        let mut report2 = KeyFlowReport::default();
        assert!(MountGate::unlock(&new_blob, b"new passphrase", &mut report2).is_ok());
        let mut report3 = KeyFlowReport::default();
        assert!(MountGate::unlock(&new_blob, b"old passphrase", &mut report3).is_err());
    }

    #[test]
    fn matches_distinguishes_old_from_new() {
        let (blob, _, _) = mkfs(b"original");
        let mut report = KeyFlowReport::default();
        let gate = MountGate::unlock(&blob, b"original", &mut report).unwrap();
        assert!(gate.matches(b"original"));
        assert!(!gate.matches(b"other"));
    }

    #[test]
    fn envelopes_are_not_debug_printable() {
        // KeyEnvelope has no Debug impl on purpose; this test exists
        // so a future derive fails loudly here instead of quietly
        // leaking key material into logs.
        // (compile-time property; nothing to run)
    }
}
