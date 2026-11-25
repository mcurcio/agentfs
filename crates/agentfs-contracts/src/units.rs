//! The units agentfs's settings and flags are written in: durations
//! (`500ms`, `5s`, `30m`, `24h`) and git's sizes (`512k`, `100m`, `2g`).
//! docs/configuration.md lists the keys and flags that take them.

use std::time::Duration;

/// Parses `<integer><unit>` with unit `ms`, `s`, `m` or `h`.
pub fn parse_duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    let split = value.find(|c: char| !c.is_ascii_digit())?;
    let (number, unit) = value.split_at(split);
    let n: u64 = number.parse().ok()?;
    match unit {
        "ms" => Some(Duration::from_millis(n)),
        "s" => Some(Duration::from_secs(n)),
        "m" => Some(Duration::from_secs(n.checked_mul(60)?)),
        "h" => Some(Duration::from_secs(n.checked_mul(3600)?)),
        _ => None,
    }
}

/// Parses a positive byte count with an optional `k`, `m` or `g` suffix
/// (powers of 1024, either case), as git reads sizes: `512k`, `100m`, `2g`.
/// Zero is not a size.
pub fn parse_size(value: &str) -> Option<u64> {
    let value = value.trim();
    let (number, shift) = match value.as_bytes().last()? {
        b'k' | b'K' => (&value[..value.len() - 1], 10),
        b'm' | b'M' => (&value[..value.len() - 1], 20),
        b'g' | b'G' => (&value[..value.len() - 1], 30),
        _ => (value, 0),
    };
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = number.parse().ok()?;
    let n = n.checked_mul(1u64 << shift)?;
    (n > 0).then_some(n)
}

/// How a duration is written, for a refusal's text: what
/// [`parse_duration`] reads.
pub const DURATION_EXAMPLES: &str = "500ms, 5s, 30m, 24h";

/// A flag's value that is not a duration ([`parse_duration`]).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not a duration (use e.g. {DURATION_EXAMPLES})")]
pub struct NotADuration(pub String);

/// [`parse_duration`] as a command-line value parser (clap's
/// `value_parser = duration_arg`).
///
/// # Errors
///
/// [`NotADuration`] for a value [`parse_duration`] does not read.
pub fn duration_arg(value: &str) -> Result<Duration, NotADuration> {
    parse_duration(value).ok_or_else(|| NotADuration(value.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("100"), Some(100));
        assert_eq!(parse_size("512k"), Some(512 << 10));
        assert_eq!(parse_size("64M"), Some(64 << 20));
        assert_eq!(parse_size("2g"), Some(2 << 30));
        for bad in ["", "0", "0k", "k", "-1", "1.5m", "1t", "big", " m"] {
            assert_eq!(parse_size(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("200ms"), Some(Duration::from_millis(200)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("10"), None);
        assert_eq!(parse_duration("s"), None);
        assert_eq!(parse_duration("1d"), None);
    }

    #[test]
    fn a_flag_that_is_not_a_duration_says_how_to_write_one() {
        assert_eq!(duration_arg("5s"), Ok(Duration::from_secs(5)));
        assert_eq!(
            duration_arg("5").unwrap_err().to_string(),
            "\"5\" is not a duration (use e.g. 500ms, 5s, 30m, 24h)"
        );
    }
}
