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
}
