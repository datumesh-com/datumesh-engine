//! The engine's one clock reading per request: [`Clock`] and [`SystemClock`].

use std::time::{SystemTime, UNIX_EPOCH};

/// Where the engine reads the time. It reads it once per request: that reading is the
/// `issued` of every receipt the request signs, and the request's asof when none is given.
pub trait Clock: Send + Sync {
    /// The current time as an instant (SPEC 3.3, `YYYY-MM-DDTHH:MM:SS.ffffffZ`, UTC).
    fn now(&self) -> String;
}

/// The system clock (UTC, microseconds).
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> String {
        let micros = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros()).unwrap_or(0);
        instant_from_unix_micros(micros)
    }
}

/// An instant from microseconds since 1970-01-01T00:00:00Z (clamped to 9999-12-31).
pub(crate) fn instant_from_unix_micros(micros: u128) -> String {
    const MAX: u128 = 253_402_300_799_999_999; // 9999-12-31T23:59:59.999999Z
    let micros = micros.min(MAX);
    let secs = (micros / 1_000_000) as i64;
    let frac = (micros % 1_000_000) as u32;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{frac:06}Z", rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// Year, month, day of a day count since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instants() {
        assert_eq!(instant_from_unix_micros(0), "1970-01-01T00:00:00.000000Z");
        assert_eq!(instant_from_unix_micros(1_710_518_400_000_001), "2024-03-15T16:00:00.000001Z");
        assert_eq!(instant_from_unix_micros(951_782_400_000_000), "2000-02-29T00:00:00.000000Z");
        assert_eq!(instant_from_unix_micros(u128::MAX), "9999-12-31T23:59:59.999999Z");
        let now = SystemClock.now();
        datumesh_core::lexical::check_instant(&now, "$").unwrap();
    }
}
