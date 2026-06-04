//! Shared human-duration parsing (#56).
//!
//! One canonical parser feeds both the config layer (`[slow_log]
//! threshold`) and CLI args (`ferrule history --since`), replacing the two
//! near-identical inline parsers those call sites grew independently
//! during the Query Telemetry Foundation sprint. It returns
//! [`chrono::Duration`] so each caller maps to whatever unit it needs
//! (milliseconds for the slow-log threshold, the raw delta for `--since`).
//!
//! Recognised units — the union of the two original alias sets:
//!   - `ms`
//!   - `s` / `sec` / `secs`
//!   - `m` / `min` / `mins`
//!   - `h` / `hr` / `hrs`
//!   - `d` / `day` / `days`
//!
//! A unit suffix is **required**: a bare integer (`"500"`) is rejected
//! here. Callers that assign a default unit to a bare integer (e.g.
//! `[slow_log] threshold = "500"` → 500 ms) keep that quirk in their own
//! thin wrapper, handled before delegating.
//!
//! Out of scope, per #56 (kept here so the boundary is explicit):
//!   - The `humantime` crate — not worth the dependency at this caller
//!     count; revisit past ~5 callers.
//!   - Fractional units (`1.5h`, `0.5d`) — unused anywhere; YAGNI.
//!   - A byte-size parser for `[slow_log] max_size` (#55) — same shape,
//!     distinct unit class; share structure, not this signature.

use chrono::Duration;

/// Parse a human-readable duration like `250ms`, `30s`, `5m`, `2h`, or
/// `7d` into a [`chrono::Duration`].
///
/// A unit suffix is required; a bare integer is an error (see the module
/// docs for why, and which caller compensates). The error is a plain
/// `String` so both the `String`-erroring config layer and the
/// `CliError`-erroring CLI layer can wrap it without a shared error type.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("duration is empty".into());
    }
    let split = s
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| format!("duration '{s}' has no unit suffix (try 30s, 5m, 2h, 7d)"))?;
    let (num, unit) = s.split_at(split);
    if num.is_empty() {
        return Err(format!("duration '{s}': missing number before unit"));
    }
    let n: i64 = num
        .parse()
        .map_err(|_| format!("duration '{s}': invalid number '{num}'"))?;
    let unit = unit.trim();
    let dur = match unit {
        "ms" => Duration::milliseconds(n),
        "s" | "sec" | "secs" => Duration::seconds(n),
        "m" | "min" | "mins" => Duration::minutes(n),
        "h" | "hr" | "hrs" => Duration::hours(n),
        "d" | "day" | "days" => Duration::days(n),
        other => return Err(format!("duration '{s}': unknown unit '{other}'")),
    };
    Ok(dur)
}

/// Parse a human-readable byte size like `1024`, `10MB`, or `5MiB`
/// into a count of bytes.
///
/// Sibling of [`parse_duration`] (same trim -> split -> numeric-prefix ->
/// unit-match shape) but a distinct unit class, kept separate per the
/// module docs rather than folded into one over-general parser.
///
/// Recognised units:
///   - bare integer (`"1024"`) -> bytes
///   - `B` -> bytes
///   - decimal SI (powers of 1000): `KB`, `MB`, `GB`
///   - binary IEC (powers of 1024): `KiB`, `MiB`, `GiB`
///
/// The unit match is case-sensitive on the IEC `i` (so `KiB` is binary
/// and `KB` is decimal). A leading `-` is non-digit, so negative inputs
/// fall out the same way `parse_duration("-5m")` does (no number before
/// the unit). The multiplier is applied with [`u64::checked_mul`] so an
/// oversized input (`"99999999999GB"`) returns `Err` rather than
/// overflowing -- this is a library crate and must not panic.
///
/// The error is a plain `String` so both the `String`-erroring config
/// layer and the `CliError`-erroring CLI layer can wrap it without a
/// shared error type.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("size is empty".into());
    }
    let split = match s.find(|c: char| !c.is_ascii_digit()) {
        // No unit suffix at all -> a bare integer is a byte count.
        None => {
            return s.parse().map_err(|_| format!("size '{s}': invalid number"));
        }
        Some(i) => i,
    };
    let (num, unit) = s.split_at(split);
    if num.is_empty() {
        return Err(format!("size '{s}': missing number before unit"));
    }
    let n: u64 = num
        .parse()
        .map_err(|_| format!("size '{s}': invalid number '{num}'"))?;
    let unit = unit.trim();
    let mul: u64 = match unit {
        "B" => 1,
        "KB" => 1_000,
        "MB" => 1_000_000,
        "GB" => 1_000_000_000,
        "KiB" => 1_024,
        "MiB" => 1_024 * 1_024,
        "GiB" => 1_024 * 1_024 * 1_024,
        other => return Err(format!("size '{s}': unknown unit '{other}'")),
    };
    n.checked_mul(mul)
        .ok_or_else(|| format!("size '{s}': value overflows u64 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_unit_and_alias() {
        assert_eq!(
            parse_duration("250ms").unwrap(),
            Duration::milliseconds(250)
        );
        assert_eq!(parse_duration("30s").unwrap(), Duration::seconds(30));
        assert_eq!(parse_duration("30sec").unwrap(), Duration::seconds(30));
        assert_eq!(parse_duration("30secs").unwrap(), Duration::seconds(30));
        assert_eq!(parse_duration("5m").unwrap(), Duration::minutes(5));
        assert_eq!(parse_duration("5min").unwrap(), Duration::minutes(5));
        assert_eq!(parse_duration("90mins").unwrap(), Duration::minutes(90));
        assert_eq!(parse_duration("2h").unwrap(), Duration::hours(2));
        assert_eq!(parse_duration("2hr").unwrap(), Duration::hours(2));
        assert_eq!(parse_duration("2hrs").unwrap(), Duration::hours(2));
        assert_eq!(parse_duration("7d").unwrap(), Duration::days(7));
        assert_eq!(parse_duration("7day").unwrap(), Duration::days(7));
        assert_eq!(parse_duration("7days").unwrap(), Duration::days(7));
    }

    #[test]
    fn trims_surrounding_whitespace() {
        assert_eq!(parse_duration("  5m  ").unwrap(), Duration::minutes(5));
    }

    #[test]
    fn rejects_bare_integer() {
        // The canonical parser requires a unit; bare-integer defaulting is
        // a caller-specific quirk (see `SlowLogConfig::threshold_ms`).
        assert!(parse_duration("10").is_err());
    }

    #[test]
    fn rejects_empty_unit_and_unknown_inputs() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("   ").is_err());
        assert!(parse_duration("hour").is_err()); // no leading number
        assert!(parse_duration("ms").is_err()); // unit only, no number
        assert!(parse_duration("10x").is_err()); // unknown unit
        assert!(parse_duration("-5m").is_err()); // leading '-' is non-digit → no number
    }

    #[test]
    fn unit_whitespace_is_tolerated() {
        // The original `parse_threshold_ms` trimmed the unit, so `"5 m"`
        // resolves to 5 minutes. Preserved here for the slow-log path.
        assert_eq!(parse_duration("5 m").unwrap(), Duration::minutes(5));
    }

    #[test]
    fn error_messages_name_the_input() {
        let err = parse_duration("10x").unwrap_err();
        assert!(err.contains("10x"), "error should echo the input: {err}");
        assert!(
            err.contains("unknown unit"),
            "error should name the fault: {err}"
        );
    }

    #[test]
    fn parse_size_bare_integer_is_bytes() {
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("1024").unwrap(), 1024);
    }

    #[test]
    fn parse_size_decimal_si_units() {
        assert_eq!(parse_size("1B").unwrap(), 1);
        assert_eq!(parse_size("1KB").unwrap(), 1_000);
        assert_eq!(parse_size("1MB").unwrap(), 1_000_000);
        assert_eq!(parse_size("2GB").unwrap(), 2_000_000_000);
    }

    #[test]
    fn parse_size_binary_iec_units() {
        assert_eq!(parse_size("1KiB").unwrap(), 1_024);
        assert_eq!(parse_size("1MiB").unwrap(), 1_048_576);
        assert_eq!(parse_size("1GiB").unwrap(), 1_073_741_824);
    }

    #[test]
    fn parse_size_trims_surrounding_whitespace() {
        assert_eq!(parse_size("  5MB ").unwrap(), 5_000_000);
    }

    #[test]
    fn parse_size_rejects_bad_inputs() {
        assert!(parse_size("").is_err());
        assert!(parse_size("   ").is_err());
        assert!(parse_size("abc").is_err()); // no leading number
        assert!(parse_size("5x").is_err()); // unknown unit
        assert!(parse_size("-5MB").is_err()); // leading '-' is non-digit -> no number
    }

    #[test]
    fn parse_size_oversized_value_errors_without_panic() {
        // checked_mul: 99999999999 * 1_000_000_000 overflows u64.
        let err = parse_size("99999999999GB").unwrap_err();
        assert!(
            err.contains("99999999999GB"),
            "error should echo input: {err}"
        );
        assert!(
            err.contains("overflow"),
            "error should name overflow: {err}"
        );
    }

    #[test]
    fn parse_size_error_messages_name_the_input() {
        let err = parse_size("5x").unwrap_err();
        assert!(err.contains("5x"), "error should echo the input: {err}");
        assert!(
            err.contains("unknown unit"),
            "error should name the fault: {err}"
        );
    }
}
