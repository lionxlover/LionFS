//! ZNS zone model and zone-append placement (RFC-002 §6.1).
//!
//! For ZNS host-managed drives the write path becomes zone-append: the
//! engine submits appends with a write-pointer token per zone, the
//! device places the write wherever its media wants, and the returned
//! offset is recorded in the extent. Write amplification on sequential
//! fills drops to ~1.0 and the flash translation layer is bypassed
//! entirely. Zone placement policy: one file per zone until 85% full.
//!
//! The [`ZoneTable`] is the in-memory mirror of device zone state,
//! *recovered from the device report* at mount (recovery state 4:
//! RECONCILE) rather than trusted from disk -- the RFC's "extent map
//! fills after the fact; recovery trusts device zone reports" residual
//! risk row (Table 20) is managed here.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Zone states from the NVMe Zoned Namespace Command Set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneState {
    /// Empty, write pointer at zone start.
    Empty,
    /// Actively being appended to.
    Active,
    /// Full: write pointer at zone capacity.
    Full,
    /// Read-only (device decided).
    ReadOnly,
    /// Offline (device decided).
    Offline,
}

/// One zone's state.
#[derive(Debug, Clone, Copy)]
pub struct Zone {
    pub id: u32,
    /// First byte offset of the zone.
    pub start: u64,
    /// Capacity in bytes (zone size minus write-protected slack).
    pub capacity: u64,
    /// Current write pointer (bytes into the zone).
    pub write_pointer: u64,
    pub state: ZoneState,
}

impl Zone {
    #[must_use]
    pub fn free_bytes(&self) -> u64 {
        self.capacity.saturating_sub(self.write_pointer)
    }

    /// Fill ratio in basis points (0..=10000); the 85%-full switch is
    /// 8500 bp.
    #[must_use]
    pub fn fill_bps(&self) -> u32 {
        if self.capacity == 0 {
            return 10_000;
        }
        ((self.write_pointer * 10_000) / self.capacity) as u32
    }

    #[must_use]
    pub fn is_appendable(&self) -> bool {
        matches!(self.state, ZoneState::Empty | ZoneState::Active) && self.free_bytes() > 0
    }
}

/// The 85% fill threshold at which a new zone is opened for the file
/// (RFC-002 Table 12: "one file per zone until 85% full") -- leaving the
/// tail for the device's own GC reclaims reduces merge pressure.
pub const ZONE_FILL_SWITCH_BPS: u32 = 8_500;

/// Thread-safe zone table: mirrors device zone state, plans appends.
#[derive(Debug, Default)]
pub struct ZoneTable {
    zones: Mutex<BTreeMap<u32, Zone>>,
    appends_planned: AtomicU64,
    zones_switched: AtomicU64,
}

/// The placement plan returned by `plan_append`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppendPlan {
    pub zone: u32,
    /// Byte offset the append will land at (the write-pointer hint; a
    /// real device returns its own placement in the completion).
    pub offset: u64,
    pub len: u64,
}

impl ZoneTable {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers/overwrites a zone (mkfs or RECONCILE from device
    /// report).
    pub fn upsert_zone(&self, zone: Zone) {
        self.zones
            .lock()
            .expect("zone table lock")
            .insert(zone.id, zone);
    }

    /// Bulk replacement: the mount RECONCILE path, where the device
    /// report is the truth.
    pub fn reconcile_from_report(&self, report: Vec<Zone>) {
        let mut zones = self.zones.lock().expect("zone table lock");
        zones.clear();
        for z in report {
            zones.insert(z.id, z);
        }
    }

    pub fn zone_count(&self) -> usize {
        self.zones.lock().expect("zone table lock").len()
    }

    pub fn get(&self, id: u32) -> Option<Zone> {
        self.zones
            .lock()
            .expect("zone table lock")
            .get(&id)
            .copied()
    }

    /// Plans a zone append of `len` bytes for the file currently
    /// assigned to `zone` (or any appendable zone when `zone` is `None`).
    ///
    /// The policy: use the file's current zone until it is 85% full
    /// *or* the append no longer fits, then switch to a fresh zone. When
    /// no zone can fit the append, returns `None` (ENOSPC at the media
    /// layer).
    pub fn plan_append(&self, zone: Option<u32>, len: u64) -> Option<AppendPlan> {
        let mut zones = self.zones.lock().expect("zone table lock");
        // Preferred zone first.
        if let Some(id) = zone {
            if let Some(z) = zones.get_mut(&id) {
                if z.is_appendable() && z.fill_bps() < ZONE_FILL_SWITCH_BPS && z.free_bytes() >= len
                {
                    let plan = AppendPlan {
                        zone: id,
                        offset: z.start + z.write_pointer,
                        len,
                    };
                    z.write_pointer += len;
                    if z.state == ZoneState::Empty {
                        z.state = ZoneState::Active;
                    }
                    if z.write_pointer >= z.capacity {
                        z.state = ZoneState::Full;
                    }
                    self.appends_planned.fetch_add(1, Ordering::Relaxed);
                    return Some(plan);
                }
            }
        }
        // Switch: prefer the lowest-numbered empty zone (device-friendly
        // sequencing), else the emptiest active zone under the switch
        // threshold.
        let candidates: Vec<u32> = zones
            .values()
            .filter(|z| {
                z.is_appendable() && z.free_bytes() >= len && z.fill_bps() < ZONE_FILL_SWITCH_BPS
            })
            .map(|z| z.id)
            .collect();
        for id in candidates {
            let empty_first = zones
                .values()
                .find(|z| z.id == id)
                .map(|z| z.state == ZoneState::Empty)
                .unwrap_or(false);
            if empty_first {
                if let Some(z) = zones.get_mut(&id) {
                    let plan = AppendPlan {
                        zone: id,
                        offset: z.start + z.write_pointer,
                        len,
                    };
                    z.write_pointer += len;
                    z.state = if z.write_pointer >= z.capacity {
                        ZoneState::Full
                    } else {
                        ZoneState::Active
                    };
                    self.appends_planned.fetch_add(1, Ordering::Relaxed);
                    self.zones_switched.fetch_add(1, Ordering::Relaxed);
                    return Some(plan);
                }
            }
        }
        // Fall back to any appendable zone that fits (over the switch
        // threshold but not full).
        let fallback: Option<u32> = zones
            .values()
            .filter(|z| z.is_appendable() && z.free_bytes() >= len)
            .map(|z| z.id)
            .min();
        if let Some(id) = fallback {
            if let Some(z) = zones.get_mut(&id) {
                let plan = AppendPlan {
                    zone: id,
                    offset: z.start + z.write_pointer,
                    len,
                };
                z.write_pointer += len;
                if z.state == ZoneState::Empty {
                    z.state = ZoneState::Active;
                }
                if z.write_pointer >= z.capacity {
                    z.state = ZoneState::Full;
                }
                self.appends_planned.fetch_add(1, Ordering::Relaxed);
                return Some(plan);
            }
        }
        None
    }

    /// Records the *actual* placement a device reported for an append
    /// (real ZNS: completion-time update; the plan's offset was only a
    /// hint). Keeps the write pointer monotonic.
    pub fn commit_placed_offset(&self, zone: u32, placed_offset_in_zone: u64, len: u64) {
        let mut zones = self.zones.lock().expect("zone table lock");
        if let Some(z) = zones.get_mut(&zone) {
            let new_wp = placed_offset_in_zone + len;
            if new_wp >= z.capacity {
                z.write_pointer = z.capacity;
                z.state = ZoneState::Full;
            } else if new_wp > z.write_pointer {
                z.write_pointer = new_wp;
            }
        }
    }

    /// Resets a zone (zone reset command): write pointer back to zero,
    /// state Empty.
    pub fn reset_zone(&self, zone: u32) {
        let mut zones = self.zones.lock().expect("zone table lock");
        if let Some(z) = zones.get_mut(&zone) {
            if z.state != ZoneState::Offline && z.state != ZoneState::ReadOnly {
                z.write_pointer = 0;
                z.state = ZoneState::Empty;
            }
        }
    }

    /// Marks a zone offline (device report; future appends fail).
    pub fn mark_offline(&self, zone: u32) {
        let mut zones = self.zones.lock().expect("zone table lock");
        if let Some(z) = zones.get_mut(&zone) {
            z.state = ZoneState::Offline;
        }
    }

    /// Telemetry: total appends planned and zone switches.
    pub fn stats(&self) -> (u64, u64) {
        (
            self.appends_planned.load(Ordering::Relaxed),
            self.zones_switched.load(Ordering::Relaxed),
        )
    }

    /// Fill report for the health bus: (zone_id, fill_bps) for all zones.
    pub fn fill_report(&self) -> Vec<(u32, u32)> {
        let zones = self.zones.lock().expect("zone table lock");
        zones.values().map(|z| (z.id, z.fill_bps())).collect()
    }
}

/// Builds a canonical zone layout for an image/device: `n` zones of
/// `zone_size` bytes starting at `base`. Typical ZNS zone: 2-4 GiB.
#[must_use]
pub fn layout(base: u64, zone_size: u64, n: u32) -> Vec<Zone> {
    (0..n)
        .map(|id| Zone {
            id,
            start: base + u64::from(id) * zone_size,
            capacity: zone_size,
            write_pointer: 0,
            state: ZoneState::Empty,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_with(n: u32, zone_size: u64) -> ZoneTable {
        let t = ZoneTable::new();
        for z in layout(0, zone_size, n) {
            t.upsert_zone(z);
        }
        t
    }

    #[test]
    fn sequential_appends_fill_one_zone_until_switch() {
        let t = table_with(4, 1024 * 1024);
        let mut current: Option<u32> = None;
        let mut zones_used = std::collections::BTreeSet::new();
        // 4 KiB appends until no space: 1 MiB zones.
        for _ in 0..1024 {
            match t.plan_append(current, 4096) {
                Some(plan) => {
                    current = Some(plan.zone);
                    zones_used.insert(plan.zone);
                }
                None => break,
            }
        }
        // 85% switch: a 1 MiB zone takes 217 appends of 4 KiB before
        // switching (0.85 * 256 = 217.6). Four zones -> ~870 appends, and
        // multiple zones used.
        assert!(
            zones_used.len() >= 2,
            "must switch zones at 85%, used {zones_used:?}"
        );
        assert!(zones_used.len() <= 4);
        let (appends, _switches) = t.stats();
        assert!(appends > 800, "appends: {appends}");
    }

    #[test]
    fn plan_is_contiguous_within_zone() {
        let t = table_with(2, 64 * 1024);
        let p1 = t.plan_append(None, 4096).unwrap();
        let p2 = t.plan_append(Some(p1.zone), 4096).unwrap();
        assert_eq!(p1.zone, p2.zone);
        assert_eq!(p2.offset, p1.offset + 4096);
    }

    #[test]
    fn oversize_append_rejected_when_no_zone_fits() {
        let t = table_with(1, 4096);
        assert!(t.plan_append(None, 8192).is_none());
    }

    #[test]
    fn zone_fills_and_reports_full() {
        let t = table_with(1, 4096);
        let p = t.plan_append(None, 4096).unwrap();
        assert_eq!(p.offset, 0);
        assert!(t.plan_append(Some(p.zone), 1).is_none(), "zone is full");
        let z = t.get(p.zone).unwrap();
        assert_eq!(z.state, ZoneState::Full);
        assert_eq!(z.fill_bps(), 10_000);
    }

    #[test]
    fn reset_zone_reopens_for_appends() {
        let t = table_with(1, 4096);
        let p = t.plan_append(None, 4096).unwrap();
        assert!(t.plan_append(Some(p.zone), 1).is_none());
        t.reset_zone(p.zone);
        let p2 = t.plan_append(None, 8).unwrap();
        assert_eq!(p2.offset, 0);
    }

    #[test]
    fn offline_zone_is_not_appendable() {
        let t = table_with(1, 4096);
        t.mark_offline(0);
        assert!(t.plan_append(None, 8).is_none());
        t.reset_zone(0); // offline zones refuse reset too
        assert!(t.plan_append(None, 8).is_none());
    }

    #[test]
    fn reconcile_replaces_state_from_device_report() {
        let t = table_with(2, 4096);
        // Device report says zone 0 is offline with wp=0.
        let mut report = layout(0, 4096, 2);
        report[0].state = ZoneState::Offline;
        t.reconcile_from_report(report);
        assert_eq!(t.zone_count(), 2);
        assert_eq!(t.get(0).unwrap().state, ZoneState::Offline);
        let p = t.plan_append(None, 8).unwrap();
        assert_eq!(p.zone, 1, "zone 0 offline: must use zone 1");
    }

    #[test]
    fn commit_placed_offset_advances_monotonic() {
        let t = table_with(1, 10_000);
        // Device placed at in-zone offset 100.
        t.commit_placed_offset(0, 100, 200);
        assert_eq!(t.get(0).unwrap().write_pointer, 300);
        // A stale completion (older placement) must not move it back.
        t.commit_placed_offset(0, 0, 100);
        assert_eq!(t.get(0).unwrap().write_pointer, 300);
        // Filling to capacity marks the zone full.
        t.commit_placed_offset(0, 9_999, 1);
        assert_eq!(t.get(0).unwrap().state, ZoneState::Full);
    }

    #[test]
    fn layout_generation() {
        let zones = layout(0x1_0000, 4096, 3);
        assert_eq!(zones[0].start, 0x1_0000);
        assert_eq!(zones[1].start, 0x1_0000 + 4096);
        assert_eq!(zones[2].state, ZoneState::Empty);
    }

    #[test]
    fn fill_report_covers_all_zones() {
        let t = table_with(3, 4096);
        t.plan_append(None, 2048);
        let report = t.fill_report();
        assert_eq!(report.len(), 3);
        let (_, fill) = report.iter().find(|(id, _)| *id == 0).copied().unwrap();
        assert_eq!(fill, 5000); // 2048/4096 = 50%
    }
}
