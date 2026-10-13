//! Pure helpers for remote URLs (design §11.1): which transport a URL
//! selects, its host and port, and its userinfo.
//!
//! They follow libgit2's own rules, so that what agentfs validates and binds
//! credentials to is what libgit2 connects to:
//!
//! - `ssh://`, `ssh+git://` and `git+ssh://` (any case) are SSH, and so is
//!   scp-like `[user@]host:path`, which needs a `:` before any `/`, as in
//!   git;
//! - `file://` URLs and paths are local;
//! - `http://` and `https://` are what they say; anything else (`git://`,
//!   unknown schemes) is [`Transport::Other`].
//!
//! libgit2 built with SSH also sends a path that contains a `:` after a `/`
//! (`/srv/a:b.git`) to its SSH transport. Such a path is treated as local
//! here, as git does; with the `ssh` feature its push fails, closed: no
//! credential or key is offered to it and its host key is never trusted.

/// The transport a remote URL selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// A path or a `file://` URL.
    Local,
    Http,
    Https,
    /// `ssh://`, `ssh+git://`, `git+ssh://` or scp-like `[user@]host:path`.
    Ssh,
    /// `git://` and any other scheme.
    Other,
}

/// The transport `url` selects (see the module documentation).
pub fn transport(url: &str) -> Transport {
    if let Some((scheme, _)) = url.split_once("://")
        && is_scheme(scheme)
    {
        return match scheme.to_ascii_lowercase().as_str() {
            "file" => Transport::Local,
            "http" => Transport::Http,
            "https" => Transport::Https,
            "ssh" | "ssh+git" | "git+ssh" => Transport::Ssh,
            _ => Transport::Other,
        };
    }
    if scp_like(url).is_some() {
        Transport::Ssh
    } else {
        Transport::Local
    }
}

/// True for a URL scheme: letters, digits, `+`, `-` and `.`, starting with
/// a letter.
fn is_scheme(s: &str) -> bool {
    s.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

/// The part before the path of a scp-like `[user@]host:path`: a `:` with no
/// `/` before it, and something before the `:`. A bracketed
/// `[user@host:port]:path` or `[::1]:path` keeps its brackets.
fn scp_like(url: &str) -> Option<&str> {
    if url.contains("://") && url.split_once("://").is_some_and(|(s, _)| is_scheme(s)) {
        return None;
    }
    let end = if url.starts_with('[') {
        // `[...]` then `:`; a `/` inside the brackets is not a path.
        let close = url.find(']')?;
        if url.as_bytes().get(close + 1) != Some(&b':') {
            return None;
        }
        close + 1
    } else {
        url.find(':')?
    };
    let before = &url[..end];
    if before.is_empty() || before.contains('/') {
        return None;
    }
    Some(before)
}

/// The lowercased `host[:port]` of a network URL, either
/// `scheme://[userinfo@]host[:port]/…` or scp-like `[user@]host:path`.
/// `None` for local paths and `file://` URLs.
pub fn authority(url: &str) -> Option<String> {
    let hostport = if let Some((scheme, rest)) = url.split_once("://") {
        if scheme.eq_ignore_ascii_case("file") {
            return None;
        }
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let auth = &rest[..end];
        auth.rsplit_once('@').map_or(auth, |(_, h)| h)
    } else {
        // scp-like syntax needs a `:` before any `/`, as in git.
        let (before, _) = url.split_once(':')?;
        if before.is_empty() || before.contains('/') {
            return None;
        }
        before.rsplit_once('@').map_or(before, |(_, h)| h)
    };
    (!hostport.is_empty()).then(|| hostport.to_ascii_lowercase())
}

/// Where an SSH remote connects: the host (lowercased, IPv6 brackets
/// stripped) and the port, if the URL names one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: Option<u16>,
}

/// The endpoint of an SSH URL; `None` for any other transport or a URL
/// without a host or with an invalid port.
pub fn ssh_endpoint(url: &str) -> Option<Endpoint> {
    if transport(url) != Transport::Ssh {
        return None;
    }
    let hostport = match url.split_once("://") {
        Some((_, rest)) => {
            let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            let auth = &rest[..end];
            auth.rsplit_once('@').map_or(auth, |(_, h)| h)
        }
        None => {
            let before = scp_like(url)?;
            match before.strip_prefix('[').and_then(|b| b.strip_suffix(']')) {
                // `[::1]`: an IPv6 address alone.
                Some(inner) if inner.contains("::") || inner.matches(':').count() > 1 => before,
                // `[user@host:port]`.
                Some(inner) => inner.rsplit_once('@').map_or(inner, |(_, h)| h),
                None => before.rsplit_once('@').map_or(before, |(_, h)| h),
            }
        }
    };
    let (host, port) = split_host_port(hostport)?;
    let host = host.to_ascii_lowercase();
    (!host.is_empty()).then_some(Endpoint { host, port })
}

/// The repository path of an SSH URL, as the remote command gets it
/// (design §11.1): the percent-decoded path of `ssh://host/path`, without
/// its leading `/` when it starts with `/~` (a home directory, as git
/// does), or the `path` of scp-like `host:path` as written. `None` for any
/// other transport or an empty path.
pub fn ssh_path(url: &str) -> Option<String> {
    if transport(url) != Transport::Ssh {
        return None;
    }
    let path = match url.split_once("://") {
        Some((_, rest)) => {
            let start = rest.find('/')?;
            let end = rest.find(['?', '#']).unwrap_or(rest.len()).max(start);
            let path = percent_decode(&rest[start..end]);
            match path.strip_prefix('/') {
                Some(home) if home.starts_with('~') => home.to_owned(),
                _ => path,
            }
        }
        None => url[scp_like(url)?.len() + 1..].to_owned(),
    };
    (!path.is_empty() && path != "/").then_some(path)
}

/// `host`, `host:port`, `[v6]` or `[v6]:port`. `None` for a bad port.
fn split_host_port(s: &str) -> Option<(&str, Option<u16>)> {
    if let Some(rest) = s.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        return match after {
            "" => Some((host, None)),
            _ => Some((host, Some(after.strip_prefix(':')?.parse().ok()?))),
        };
    }
    match s.rsplit_once(':') {
        // An empty port (`host:`) means the default, as in URLs.
        Some((host, "")) => Some((host, None)),
        Some((host, port)) => Some((host, Some(port.parse().ok()?))),
        None => Some((s, None)),
    }
}

/// The userinfo of a URL: `user[:password]` of `scheme://userinfo@host/…`,
/// or `user` of scp-like `user@host:path`.
fn userinfo(url: &str) -> Option<&str> {
    let info = match url.split_once("://") {
        Some((_, rest)) => {
            let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            rest[..end].rsplit_once('@')?.0
        }
        None => {
            let before = scp_like(url)?;
            let before = before
                .strip_prefix('[')
                .and_then(|b| b.strip_suffix(']'))
                .unwrap_or(before);
            before.rsplit_once('@')?.0
        }
    };
    Some(info).filter(|i| !i.is_empty())
}

/// The username a URL names, if any.
pub fn url_username(url: &str) -> Option<&str> {
    let info = userinfo(url)?;
    let user = info.split_once(':').map_or(info, |(u, _)| u);
    Some(user).filter(|u| !u.is_empty())
}

/// True if the URL carries a password (`scheme://user:password@host/…`).
pub fn has_password(url: &str) -> bool {
    url.contains("://") && userinfo(url).is_some_and(|i| i.contains(':'))
}

/// The password of a `scheme://user:password@host/…` URL, percent-decoded.
pub fn url_password(url: &str) -> Option<String> {
    if !url.contains("://") {
        return None;
    }
    let (_, password) = userinfo(url)?.split_once(':')?;
    Some(percent_decode(password)).filter(|p| !p.is_empty())
}

/// `url` without the userinfo of a `scheme://userinfo@host/…` URL.
pub fn without_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    match rest[..end].rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => url.to_owned(),
    }
}

/// The origin of an `http(s)://` URL: its lowercased scheme and authority,
/// without the scheme's default port. `None` for anything else.
pub fn origin(url: &str) -> Option<(String, String)> {
    let scheme = match transport(url) {
        Transport::Http => "http",
        Transport::Https => "https",
        _ => return None,
    };
    let authority = authority(url)?;
    let default = if scheme == "https" { ":443" } else { ":80" };
    let authority = authority
        .strip_suffix(default)
        .map_or(authority.clone(), str::to_owned);
    Some((scheme.to_owned(), authority))
}

/// `%XX` escapes decoded; anything malformed is kept as it is.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && let (Some(Some(h)), Some(Some(l))) = (
                bytes.get(i + 1).map(|b| hex(*b)),
                bytes.get(i + 2).map(|b| hex(*b)),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorities() {
        for (url, expected) in [
            ("https://github.com/o/r", Some("github.com")),
            ("https://u:p@Example.COM:8443/r", Some("example.com:8443")),
            ("ssh://git@host.example/r.git", Some("host.example")),
            ("git@github.com:o/r.git", Some("github.com")),
            ("host.example:r.git", Some("host.example")),
            ("https://host?x=1", Some("host")),
            ("file:///x", None),
            ("FILE:///x", None),
            ("/x", None),
            ("./a:b", None),
            ("rel/dir:x", None),
            ("https:///nohost", None),
        ] {
            assert_eq!(authority(url).as_deref(), expected, "{url}");
        }
    }

    #[test]
    fn transports() {
        use Transport::*;
        for (url, expected) in [
            ("ssh://git@Host:2222/p", Ssh),
            ("SSH://host/p", Ssh),
            ("git+ssh://host/p", Ssh),
            ("ssh+git://host/p", Ssh),
            ("Git+SSH://host/p", Ssh),
            ("git@github.com:o/r.git", Ssh),
            ("host:path", Ssh),
            ("host.example:r.git", Ssh),
            ("[git@host:2222]:r.git", Ssh),
            ("[::1]:r.git", Ssh),
            ("./a:b", Local),
            ("rel/dir:x", Local),
            ("/abs", Local),
            ("/srv/a:b.git", Local),
            ("relative", Local),
            ("file:///x", Local),
            ("FILE:///x", Local),
            ("https://h/x", Https),
            ("HTTPS://h/x", Https),
            ("http://h/x", Http),
            ("git://h/x", Other),
            ("foo://h/x", Other),
        ] {
            assert_eq!(transport(url), expected, "{url}");
        }
    }

    #[test]
    fn endpoints() {
        let ep = |host: &str, port: Option<u16>| {
            Some(Endpoint {
                host: host.into(),
                port,
            })
        };
        for (url, expected) in [
            ("ssh://git@Host:2222/p", ep("host", Some(2222))),
            ("ssh://host/p", ep("host", None)),
            ("ssh://host:/p", ep("host", None)),
            ("git+ssh://u@h.example:22/p", ep("h.example", Some(22))),
            ("ssh+git://h", ep("h", None)),
            ("ssh://[::1]:2222/x", ep("::1", Some(2222))),
            ("ssh://u@[fe80::1]/x", ep("fe80::1", None)),
            ("git@GitHub.com:o/r.git", ep("github.com", None)),
            ("host:path", ep("host", None)),
            ("[git@host:2222]:r.git", ep("host", Some(2222))),
            ("[::1]:r.git", ep("::1", None)),
            ("ssh://host:notaport/p", None),
            ("ssh://host:99999/p", None),
            ("ssh:///p", None),
            ("https://host/p", None),
            ("/abs", None),
            ("file:///x", None),
        ] {
            assert_eq!(ssh_endpoint(url), expected, "{url}");
        }
    }

    #[test]
    fn ssh_paths() {
        for (url, expected) in [
            ("ssh://git@host:2222/srv/r.git", Some("/srv/r.git")),
            ("ssh://host/~/r.git", Some("~/r.git")),
            ("ssh://host/~alice/r.git", Some("~alice/r.git")),
            ("ssh://host/a%20b.git", Some("/a b.git")),
            ("git+ssh://h/p?x#y", Some("/p")),
            ("git@github.com:o/r.git", Some("o/r.git")),
            ("host:/abs/r.git", Some("/abs/r.git")),
            ("[git@host:2222]:r.git", Some("r.git")),
            ("[::1]:r.git", Some("r.git")),
            ("ssh://host", None),
            ("ssh://host/", None),
            ("host:", None),
            ("/srv/a:b.git", None),
            ("https://h/p", None),
        ] {
            assert_eq!(ssh_path(url).as_deref(), expected, "{url}");
        }
    }

    #[test]
    fn userinfo_parts() {
        assert_eq!(url_username("ssh://git@h/p"), Some("git"));
        assert_eq!(url_username("ssh://u:p@h/p"), Some("u"));
        assert_eq!(url_username("git@h:p"), Some("git"));
        assert_eq!(url_username("[deploy@h:2222]:p"), Some("deploy"));
        assert_eq!(url_username("ssh://h/p"), None);
        assert_eq!(url_username("ssh://@h/p"), None);
        assert_eq!(url_username("h:p"), None);
        assert_eq!(url_username("/a@b"), None);
        assert_eq!(url_username("https://h/a@b"), None);
        assert!(has_password("ssh://u:p@h/p"));
        assert!(has_password("https://u:@h/p"));
        assert!(!has_password("ssh://u@h/p"));
        assert!(!has_password("git@h:p"));
        assert!(!has_password("/srv/u:p@x"));
    }

    #[test]
    fn lfs_url_helpers() {
        assert_eq!(
            url_password("https://u:p%40ss@h/r").as_deref(),
            Some("p@ss")
        );
        assert_eq!(url_password("https://u@h/r"), None);
        assert_eq!(url_password("u:p@h:r"), None);
        assert_eq!(without_userinfo("https://u:p@h:1/r@x"), "https://h:1/r@x");
        assert_eq!(without_userinfo("/srv/r.git"), "/srv/r.git");
        let o = |u: &str| origin(u).map(|(s, a)| format!("{s} {a}"));
        assert_eq!(
            o("HTTPS://Git.Example:443/r").as_deref(),
            Some("https git.example")
        );
        assert_eq!(o("http://h:80/r").as_deref(), Some("http h"));
        assert_eq!(o("http://h:8080/r").as_deref(), Some("http h:8080"));
        assert_eq!(origin("https://h/r"), Some(("https".into(), "h".into())));
        assert_ne!(o("http://h/r"), o("https://h/r"));
        assert_eq!(o("ssh://h/r"), None);
        assert_eq!(o("/srv/x"), None);
    }
}
