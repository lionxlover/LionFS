//! Thin wrappers around the `log`/`env_logger` crates -- both are already
//! dependencies and `env_logger::init()` is already called from
//! `userspace::cli::mount`, but nothing in the codebase actually calls
//! `log::info!`/`log::warn!`/etc, so the logging infrastructure was wired
//! up but never used. This module gives call sites (recovery, the
//! scrubber, RAID rebuild) a small set of named helpers to log through,
//! and initializes the logger for tools that don't already do so
//! themselves.

/// Initializes `env_logger` if it hasn't been already -- safe to call from
/// multiple entry points (each CLI tool's `main`), since `try_init`
/// doesn't panic if a logger is already installed.
pub fn init() {
    let _ = env_logger::try_init();
}

pub fn log_recovery_replay(tx_id: u64, block_count: usize) {
    log::info!("recovery: replaying transaction {tx_id} ({block_count} blocks)");
}

pub fn log_corruption_detected(object_id: u64, logical_block: u64) {
    log::error!("checksum mismatch: object {object_id}, logical block {logical_block}");
}

pub fn log_raid_degraded(profile: &str, failed_device: usize) {
    log::warn!("{profile} array degraded: device {failed_device} is unreadable, reconstructing from redundancy");
}

pub fn log_scrub_summary(blocks_scanned: u64, errors_found: u64, errors_repaired: u64) {
    log::info!("scrub complete: {blocks_scanned} blocks scanned, {errors_found} errors found, {errors_repaired} repaired");
}
