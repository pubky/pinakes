use chrono::{DateTime, Datelike, Duration, SubsecRound, Timelike, Utc};

/// The instant if its UTC year is 0001–9999: what Postgres reads back from the text form
/// (`to_rfc3339` writes year 0 as `0000`, which Postgres refuses, and years past 9999 with a
/// `+`). The string parsers accept year 0000 and local times that land in year 10000, so every
/// source instant that is stored or bound passes through this.
///
/// Postgres keeps microseconds and rounds anything finer, so 9999-12-31T23:59:59.9999995Z would
/// be stored as year 10000: the instant is truncated to microseconds first, and callers store the
/// returned value.
///
/// chrono keeps a leap second as nanoseconds ≥ 1e9 within the 59th second, so
/// 9999-12-31T23:59:60Z reads as year 9999 while Postgres rolls it into year 10000: a leap second
/// is first normalised into the next second.
pub fn bounded(t: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let t = if t.nanosecond() >= 1_000_000_000 {
        t.with_nanosecond(t.nanosecond() - 1_000_000_000)?
            .checked_add_signed(Duration::seconds(1))?
    } else {
        t
    };
    let t = t.trunc_subsecs(6);
    (1..=9999).contains(&t.year()).then_some(t)
}
