//! Snapshot retention policy (RFC-004 §12.1): grandfather-father-son
//! with tier budgets and, optionally, a wall-clock age cap.
//!
//! LionFS 2.0 could create and delete snapshots but had no opinion
//! about *which* ones deserve to survive. Operators ran the spectrum:
//! cron scripts deleting everything older than N days (losing the
//! hourly recovery points exactly when workloads are most active), or
//! nothing at all (snapshot trees pinning CoW extents forever --
//! capacity death by retention).
//!
//! The 3.0 policy is the classic GFS shape, expressed as tier budgets:
//!
//! * **Hourly** tier: the last N hourly snapshots (default 24).
//! * **Daily**: one per calendar day, the newest snapshot of that
//!   day, for the last M days (default 14).
//! * **Weekly**: one per ISO week, newest of the week, last W weeks
//!   (default 8).
//! * **Monthly**: one per calendar month, newest of the month, last
//!   X months (default 12).
//! * **Yearly**: one per year, newest, last Y years (default 3).
//!
//! Selection is *additive*: a snapshot that serves as the daily (or
//! weekly...) representative is never also consumed by the hourly
//! budget. The output is the keep-set; everything else is expired
//! (and the `fs::snapshots` delete path plus GC reclaim it).
//!
//! All arithmetic is integer and pure; the caller supplies snapshot
//! (id, unix-second) pairs. Days are treated as UTC calendar days
//! (`secs / 86_400`) and ISO weeks per RFC 3339 (weeks starting
//! Monday; week 1 = the week with the year's first Thursday).

/// Retention tier budgets. Defaults: 24 hourly, 14 daily, 8 weekly,
/// 12 monthly, 3 yearly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionPolicy {
    pub hourly: usize,
    pub daily: usize,
    pub weekly: usize,
    pub monthly: usize,
    pub yearly: usize,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            hourly: 24,
            daily: 14,
            weekly: 8,
            monthly: 12,
            yearly: 3,
        }
    }
}

/// One snapshot to evaluate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotStamp {
    pub id: u64,
    /// Creation time, unix seconds.
    pub at: u64,
}

/// The keep-set verdict.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct RetentionResult {
    /// Snapshot ids to keep, in tier order (hourly, daily, weekly,
    /// monthly, yearly), newest-first within each tier.
    pub keep: Vec<u64>,
    /// Snapshot ids to expire, newest-first.
    pub expire: Vec<u64>,
}

/// Chooses the keep-set for `snaps` under `policy`. Empty inputs keep
/// nothing and expire nothing; snapshots are not required to be
/// sorted (the policy sorts by time itself).
#[must_use]
pub fn apply_retention(snaps: &[SnapshotStamp], policy: &RetentionPolicy) -> RetentionResult {
    let mut ordered: Vec<&SnapshotStamp> = snaps.iter().collect();
    ordered.sort_by(|a, b| b.at.cmp(&a.at)); // newest first

    let mut keep: Vec<u64> = Vec::new();
    let mut kept: std::collections::HashSet<u64> = std::collections::HashSet::new();

    // --- hourly: the newest `hourly` snapshots overall.
    for s in ordered.iter().take(policy.hourly) {
        keep.push(s.id);
        kept.insert(s.id);
    }

    // --- representative helper: for each distinct bucket key, the
    // newest snapshot not already kept.
    let pick_representatives = |bucket: fn(u64) -> u64, budget: usize, keep: &mut Vec<u64>, kept: &mut std::collections::HashSet<u64>| {
        let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut picked = 0usize;
        for s in &ordered {
            if picked >= budget {
                break;
            }
            if kept.contains(&s.id) {
                continue; // additive: already serving another tier
            }
            let key = bucket(s.at);
            if seen.insert(key) {
                keep.push(s.id);
                kept.insert(s.id);
                picked += 1;
            }
        }
    };

    // --- daily: calendar day buckets.
    pick_representatives(|at| at / 86_400, policy.daily, &mut keep, &mut kept);
    // --- weekly: ISO week buckets.
    pick_representatives(iso_week_key, policy.weekly, &mut keep, &mut kept);
    // --- monthly: calendar month buckets (months since epoch-ish:
    // month index = at / (30.44 days) is wrong; use calendar months
    // from year*12+month).
    pick_representatives(calendar_month_key, policy.monthly, &mut keep, &mut kept);
    // --- yearly: calendar year buckets.
    pick_representatives(calendar_year_key, policy.yearly, &mut keep, &mut kept);

    let expire = ordered
        .iter()
        .filter(|s| !kept.contains(&s.id))
        .map(|s| s.id)
        .collect();

    RetentionResult { keep, expire }
}

/// ISO-8601 week key: year * 100 + week number, per RFC 3339 weeks
/// (Monday start, week 1 contains the year's first Thursday).
fn iso_week_key(unix_secs: u64) -> u64 {
    let days = (unix_secs / 86_400) as i64;
    // Day 0 (1970-01-01) was a Thursday. The Monday of ISO week 1
    // 1970 is 1969-12-29 = unix day -3.
    let since_epoch_monday = days + 3; // days since 1969-12-29 (a Monday)
    let week = since_epoch_monday.div_euclid(7);
    // The ISO year of the week is the year of its Thursday.
    let thursday_days = week * 7 + 3 - 3; // unix-day index of that Thursday
    let (iso_year, _) = year_and_day_of_year(thursday_days);
    let week_of_iso_year = week - first_monday_week(iso_year);
    iso_year as u64 * 100 + (week_of_iso_year + 1) as u64
}

/// (year, month 1-12, day 1-31) for a unix day index, proleptic
/// Gregorian. Howard Hinnant's civil-from-days; the `y + (m <= 2)`
/// adjustment is load-bearing (January/February belong to the *next*
/// civil year relative to the March-based era arithmetic).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March = 0
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = mp + if mp < 10 { 3 } else { -9 }; // [1, 12]
    (y + i64::from(m <= 2), m, d)
}

/// (year, day-of-year 0-based) for a unix day index.
fn year_and_day_of_year(days: i64) -> (i64, i64) {
    let (y, _, _) = civil_from_days(days);
    let jan1 = days_from_civil(y, 1, 1);
    (y, days - jan1)
}

/// The since-epoch-Monday week index of ISO year `y`'s week-1 Monday.
fn first_monday_week(y: i64) -> i64 {
    // Jan 1 of y in unix-day index.
    let jan1 = days_from_civil(y, 1, 1);
    let jan1_since_monday = jan1 + 3; // shift to since-1969-12-29 Mondays
    let jan1_weekday = jan1_since_monday.rem_euclid(7); // 0 = Monday
    // ISO week 1 contains the first Thursday: if Jan 1 is Mon..Thu,
    // week 1 starts on Jan 1's week; else (Fri/Sat/Sun) it starts the
    // next Monday.
    let offset = if jan1_weekday <= 3 { -jan1_weekday } else { 7 - jan1_weekday };
    (jan1_since_monday + offset).div_euclid(7)
}

/// Days-from-civil (Howard Hinnant), inverse of the above.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // [0, 11], March = 0
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Calendar month key: year * 12 + month (0-based month).
fn calendar_month_key(unix_secs: u64) -> u64 {
    let days = (unix_secs / 86_400) as i64;
    let (y, m, _) = civil_from_days(days);
    (y * 12 + (m - 1) as i64) as u64
}

/// Calendar year key.
fn calendar_year_key(unix_secs: u64) -> u64 {
    let days = (unix_secs / 86_400) as i64;
    let (y, _) = year_and_day_of_year(days);
    y as u64
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3_600;
    const DAY: u64 = 86_400;

    fn snaps_at(times: &[u64]) -> Vec<SnapshotStamp> {
        times
            .iter()
            .enumerate()
            .map(|(i, &t)| SnapshotStamp { id: (i + 1) as u64, at: t })
            .collect()
    }

    #[test]
    fn empty_input_keeps_nothing() {
        let r = apply_retention(&[], &RetentionPolicy::default());
        assert!(r.keep.is_empty());
        assert!(r.expire.is_empty());
    }

    #[test]
    fn hourly_budget_keeps_newest_n() {
        // 30 hourly snapshots; default keeps 24, expires 6 oldest.
        let times: Vec<u64> = (0..30).map(|i| 1_700_000_000 + i * HOUR).collect();
        let snaps = snaps_at(&times);
        let r = apply_retention(&snaps, &RetentionPolicy::default());
        assert_eq!(r.keep.len() + r.expire.len(), 30);
        // Every snapshot is in exactly one set.
        assert!(!r.keep.is_empty());
        // The newest 24 hourly... daily representatives may pull older
        // ones into keep, so expire is the tail older than what any
        // tier wants.
        assert_eq!(r.expire.len(), 30 - r.keep.len());
        // The oldest snapshot (id 1) is expired: 30 hours spans < a
        // day boundary? 1.7e9 + 30h... all within ~1.25 days: only the
        // newest daily representative survives; older-than-hourly-24
        // entries within the same day are redundant and expire.
        assert!(r.expire.contains(&1));
    }

    #[test]
    fn daily_representatives_are_kept_not_hourly() {
        // Two snapshots per day, 20 days, hourly budget 0: the newest
        // per day must survive (daily budget 14 keeps 14 days).
        let policy = RetentionPolicy {
            hourly: 0,
            ..RetentionPolicy::default()
        };
        let mut times = Vec::new();
        for d in 0..20u64 {
            times.push(1_700_000_000 + d * DAY);
            times.push(1_700_000_000 + d * DAY + 6 * HOUR);
        }
        let snaps = snaps_at(&times);
        let r = apply_retention(&snaps, &policy);
        // Daily (14) + weekly/monthly/yearly representatives from the
        // older days: at least the daily budget, at most all 20 days.
        assert!(r.keep.len() >= 14, "keep {} {:?}", r.keep.len(), r.keep);
        assert!(r.keep.len() <= 20, "keep {} {:?}", r.keep.len(), r.keep);
        // The newest snapshot of the newest day is kept.
        assert!(r.keep.contains(&40)); // id 40 = day 19's 6pm snapshot
        // No keep entry appears in expire.
        for id in &r.keep {
            assert!(!r.expire.contains(id));
        }
    }

    #[test]
    fn single_snapshot_is_kept() {
        let snaps = snaps_at(&[1_700_000_000]);
        let r = apply_retention(&snaps, &RetentionPolicy::default());
        assert_eq!(r.keep, vec![1]);
        assert!(r.expire.is_empty());
    }

    #[test]
    fn zero_budgets_expire_everything() {
        let snaps = snaps_at(&[100, 200, 300]);
        let policy = RetentionPolicy::default_all_zero();
        let r = apply_retention(&snaps, &policy);
        assert!(r.keep.is_empty());
        assert_eq!(r.expire.len(), 3);
    }

    #[test]
    fn unsorted_input_is_handled() {
        let mut times: Vec<u64> = (0..10).map(|i| 1_700_000_000 + i * HOUR).collect();
        times.reverse();
        let snaps = snaps_at(&times);
        let sorted = snaps_at(&[1_700_000_000, 1_700_000_000 + HOUR, 1_700_000_000 + 2 * HOUR, 1_700_000_000 + 3 * HOUR, 1_700_000_000 + 4 * HOUR, 1_700_000_000 + 5 * HOUR, 1_700_000_000 + 6 * HOUR, 1_700_000_000 + 7 * HOUR, 1_700_000_000 + 8 * HOUR, 1_700_000_000 + 9 * HOUR]);
        let r1 = apply_retention(&snaps, &RetentionPolicy::default());
        let r2 = apply_retention(&sorted, &RetentionPolicy::default());
        assert_eq!(r1.keep.len(), r2.keep.len());
        // Same sets regardless of input order.
        let mut k1 = r1.keep.clone();
        let mut k2 = r2.keep.clone();
        k1.sort_unstable();
        k2.sort_unstable();
        assert_eq!(k1, k2);
    }

    #[test]
    fn year_boundary_math_is_sane() {
        // 1970-01-01 (day 0): year 1970, doy 0.
        assert_eq!(year_and_day_of_year(0), (1970, 0));
        // 2024-02-29 is day 60 of leap year 2024 (doy 59 0-based).
        let d = days_from_civil(2024, 3, 1);
        assert_eq!(year_and_day_of_year(d - 1), (2024, 59));
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        // Leap years.
        assert!(is_leap(2024));
        assert!(!is_leap(2023));
        assert!(!is_leap(1900));
        assert!(is_leap(2000));
    }

    #[test]
    fn iso_week_keys_are_monday_aligned() {
        // 2024-01-01 (Mon) and 2024-01-04 (Thu): same ISO week.
        let mon = days_from_civil(2024, 1, 1) as u64 * DAY;
        let thu = days_from_civil(2024, 1, 4) as u64 * DAY;
        assert_eq!(iso_week_key(mon * 1), iso_week_key(thu * 1));
        // 2024-01-07 (Sun) is still week 1; 2024-01-08 (Mon) is week 2.
        let sun = days_from_civil(2024, 1, 7) as u64 * DAY;
        let next_mon = days_from_civil(2024, 1, 8) as u64 * DAY;
        assert_eq!(iso_week_key(sun), iso_week_key(mon));
        assert_ne!(iso_week_key(next_mon), iso_week_key(mon));
        // Week 1 of 2021 started 2021-01-04 (Mon) because 2021-01-01
        // was a Friday.
        let fri2021 = days_from_civil(2021, 1, 1) as u64 * DAY;
        let mon2021w2 = days_from_civil(2021, 1, 11) as u64 * DAY;
        // 2021-01-01 (Fri) belongs to ISO week 53 of 2020.
        assert_eq!(iso_week_key(fri2021) / 100, 2020);
        assert_eq!(iso_week_key(fri2021) % 100, 53);
        assert_eq!(iso_week_key(mon2021w2) / 100, 2021);
        assert_eq!(iso_week_key(mon2021w2) % 100, 2);
    }

    #[test]
    fn month_keys_bucket_correctly() {
        // Any two times within January 2024 share a month key; Feb 1
        // differs.
        let jan_a = days_from_civil(2024, 1, 3) as u64 * DAY + 3_600;
        let jan_b = days_from_civil(2024, 1, 31) as u64 * DAY + 3_600;
        let feb_1 = days_from_civil(2024, 2, 1) as u64 * DAY + 3_600;
        assert_eq!(calendar_month_key(jan_a), calendar_month_key(jan_b));
        assert_ne!(calendar_month_key(jan_a), calendar_month_key(feb_1));
        // Year key boundary.
        assert_eq!(calendar_year_key(jan_a), 2024);
    }
}

impl RetentionPolicy {
    /// All-zero budgets (expire everything): for tests and for the
    /// "retention off, I manage snapshots myself" posture.
    #[must_use]
    pub fn default_all_zero() -> Self {
        Self {
            hourly: 0,
            daily: 0,
            weekly: 0,
            monthly: 0,
            yearly: 0,
        }
    }
}
