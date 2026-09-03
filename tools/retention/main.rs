//! `lfs_retention` -- snapshot retention policy inspection
//! (RFC-004 §12.1).
//!
//! `lfs_retention sim` lays out a synthetic two-week snapshot history
//! (hourly + one week of dailies) and prints exactly which snapshots
//! the GFS policy keeps and which it expires -- the policy verdict,
//! observable without a mounted volume.

use lionfs_core::fs::retention::{apply_retention, RetentionPolicy, SnapshotStamp};

const HOUR: u64 = 3_600;
const DAY: u64 = 86_400;

fn main() {
    println!("LionFS snapshot retention simulation (RFC-004 §12.1, GFS)");
    let policy = RetentionPolicy::default();
    println!(
        "policy: {} hourly / {} daily / {} weekly / {} monthly / {} yearly",
        policy.hourly, policy.daily, policy.weekly, policy.monthly, policy.yearly
    );
    println!();

    // Two weeks of history: hourly for the last 3 days, daily for 14.
    let mut stamps = Vec::new();
    let mut next_id = 1u64;
    let base = 1_700_000_000u64;
    for d in 0..14u64 {
        // Daily snapshot at 00:00 each day.
        stamps.push(SnapshotStamp { id: next_id, at: base + d * DAY });
        next_id += 1;
        // Hourly snapshots only for the last 3 days.
        if d >= 11 {
            for h in 1..24u64 {
                stamps.push(SnapshotStamp { id: next_id, at: base + d * DAY + h * HOUR });
                next_id += 1;
            }
        }
    }
    println!("history: {} snapshots over 14 days (hourly for the last 3)", stamps.len());

    let result = apply_retention(&stamps, &policy);
    println!();
    println!("keep ({}):", result.keep.len());
    for id in &result.keep {
        let s = stamps.iter().find(|x| x.id == *id).expect("known id");
        let age_h = (base + 14 * DAY - s.at) / HOUR;
        println!("  snapshot {:>3}  age {:>4}h  ({}h into the timeline)", id, age_h, (s.at - base) / HOUR);
    }
    println!("expire ({}):", result.expire.len());
    for id in &result.expire {
        let s = stamps.iter().find(|x| x.id == *id).expect("known id");
        let age_h = (base + 14 * DAY - s.at) / HOUR;
        println!("  snapshot {:>3}  age {:>4}h", id, age_h);
    }
    println!();
    println!(
        "verdict: {} kept / {} expired / {} total",
        result.keep.len(),
        result.expire.len(),
        stamps.len()
    );
    assert_eq!(result.keep.len() + result.expire.len(), stamps.len());
}
