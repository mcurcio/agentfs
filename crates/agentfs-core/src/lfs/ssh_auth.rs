//! `git-lfs-authenticate` (design §7): how an SSH remote without an
//! explicit LFS endpoint finds one. git-lfs's SSH protocol runs
//! `git-lfs-authenticate <path> <upload|download>` on the git server,
//! which prints JSON naming the HTTP(S) endpoint and the header to send to
//! it:
//!
//! ```json
//! {"href": "https://lfs.example/acme/r.git/info/lfs",
//!  "header": {"Authorization": "RemoteAuth abc"},
//!  "expires_in": 3600}
//! ```
//!
//! agentfs runs it over its own SSH transport
//! ([`crate::ssh_transport::run_command`], `ssh` feature), bound to the
//! push URL's host and port with the remote's known_hosts, key or agent
//! and user, the host checked before any key is offered. The path is the
//! SSH URL's, without a leading `/`, sent unquoted as git-lfs sends it; a
//! path that would need quoting is refused. The answer is valid until
//! `expires_in` seconds or `expires_at` (RFC 3339), whichever is first,
//! less a few seconds; without either, for one push attempt or fetch.
//! The pure-SSH protocol (`git-lfs-transfer`) is not built.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use super::LfsError;
use super::endpoint::Direction;
use crate::events::PushFailureKind;
use crate::remote_url::{self, Transport};

/// The most output read from `git-lfs-authenticate`.
pub const MAX_RESPONSE: usize = 64 << 10;

/// What `git-lfs-authenticate` answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authentication {
    /// The LFS endpoint, without a final `/`.
    pub href: String,
    /// Headers for requests to it (usually `Authorization`).
    pub headers: Vec<(String, String)>,
    /// When the answer stops being valid, if it says.
    pub expires_at: Option<SystemTime>,
}

#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    href: Option<String>,
    #[serde(default)]
    header: BTreeMap<String, String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    expires_at: Option<String>,
}

/// The path git-lfs sends for an SSH URL: the URL's path without a
/// leading `/`.
pub fn lfs_path(url: &str) -> Option<String> {
    let path = remote_url::ssh_path(url)?;
    let path = path.trim_start_matches('/');
    (!path.is_empty()).then(|| path.to_owned())
}

/// `git-lfs-authenticate <path> <operation>`, the path unquoted as git-lfs
/// sends it. A path that a shell would split or interpret is refused
/// (`local`): quoting it would not match what servers expect.
pub fn command_for(path: &str, direction: Direction) -> Result<String, LfsError> {
    let plain = !path.is_empty()
        && path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._/~@+,:=%".contains(&b));
    if !plain {
        return Err(LfsError::local(format!(
            "the repository path {path:?} cannot be sent to git-lfs-authenticate unquoted; set \
             the remote's lfsurl"
        )));
    }
    Ok(format!(
        "git-lfs-authenticate {path} {}",
        direction.operation()
    ))
}

/// Reads `git-lfs-authenticate`'s answer, received at `now`. A missing or
/// non-`http(s)` href, or JSON that cannot be read, is `refused`.
pub fn parse_response(bytes: &[u8], now: SystemTime) -> Result<Authentication, LfsError> {
    let refused = |why: String| {
        LfsError::new(
            PushFailureKind::Refused,
            format!("git-lfs-authenticate answered {why}"),
        )
    };
    let response: Response = serde_json::from_slice(bytes)
        .map_err(|e| refused(format!("something that is not its JSON ({e})")))?;
    let Some(href) = response.href.filter(|h| !h.trim().is_empty()) else {
        return Err(refused("no href".into()));
    };
    let href = href.trim().trim_end_matches('/').to_owned();
    match remote_url::transport(&href) {
        Transport::Http | Transport::Https => {}
        _ => {
            return Err(refused(format!(
                "an href that is not an http(s) URL ({})",
                crate::push::redact_url(&href)
            )));
        }
    }
    let mut expires_at = None;
    if let Some(seconds) = response.expires_in {
        expires_at = Some(now + Duration::from_secs(seconds.max(0) as u64));
    }
    if let Some(at) = &response.expires_at
        && !at.trim().is_empty()
    {
        // git-lfs's zero time means "no expiry".
        let at = parse_rfc3339(at.trim())
            .ok_or_else(|| refused(format!("an expires_at it cannot read ({at:?})")))?;
        if at > UNIX_EPOCH {
            expires_at = Some(expires_at.map_or(at, |e: SystemTime| e.min(at)));
        }
    }
    Ok(Authentication {
        href,
        headers: response.header.into_iter().collect(),
        expires_at,
    })
}

/// An RFC 3339 date-time (`2026-10-13T12:00:00Z`, with optional fractional
/// seconds and a `Z` or `±hh:mm` offset). Times before 1970 are the epoch.
pub fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    let b = text.as_bytes();
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = text.get(range)?;
        (!part.is_empty() && part.bytes().all(|c| c.is_ascii_digit()))
            .then(|| part.parse().ok())
            .flatten()
    };
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    if second > 60 {
        return None;
    }
    let mut i = 19;
    let mut nanos: u64 = 0;
    if b.get(i) == Some(&b'.') {
        let start = i + 1;
        i = start;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return None;
        }
        let digits = &text[start..i.min(start + 9)];
        nanos = digits.parse::<u64>().ok()? * 10u64.pow(9 - digits.len() as u32);
    }
    let offset = match b.get(i)? {
        b'Z' | b'z' if i + 1 == b.len() => 0,
        sign @ (b'+' | b'-') if i + 6 == b.len() && b[i + 3] == b':' => {
            let minutes = num(i + 1..i + 3)? * 60 + num(i + 4..i + 6)?;
            if *sign == b'+' { minutes } else { -minutes }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset * 60;
    if seconds < 0 {
        return Some(UNIX_EPOCH);
    }
    Some(UNIX_EPOCH + Duration::new(seconds as u64, nanos as u32))
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Runs `git-lfs-authenticate` for `direction` on the SSH remote
/// `remote`, over `ssh_url` (its push URL, or its `url` on the same host
/// and port), and reads the answer (see the module documentation).
#[cfg(feature = "ssh")]
pub(crate) fn authenticate(
    remote: &crate::config::RemoteSettings,
    ssh_url: &str,
    path: &str,
    direction: Direction,
) -> Result<Authentication, LfsError> {
    use crate::remote_auth::{AuthState, RemoteAuth};
    let command = command_for(path, direction)?;
    let auth = RemoteAuth::new(remote);
    let Some(ssh) = auth.ssh_remote() else {
        return Err(LfsError::local(format!(
            "remote {} is not an SSH remote",
            remote.name
        )));
    };
    let state = AuthState::new();
    let name = &remote.name;
    let shown = crate::push::redact_url(ssh_url);
    let output = crate::ssh_transport::run_command(ssh, &state, ssh_url, &command, MAX_RESPONSE)
        .map_err(|e| {
            let kind = if state.host_key_failure().is_some() {
                PushFailureKind::UntrustedHost
            } else if state.auth_failed() {
                PushFailureKind::Auth
            } else {
                PushFailureKind::Unreachable
            };
            let why = state
                .host_key_failure()
                .unwrap_or_else(|| e.message().to_owned());
            LfsError::new(
                kind,
                format!("running git-lfs-authenticate over {shown} (remote {name}): {why}"),
            )
        })?;
    match output.status {
        Some(0) => {}
        status => {
            let status = status.map_or("ended without an exit status".to_owned(), |s| {
                format!("exited with status {s}")
            });
            let said = if output.stderr.is_empty() {
                String::new()
            } else {
                format!(": {}", output.stderr)
            };
            return Err(LfsError::new(
                PushFailureKind::Refused,
                format!(
                    "`{command}` on {shown} {status}{said}; the server has no \
                     git-lfs-authenticate: set remote.{name}.lfsurl to its LFS endpoint"
                ),
            ));
        }
    }
    parse_response(&output.stdout, SystemTime::now()).map_err(|mut e| {
        e.message = format!("{} (from {shown}, remote {name})", e.message);
        e
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(seconds: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(seconds)
    }

    #[test]
    fn responses_are_read() {
        let now = at(1_000_000);
        let a = parse_response(
            br#"{"href": "https://lfs.example/r.git/info/lfs/", "header": {"Authorization": "RemoteAuth abc"}, "expires_in": 60}"#,
            now,
        )
        .unwrap();
        assert_eq!(a.href, "https://lfs.example/r.git/info/lfs");
        assert_eq!(
            a.headers,
            [("Authorization".to_owned(), "RemoteAuth abc".to_owned())]
        );
        assert_eq!(a.expires_at, Some(at(1_000_060)));
        // expires_at, Z or an offset, with fractions; the earlier wins.
        let a = parse_response(
            br#"{"href": "http://h/x", "expires_at": "1970-01-12T13:46:40Z"}"#,
            now,
        )
        .unwrap();
        assert_eq!(a.expires_at, Some(at(1_000_000)));
        assert!(a.headers.is_empty());
        let a = parse_response(
            br#"{"href": "http://h/x", "expires_in": 10, "expires_at": "1970-01-12T14:46:40.5+01:00"}"#,
            now,
        )
        .unwrap();
        assert_eq!(
            a.expires_at,
            Some(at(1_000_000) + Duration::from_millis(500))
        );
        // git-lfs's zero time, or nothing: no expiry.
        let a = parse_response(
            br#"{"href": "http://h/x", "expires_at": "0001-01-01T00:00:00Z"}"#,
            now,
        )
        .unwrap();
        assert_eq!(a.expires_at, None);
        for bad in [
            &br#"{"header": {}}"#[..],
            br#"{"href": ""}"#,
            br#"{"href": "ssh://h/x"}"#,
            br#"{"href": "file:///x"}"#,
            br#"{"href": "http://h/x", "expires_at": "tomorrow"}"#,
            b"Welcome to the server\n",
            b"",
        ] {
            let err = parse_response(bad, now).unwrap_err();
            assert_eq!(err.kind, PushFailureKind::Refused, "{err}");
        }
    }

    #[test]
    fn rfc3339() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(UNIX_EPOCH));
        assert_eq!(
            parse_rfc3339("2026-10-13T12:34:56Z"),
            Some(at(1_791_894_896))
        );
        assert_eq!(
            parse_rfc3339("2026-10-13t14:34:56.25+02:00"),
            Some(at(1_791_894_896) + Duration::from_millis(250))
        );
        assert_eq!(
            parse_rfc3339("2026-10-13T07:34:56.123456789123-05:00"),
            Some(at(1_791_894_896) + Duration::from_nanos(123_456_789))
        );
        assert_eq!(parse_rfc3339("2000-02-29T00:00:00Z"), Some(at(951_782_400)));
        for bad in [
            "2026-10-13",
            "2026-10-13T12:34:56",
            "2026-13-13T12:34:56Z",
            "2026-10-13T25:00:00Z",
            "2026-10-13T12:34:56+0200",
            "2026-10-13T12:34:56.Z",
            "2026-10-13T12:34:56Zjunk",
            "x026-10-13T12:34:56Z",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
    }

    #[test]
    fn commands_send_the_path_unquoted() {
        assert_eq!(
            command_for("acme/r.git", Direction::Upload).unwrap(),
            "git-lfs-authenticate acme/r.git upload"
        );
        assert_eq!(
            command_for("home/git/repos/r.git", Direction::Download).unwrap(),
            "git-lfs-authenticate home/git/repos/r.git download"
        );
        for bad in [
            "a b", "a'b", "a;b", "$(x)", "a\nb", "a`b", "a|b", "", "a\"b",
        ] {
            assert_eq!(
                command_for(bad, Direction::Upload).unwrap_err().kind,
                PushFailureKind::Local,
                "{bad:?}"
            );
        }
        assert_eq!(
            lfs_path("ssh://git@h:2222/home/git/r.git").as_deref(),
            Some("home/git/r.git")
        );
        assert_eq!(lfs_path("git@h:acme/r.git").as_deref(), Some("acme/r.git"));
    }
}
