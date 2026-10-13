//! HTTP proxies for remote traffic (design §11.1), from git's own keys in
//! the state area's config: `http.proxy` for every remote and
//! `remote.<name>.proxy`, which overrides it for one remote (an empty
//! value means no proxy for that remote). Proxy environment variables
//! (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`) are never read:
//! a per-remote override takes `NO_PROXY`'s place.
//!
//! A proxy is a plain HTTP proxy, `http://host[:port]` (git's syntax; a
//! bare `host[:port]` means the same, and the port defaults to 1080, as in
//! git). It carries libgit2's `https` pushes, listings and patch fetches
//! and every LFS request of the remote, tunnelled with `CONNECT`, so TLS
//! is still verified end to end. A plain `http://` git remote connects
//! directly: libgit2 (1.9) writes a proxy's absolute-form request but
//! opens the connection to the server itself, so it is not given the
//! proxy at all. SSH remotes' git traffic never goes through it. Refused, so a proxy secret never sits in config:
//! a user or password (proxy authentication is not supported), and
//! `https://` or SOCKS proxies. A path or query is refused too.

use std::fmt;

/// The port a proxy URL without one uses, as in git.
pub const DEFAULT_PORT: u16 = 1080;

/// A remote's effective proxy, and the key it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxySetting {
    /// `http://host:port`.
    pub url: String,
    /// `http.proxy` or `remote.<name>.proxy`.
    pub key: String,
}

impl fmt::Display for ProxySetting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.url, self.key)
    }
}

/// Parses a proxy setting: `None` for an empty value (no proxy), else the
/// normalised `http://host:port`, or why it is refused.
pub fn parse(value: &str) -> Result<Option<String>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let rest = match value.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") => rest,
        Some((scheme, _)) => {
            return Err(format!(
                "{scheme}:// proxies are not supported; use a plain http:// proxy (it tunnels \
                 https with CONNECT)"
            ));
        }
        None => value,
    };
    let authority = rest.strip_suffix('/').unwrap_or(rest);
    if authority.contains('@') {
        return Err(
            "a proxy user or password is not supported (proxy authentication is not built); \
             use a proxy that needs none"
                .into(),
        );
    }
    if authority.contains(['/', '?', '#']) {
        return Err("a proxy URL is http://host[:port], without a path".into());
    }
    let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((host, after)) = bracketed.split_once(']') else {
            return Err("an IPv6 proxy host needs its closing ]".into());
        };
        let port = match after {
            "" => None,
            p => Some(
                p.strip_prefix(':')
                    .ok_or("unexpected text after the IPv6 host")?,
            ),
        };
        (format!("[{host}]"), port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_owned(), Some(port)),
            None => (authority.to_owned(), None),
        }
    };
    let valid_host = !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-_[]:".contains(&b));
    if !valid_host {
        return Err(format!("{host:?} is not a host name"));
    }
    let port = match port {
        None => DEFAULT_PORT,
        Some(p) => match p.parse::<u16>() {
            Ok(p) if p > 0 => p,
            _ => return Err(format!("{p:?} is not a port")),
        },
    };
    Ok(Some(format!("http://{host}:{port}")))
}

/// `value` with any userinfo replaced by `***`, for messages: a proxy
/// setting with a password is refused, and the password never echoed.
pub fn redact(value: &str) -> String {
    let (scheme, rest) = match value.split_once("://") {
        Some((scheme, rest)) => (format!("{scheme}://"), rest),
        None => (String::new(), value),
    };
    match rest.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}***@{host}"),
        None => value.to_owned(),
    }
}

/// Whether libgit2 sends a connection to `url` through a proxy: `https`
/// only (see the module documentation).
pub fn git_uses_proxy(url: &str) -> bool {
    crate::remote_url::transport(url) == crate::remote_url::Transport::Https
}

/// libgit2's proxy options for a connection to `url` with `proxy`: the
/// proxy for an `https` URL, none for any other (plain `http`, SSH's own
/// transport, local paths). Without a proxy, libgit2 uses none and reads
/// no environment.
pub(crate) fn git_options<'a>(
    proxy: Option<&'a ProxySetting>,
    url: &str,
) -> git2::ProxyOptions<'a> {
    let mut options = git2::ProxyOptions::new();
    if let Some(proxy) = proxy
        && git_uses_proxy(url)
    {
        options.url(&proxy.url);
    }
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxies_are_plain_http_without_credentials() {
        for (value, want) in [
            ("http://proxy.example:3128", "http://proxy.example:3128"),
            ("http://proxy.example:3128/", "http://proxy.example:3128"),
            ("HTTP://proxy.example", "http://proxy.example:1080"),
            ("proxy.example:8080", "http://proxy.example:8080"),
            ("10.0.0.1", "http://10.0.0.1:1080"),
            ("http://[::1]:3128", "http://[::1]:3128"),
            ("[fd00::1]", "http://[fd00::1]:1080"),
        ] {
            assert_eq!(parse(value).unwrap().as_deref(), Some(want), "{value}");
        }
        assert_eq!(redact("http://u:pw@p:1"), "http://***@p:1");
        assert_eq!(redact("u:pw@p:1"), "***@p:1");
        assert_eq!(redact("p:1"), "p:1");
        assert_eq!(parse("").unwrap(), None);
        assert!(git_uses_proxy("https://h/r.git"));
        for url in ["http://h/r.git", "ssh://h/r.git", "/srv/r.git"] {
            assert!(!git_uses_proxy(url), "{url}");
        }
        assert_eq!(parse("  ").unwrap(), None);
        for (value, why) in [
            ("https://proxy.example:3128", "https:// proxies"),
            ("socks5://proxy.example:1080", "socks5:// proxies"),
            ("http://user:pw@proxy.example:3128", "user or password"),
            ("user@proxy.example", "user or password"),
            ("http://proxy.example:3128/path", "without a path"),
            ("http://proxy.example:3128?x=1", "without a path"),
            ("http://proxy.example:port", "not a port"),
            ("http://proxy.example:0", "not a port"),
            ("http://:3128", "not a host name"),
            ("http://[::1", "closing ]"),
        ] {
            let err = parse(value).unwrap_err();
            assert!(err.contains(why), "{value}: {err}");
            assert!(!err.contains("pw"), "{value}: {err}");
        }
    }
}
