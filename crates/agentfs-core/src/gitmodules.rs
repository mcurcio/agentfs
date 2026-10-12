//! git fsck's `.gitmodules` content rules (design §3.4).
//!
//! A remote with `receive.fsckObjects` refuses a push whose history holds a
//! hostile `.gitmodules`, so agentfs refuses such content before it is
//! recorded, as it does git's other fsck rules ([`crate::checks`]).
//!
//! This is a port of the parts of git that fsck uses, kept close to git's C
//! so its behaviour can be compared line by line, and checked against the
//! `git` binary by `tests/gitmodules_git.rs`:
//!
//! - **The config parser** (`config.c`: `git_parse_source`, `get_base_var`,
//!   `get_extended_base_var`, `get_value`, `parse_value`): a UTF-8 BOM,
//!   CR LF line ends, `#` and `;` comments, `[section "subsection"]` with
//!   `\` escapes in the subsection, the legacy `[section.subsection]` form
//!   (lowercased), keys of alphanumerics and `-` starting with a letter,
//!   quoted values, the escapes `\n \t \b \\ \"`, backslash-newline
//!   continuation, whitespace trimming and value-less (boolean) keys. Names
//!   and values are used as C strings, so a NUL byte ends them, as in git.
//! - **The fsck callback** (`fsck.c`: `fsck_gitmodules_fn`): for every
//!   `submodule.<name>.<key>` entry, the name (`gitmodulesName`), `url`
//!   (`gitmodulesUrl`), `path` (`gitmodulesPath`) and `update`
//!   (`gitmodulesUpdate`). A parse error is `gitmodulesParse`; entries
//!   before it are still checked, as in git.
//! - **URL checks** (`check_submodule_url`, `url_decode`, and urlmatch.c's
//!   `url_normalize`): options, decoded newlines, `../` escaping the root of
//!   a relative URL, and full normalization of the URLs git-remote-curl
//!   would be given.
//!
//! `gitmodulesLarge` (over git's `core.bigFileThreshold`) is a size rule
//! and lives in [`crate::checks`].

use std::fmt;

/// One `name=value` as git's parser hands it to a callback: the full
/// variable name (`section.subsection.key`, section and key lowercased)
/// and the value, or `None` for a key without `=`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigEntry {
    pub name: Vec<u8>,
    pub value: Option<Vec<u8>>,
}

/// Where parsing stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseError {
    /// 1-based line of the error.
    pub line: usize,
}

/// What [`parse`] read: every entry before the first error, and the error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub entries: Vec<ConfigEntry>,
    pub error: Option<ParseError>,
}

/// The first rule a `.gitmodules` breaks, in the order git's fsck reports
/// them: each entry's name, url, path and update in file order, then a
/// parse error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// `gitmodulesParse`.
    Parse { line: usize },
    /// `gitmodulesName`.
    Name(String),
    /// `gitmodulesUrl`.
    Url(String),
    /// `gitmodulesPath`.
    Path(String),
    /// `gitmodulesUpdate`.
    Update(String),
}

impl Problem {
    /// git fsck's message id.
    pub fn fsck_id(&self) -> &'static str {
        match self {
            Self::Parse { .. } => "gitmodulesParse",
            Self::Name(_) => "gitmodulesName",
            Self::Url(_) => "gitmodulesUrl",
            Self::Path(_) => "gitmodulesPath",
            Self::Update(_) => "gitmodulesUpdate",
        }
    }
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse { line } => write!(f, "could not parse .gitmodules (line {line})"),
            Self::Name(name) => write!(f, "disallowed submodule name: {name:?}"),
            Self::Url(url) => write!(f, "disallowed submodule url: {url:?}"),
            Self::Path(path) => write!(f, "disallowed submodule path: {path:?}"),
            Self::Update(value) => write!(f, "disallowed submodule update setting: {value:?}"),
        }
    }
}

/// Checks `.gitmodules` content as git's fsck does.
pub fn check(content: &[u8]) -> Result<(), Problem> {
    let parsed = parse(content);
    for entry in &parsed.entries {
        check_entry(entry)?;
    }
    match parsed.error {
        Some(e) => Err(Problem::Parse { line: e.line }),
        None => Ok(()),
    }
}

/// `fsck_gitmodules_fn`.
fn check_entry(entry: &ConfigEntry) -> Result<(), Problem> {
    let name = c_str(&entry.name);
    let Some((subsection, key)) = submodule_key(name) else {
        return Ok(());
    };
    if !check_submodule_name(subsection) {
        return Err(Problem::Name(lossy(subsection)));
    }
    let Some(value) = entry.value.as_deref().map(c_str) else {
        return Ok(());
    };
    match key {
        b"url" if !check_submodule_url(value) => Err(Problem::Url(lossy(value))),
        b"path" if value.first() == Some(&b'-') => Err(Problem::Path(lossy(value))),
        b"update" if value.first() == Some(&b'!') => Err(Problem::Update(lossy(value))),
        _ => Ok(()),
    }
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The bytes up to the first NUL: what git's C code sees.
fn c_str(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|&b| b == 0) {
        Some(i) => &bytes[..i],
        None => bytes,
    }
}

/// `parse_config_key(var, "submodule", ...)`: the subsection and key of a
/// `submodule.<subsection>.<key>` name, or `None` when the section differs
/// or there is no subsection.
fn submodule_key(var: &[u8]) -> Option<(&[u8], &[u8])> {
    let rest = var.strip_prefix(b"submodule")?;
    if rest.first() != Some(&b'.') {
        return None;
    }
    let dot = rest.iter().rposition(|&b| b == b'.')?;
    if dot == 0 {
        return None;
    }
    Some((&rest[1..dot], &rest[dot + 1..]))
}

/// git's `isspace`: space, tab, LF and CR only.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

/// git's `iskeychar`.
fn is_keychar(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-'
}

/// git's config source over a buffer (`config_buf_fgetc` and
/// `get_next_char`).
struct Source<'a> {
    bytes: &'a [u8],
    pos: usize,
    eof: bool,
}

impl Source<'_> {
    fn fgetc(&mut self) -> Option<u8> {
        let c = self.bytes.get(self.pos).copied();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    /// `get_next_char`: CR LF reads as LF, and the end reads as LF with
    /// `eof` set.
    fn next(&mut self) -> u8 {
        let mut c = self.fgetc();
        if c == Some(b'\r') {
            match self.fgetc() {
                Some(b'\n') => c = Some(b'\n'),
                Some(_) => self.pos -= 1,
                None => {}
            }
        }
        c.unwrap_or_else(|| {
            self.eof = true;
            b'\n'
        })
    }

    /// The line of the last byte read: a line end belongs to the line it
    /// ends.
    fn line(&self) -> usize {
        let read = &self.bytes[..self.pos];
        let ends = read.iter().filter(|&&b| b == b'\n').count();
        1 + ends - usize::from(read.last() == Some(&b'\n'))
    }
}

/// Parses `content` as git's config parser does (no includes).
pub fn parse(content: &[u8]) -> Parsed {
    let mut src = Source {
        bytes: content,
        pos: 0,
        eof: false,
    };
    let mut entries = Vec::new();
    let error = parse_source(&mut src, &mut entries)
        .err()
        .map(|()| ParseError { line: src.line() });
    Parsed { entries, error }
}

const UTF8_BOM: [u8; 3] = [0xef, 0xbb, 0xbf];

/// `git_parse_source`.
fn parse_source(src: &mut Source<'_>, entries: &mut Vec<ConfigEntry>) -> Result<(), ()> {
    let mut comment = false;
    let mut var: Vec<u8> = Vec::new();
    let mut baselen = 0;
    let mut bom = Some(0);
    loop {
        let c = src.next();
        if let Some(i) = bom
            && i < UTF8_BOM.len()
        {
            if c == UTF8_BOM[i] {
                bom = Some(i + 1);
                continue;
            }
            // A partial BOM is not tolerated.
            if i != 0 {
                return Err(());
            }
            bom = None;
        }
        if c == b'\n' {
            if src.eof {
                return Ok(());
            }
            comment = false;
            continue;
        }
        if comment || is_space(c) {
            continue;
        }
        if c == b'#' || c == b';' {
            comment = true;
            continue;
        }
        if c == b'[' {
            var.clear();
            get_base_var(src, &mut var)?;
            if var.is_empty() {
                return Err(());
            }
            var.push(b'.');
            baselen = var.len();
            continue;
        }
        if !c.is_ascii_alphabetic() {
            return Err(());
        }
        var.truncate(baselen);
        var.push(c.to_ascii_lowercase());
        let value = get_value(src, &mut var)?;
        entries.push(ConfigEntry {
            name: var.clone(),
            value,
        });
    }
}

/// `get_base_var`: a section header after its `[`.
fn get_base_var(src: &mut Source<'_>, name: &mut Vec<u8>) -> Result<(), ()> {
    loop {
        let c = src.next();
        if src.eof {
            return Err(());
        }
        if c == b']' {
            return Ok(());
        }
        if is_space(c) {
            return get_extended_base_var(src, name, c);
        }
        if !is_keychar(c) && c != b'.' {
            return Err(());
        }
        name.push(c.to_ascii_lowercase());
    }
}

/// `get_extended_base_var`: `[section "subsection"]`, case kept.
fn get_extended_base_var(src: &mut Source<'_>, name: &mut Vec<u8>, mut c: u8) -> Result<(), ()> {
    loop {
        if c == b'\n' {
            return Err(());
        }
        c = src.next();
        if !is_space(c) {
            break;
        }
    }
    if c != b'"' {
        return Err(());
    }
    name.push(b'.');
    loop {
        let mut c = src.next();
        if c == b'\n' {
            return Err(());
        }
        if c == b'"' {
            break;
        }
        if c == b'\\' {
            c = src.next();
            if c == b'\n' {
                return Err(());
            }
        }
        name.push(c);
    }
    if src.next() != b']' {
        return Err(());
    }
    Ok(())
}

/// `get_value`: the rest of a key, then `= value` or nothing.
fn get_value(src: &mut Source<'_>, name: &mut Vec<u8>) -> Result<Option<Vec<u8>>, ()> {
    let mut c;
    loop {
        c = src.next();
        if src.eof || !is_keychar(c) {
            break;
        }
        name.push(c.to_ascii_lowercase());
    }
    while c == b' ' || c == b'\t' {
        c = src.next();
    }
    if c == b'\n' {
        return Ok(None);
    }
    if c != b'=' {
        return Err(());
    }
    parse_value(src).map(Some)
}

/// `parse_value`.
fn parse_value(src: &mut Source<'_>) -> Result<Vec<u8>, ()> {
    let mut quote = false;
    let mut comment = false;
    let mut trim_len = 0;
    let mut value = Vec::new();
    loop {
        let c = src.next();
        if c == b'\n' {
            if quote {
                return Err(());
            }
            if trim_len > 0 {
                value.truncate(trim_len);
            }
            return Ok(value);
        }
        if comment {
            continue;
        }
        if is_space(c) && !quote {
            if trim_len == 0 {
                trim_len = value.len();
            }
            if !value.is_empty() {
                value.push(c);
            }
            continue;
        }
        if !quote && (c == b';' || c == b'#') {
            comment = true;
            continue;
        }
        trim_len = 0;
        if c == b'\\' {
            let escaped = match src.next() {
                b'\n' => continue,
                b't' => b'\t',
                b'b' => 0x08,
                b'n' => b'\n',
                c @ (b'\\' | b'"') => c,
                _ => return Err(()),
            };
            value.push(escaped);
            continue;
        }
        if c == b'"' {
            quote = !quote;
            continue;
        }
        value.push(c);
    }
}

fn is_xplatform_dir_sep(c: u8) -> bool {
    c == b'/' || c == b'\\'
}

/// `check_submodule_name`: not empty, and no `..` component (split on `/`
/// and `\`).
pub fn check_submodule_name(name: &[u8]) -> bool {
    !name.is_empty()
        && !name
            .split(|&c| is_xplatform_dir_sep(c))
            .any(|component| component == b"..")
}

fn starts_with_dot_slash(url: &[u8]) -> bool {
    url.len() >= 2 && url[0] == b'.' && is_xplatform_dir_sep(url[1])
}

fn starts_with_dot_dot_slash(url: &[u8]) -> bool {
    url.len() >= 3 && url[0] == b'.' && url[1] == b'.' && is_xplatform_dir_sep(url[2])
}

/// How git checks the URLs git-remote-curl would be given. git changed
/// this in 2.40; a remote may run either, so [`check_submodule_url`]
/// refuses what either refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurlUrlCheck {
    /// git 2.26.1 to 2.39: `credential_from_url_gently` must parse it, with
    /// a host and no newline in any decoded component.
    Credential,
    /// git 2.40 and later: urlmatch.c's `url_normalize` must accept it, and
    /// the decoded normalized URL must hold no newline.
    Normalize,
}

/// `check_submodule_url`, refusing what any git version since 2.26.1
/// refuses.
pub fn check_submodule_url(url: &[u8]) -> bool {
    check_submodule_url_as(url, CurlUrlCheck::Credential)
        && check_submodule_url_as(url, CurlUrlCheck::Normalize)
}

/// `check_submodule_url` as the git versions of `curl` implement it.
pub fn check_submodule_url_as(url: &[u8], curl: CurlUrlCheck) -> bool {
    let url = c_str(url);
    if url.first() == Some(&b'-') {
        return false;
    }
    if starts_with_dot_slash(url) || starts_with_dot_dot_slash(url) || url.starts_with(b"git://") {
        // Could be appended to an http URL and decoded.
        if url_decode(url).contains(&b'\n') {
            return false;
        }
        // `../` escaping the root can rewrite the host (CVE-2020-11008).
        let mut rest = url;
        let mut dotdots = 0;
        loop {
            if starts_with_dot_dot_slash(rest) {
                dotdots += 1;
                rest = &rest[3..];
            } else if starts_with_dot_slash(rest) {
                rest = &rest[2..];
            } else {
                break;
            }
        }
        return !(dotdots > 0 && matches!(rest.first(), Some(b':' | b'/')));
    }
    if let Some(curl_url) = url_to_curl_url(url) {
        return match curl {
            CurlUrlCheck::Normalize => match url_normalize(curl_url) {
                Some(normalized) => !url_decode(&normalized).contains(&b'\n'),
                None => false,
            },
            CurlUrlCheck::Credential => credential_from_url(curl_url),
        };
    }
    true
}

/// git 2.39's `credential_from_url_gently(c, url, quiet)` followed by
/// `!*c.host`: the URL has a scheme and a host, and no decoded component
/// (user, password, protocol, host, path) holds a newline.
fn credential_from_url(url: &[u8]) -> bool {
    let Some(proto_end) = url.windows(3).position(|w| w == b"://") else {
        return false;
    };
    if proto_end == 0 {
        return false;
    }
    let cp = &url[proto_end + 3..];
    let at = cp.iter().position(|&b| b == b'@');
    let colon = cp.iter().position(|&b| b == b':');
    let slash = strcspn(cp, b"/?#");
    let mut components: Vec<Vec<u8>> = vec![url[..proto_end].to_vec()];
    let host_start = match at {
        Some(at) if at < slash => {
            match colon {
                Some(colon) if colon < at => {
                    components.push(url_decode(&cp[..colon]));
                    components.push(url_decode(&cp[colon + 1..at]));
                }
                _ => components.push(url_decode(&cp[..at])),
            }
            at + 1
        }
        _ => 0,
    };
    let host = url_decode(&cp[host_start..slash]);
    let mut path = &cp[slash..];
    while path.first() == Some(&b'/') {
        path = &path[1..];
    }
    if !path.is_empty() {
        components.push(url_decode(path));
    }
    !host.is_empty() && !host.contains(&b'\n') && !components.iter().any(|c| c.contains(&b'\n'))
}

/// `url_to_curl_url`: the URL git-remote-curl would be given, if any.
fn url_to_curl_url(url: &[u8]) -> Option<&[u8]> {
    for prefix in [&b"http::"[..], b"https::", b"ftp::", b"ftps::"] {
        if let Some(rest) = url.strip_prefix(prefix) {
            return Some(rest);
        }
    }
    [&b"http://"[..], b"https://", b"ftp://", b"ftps://"]
        .iter()
        .any(|p| url.starts_with(p))
        .then_some(url)
}

fn hexval(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// git's `hex2chr` over the two bytes at `s`, if both are hex digits.
fn hex2chr(s: &[u8]) -> Option<u8> {
    Some((hexval(*s.first()?)? << 4) | hexval(*s.get(1)?)?)
}

/// `url_decode`: everything after the first `:` (if it is not the first
/// byte) has `%XX` decoded, except `%00`.
fn url_decode(url: &[u8]) -> Vec<u8> {
    let url = c_str(url);
    let mut out = Vec::with_capacity(url.len());
    let mut q = match url.iter().position(|&b| b == b':') {
        Some(colon) if colon > 0 => {
            out.extend_from_slice(&url[..colon]);
            &url[colon..]
        }
        _ => url,
    };
    while let Some(&c) = q.first() {
        if c == b'%'
            && q.len() >= 3
            && let Some(val) = hex2chr(&q[1..])
            && val > 0
        {
            out.push(val);
            q = &q[3..];
            continue;
        }
        out.push(c);
        q = &q[1..];
    }
    out
}

const URL_SCHEME_CHARS: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+.-";
const URL_HOST_CHARS: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789.-_[:]";
const URL_UNSAFE_CHARS: &[u8] = b" <>\"%{}|\\^`";
const URL_RESERVED: &[u8] = b":/?#[]@!$&'()*+,;=";

fn strspn(s: &[u8], accept: &[u8]) -> usize {
    s.iter().take_while(|c| accept.contains(c)).count()
}

fn strcspn(s: &[u8], reject: &[u8]) -> usize {
    s.iter().take_while(|c| !reject.contains(c)).count()
}

/// `append_normalized_escapes` with an empty `esc_extra` and
/// [`URL_RESERVED`] as `esc_ok`. `None` for an invalid `%` escape.
fn append_normalized_escapes(buf: &mut Vec<u8>, mut from: &[u8]) -> Option<()> {
    while let Some(&first) = from.first() {
        from = &from[1..];
        let mut ch = first;
        let mut was_esc = false;
        if ch == b'%' {
            if from.len() < 2 {
                return None;
            }
            ch = hex2chr(from)?;
            from = &from[2..];
            was_esc = true;
        }
        if ch <= 0x1f
            || ch >= 0x7f
            || URL_UNSAFE_CHARS.contains(&ch)
            || (was_esc && URL_RESERVED.contains(&ch))
        {
            buf.extend_from_slice(format!("%{ch:02X}").as_bytes());
        } else {
            buf.push(ch);
        }
    }
    Some(())
}

/// urlmatch.c's `url_normalize` (without globs): the normalized URL, or
/// `None` where git finds the URL invalid.
fn url_normalize(url: &[u8]) -> Option<Vec<u8>> {
    let mut url = c_str(url);
    // The lowercased scheme and `://`; no escapes.
    let spanned = strspn(url, URL_SCHEME_CHARS);
    if spanned == 0
        || !url[0].is_ascii_alphabetic()
        || spanned + 3 > url.len()
        || &url[spanned..spanned + 3] != b"://"
    {
        return None;
    }
    let mut norm: Vec<u8> = url[..spanned + 3].to_ascii_lowercase();
    url = &url[spanned + 3..];

    // user:password@, with escapes normalized.
    let slash = strcspn(url, b"/?#");
    if let Some(at) = url.iter().position(|&b| b == b'@')
        && at < slash
    {
        if at > 0 {
            append_normalized_escapes(&mut norm, &url[..at])?;
        }
        norm.push(b'@');
        url = &url[at + 1..];
    }
    // `slash` is now relative to the new start.
    let slash = strcspn(url, b"/?#");

    // The host, without a port; no escapes.
    let mut host_off = 0;
    if url.is_empty() || b":/?#".contains(&url[0]) {
        if !norm.starts_with(b"file:") {
            return None;
        }
    } else {
        host_off = norm.len();
    }
    let mut colon = slash.saturating_sub(1);
    while colon > 0 && url[colon] != b':' && url[colon] != b']' {
        colon -= 1;
    }
    if url.get(colon) != Some(&b':') {
        colon = slash;
    } else if host_off == 0 && colon < slash && colon + 1 != slash {
        // A file: URL may not have a port.
        return None;
    }
    if strspn(url, URL_HOST_CHARS) < colon {
        return None;
    }
    norm.extend(url[..colon].iter().map(u8::to_ascii_lowercase));

    // The port: digits, 1 to 65535, leading zeros and defaults dropped.
    if colon < slash {
        let mut start = colon + 1;
        start += strspn(&url[start..slash], b"0");
        if start == slash && url[start - 1] == b'0' {
            start -= 1;
        }
        let port = &url[start..slash];
        let default = port.is_empty()
            || (port == b"80" && norm.starts_with(b"http:"))
            || (port == b"443" && norm.starts_with(b"https:"));
        if !default {
            if strspn(port, b"0123456789") < port.len() {
                return None;
            }
            let number: u32 = if port.len() <= 5 {
                std::str::from_utf8(port).ok()?.parse().ok()?
            } else {
                0
            };
            if number == 0 || number > 65535 {
                return None;
            }
            norm.push(b':');
            norm.extend_from_slice(port);
        }
    }
    url = &url[slash..];

    // The path, with `.` and `..` segments resolved.
    let path_start = norm.len();
    norm.push(b'/');
    if url.first() == Some(&b'/') {
        url = &url[1..];
    }
    loop {
        let seg_start = norm.len();
        let next = strcspn(url, b"/?#");
        let mut skip_add_slash = false;
        append_normalized_escapes(&mut norm, &url[..next])?;
        let segment = &norm[seg_start..];
        if segment == b"." {
            if seg_start == path_start + 1 {
                norm.truncate(norm.len() - 1);
                skip_add_slash = true;
            } else {
                norm.truncate(norm.len() - 2);
            }
        } else if segment == b".." {
            let mut prev = norm.len() - 3;
            if prev == path_start {
                return None;
            }
            loop {
                prev -= 1;
                if norm[prev] == b'/' {
                    break;
                }
            }
            if prev == path_start {
                norm.truncate(prev + 1);
                skip_add_slash = true;
            } else {
                norm.truncate(prev);
            }
        }
        url = &url[next..];
        if url.first() != Some(&b'/') {
            break;
        }
        url = &url[1..];
        if !skip_add_slash {
            norm.push(b'/');
        }
    }

    // The query and fragment, escapes normalized.
    if !url.is_empty() {
        append_normalized_escapes(&mut norm, url)?;
    }
    Some(norm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(content: &str) -> Vec<(String, Option<String>)> {
        let parsed = parse(content.as_bytes());
        assert_eq!(parsed.error, None, "{content:?}");
        parsed
            .entries
            .into_iter()
            .map(|e| (lossy(&e.name), e.value.map(|v| lossy(&v))))
            .collect()
    }

    fn kv(name: &str, value: Option<&str>) -> (String, Option<String>) {
        (name.to_owned(), value.map(str::to_owned))
    }

    #[test]
    fn parses_sections_keys_and_values() {
        assert_eq!(
            entries(
                "# comment\n; another\n[submodule \"Lib\"]\n\tpath = lib\n\tURL=https://x/y.git\n"
            ),
            [
                kv("submodule.Lib.path", Some("lib")),
                kv("submodule.Lib.url", Some("https://x/y.git")),
            ]
        );
        // The legacy form is lowercased; the extended form keeps case.
        assert_eq!(
            entries("[Submodule.LIB]\nPath=x\n"),
            [kv("submodule.lib.path", Some("x"))]
        );
        // Subsection escapes: any byte after `\` is itself.
        assert_eq!(
            entries("[a \"x\\\"y\\\\z\\n\"]\nk=v\n"),
            [kv("a.x\"y\\zn.k", Some("v"))]
        );
        // Boolean keys, key characters, spaces before `=`.
        assert_eq!(
            entries("[s]\nflag\nother-key2 \t= v\n"),
            [kv("s.flag", None), kv("s.other-key2", Some("v"))]
        );
        // A key before any section has no section.
        assert_eq!(entries("a=b\n"), [kv("a", Some("b"))]);
        // CR LF, a BOM, no final newline.
        assert_eq!(
            entries("\u{feff}[s]\r\nk = v \r\nj=w"),
            [kv("s.k", Some("v")), kv("s.j", Some("w"))]
        );
        // A lone CR is whitespace.
        assert_eq!(entries("[s]\rk=v\n"), [kv("s.k", Some("v"))]);
    }

    #[test]
    fn parses_values_as_git_does() {
        let value = |v: &str| {
            let parsed = parse(format!("[s]\nk={v}\n").as_bytes());
            assert_eq!(parsed.error, None, "{v:?}");
            lossy(parsed.entries[0].value.as_deref().unwrap())
        };
        assert_eq!(value("  a b  "), "a b");
        assert_eq!(value("a # comment"), "a");
        assert_eq!(value("a;comment"), "a");
        assert_eq!(value("\" a # b \""), " a # b ");
        assert_eq!(value("a\\tb\\nc\\\\d\\\"e\\bf"), "a\tb\nc\\d\"e\u{8}f");
        assert_eq!(value("x\"y\"z"), "xyz");
        assert_eq!(value("\"\" x"), "x");
        assert_eq!(value("a \"b\" "), "a b");
        assert_eq!(value(""), "");
        let parsed = parse(b"[s]\nk=one\\\n two\n");
        assert_eq!(parsed.entries[0].value.as_deref(), Some(&b"one two"[..]));
    }

    #[test]
    fn parse_errors() {
        let cases: [(&[u8], usize); 13] = [
            (b"[s]\nk=\"unterminated\n", 2),
            (b"[s]\nk=bad\\escape\n", 2),
            (b"[s]\nk # comment\n", 2),
            (b"[s]\n1key=v\n", 2),
            (b"[s\n", 1),
            (b"[]\n", 1),
            (b"[s \"x]\n", 1),
            (b"[s \"x\" ]\n", 1),
            (b"[s x]\n", 1),
            (b"[s_x]\n", 1),
            (b"[s]\nk_ey=v\n", 2),
            (b"\xef\xbbx", 1),
            (b"[s]\nk=v\n=x\n", 3),
        ];
        for (content, line) in cases {
            let parsed = parse(content);
            assert_eq!(
                parsed.error,
                Some(ParseError { line }),
                "{:?}",
                lossy(content)
            );
        }
        // Entries before the error are kept.
        let parsed = parse(b"[submodule \"a\"]\nurl=x\n!\n");
        assert_eq!(parsed.entries.len(), 1);
        assert!(parsed.error.is_some());
    }

    #[test]
    fn names() {
        for ok in ["valid", "valid/with/paths", "a..b", "...", ".", "x/./y"] {
            assert!(check_submodule_name(ok.as_bytes()), "{ok}");
        }
        // t7450-bad-git-dotfiles.sh, "check names".
        for bad in [
            "",
            "..",
            "../",
            "/../",
            "..\\",
            "../foo",
            "foo/..",
            "foo\\..",
            "foo/../bar",
        ] {
            assert!(!check_submodule_name(bad.as_bytes()), "{bad}");
        }
    }

    #[test]
    fn urls() {
        // t7450 and t0060's check-url vectors, and more.
        for ok in [
            "./bar/baz/foo.git",
            "https://example.com/foo.git",
            "http://example.com:80/deeper/foo.git",
            "../relative/path",
            "../../relative/path",
            "./-sneaky-option",
            "../-sneaky-option",
            "git://example.com/x.git",
            "git@github.com:a/b.git",
            "ssh://example.com/x",
            "/abs/path",
            "https://user:pass@example.com:8443/a/./b/../c?q=1#f",
            "https://example.com/a%2fb",
            "https://[::1]:8080/x",
            "http::https://example.com/x",
            "https://example.com/%00",
            "./%00",
        ] {
            assert!(check_submodule_url(ok.as_bytes()), "{ok}");
        }
        for bad in [
            "-sneaky-option",
            "./%0ahost=example.com/foo.git",
            "./%0Ahost=example.com/foo.git",
            "../a\nb",
            "git://example.com/%0a",
            "https://one.example.com/evil?%0ahost=two.example.com",
            "https:///example.com/foo.git",
            "http://example.com:test/foo.git",
            "https::example.com/foo.git",
            "http:::example.com/foo.git",
            "http::https:///x",
            "../:x",
            ".././/x",
            "..\\..\\:x",
            "https://example.com:0/x",
            "https://example.com:65536/x",
            "https://example.com:123456/x",
            "https://example.com/a%0/b",
            "https://exa mple.com/x",
            "https://example.com/../x",
            "https://example.com/%zz",
            "https://example.com/%4",
            "https://u%0a@example.com/x",
            "ftp://example.com/a\nb",
            "https://example.com#%0a",
            "https://example.com/a%0a/../b",
        ] {
            assert!(!check_submodule_url(bad.as_bytes()), "{bad:?}");
        }
        // What git 2.40 changed: each version refuses some URLs the other
        // accepts, and agentfs refuses both.
        use CurlUrlCheck::{Credential, Normalize};
        for (url, credential, normalize) in [
            ("https://example.com:0/x", true, false),
            ("https://exa mple.com/x", true, false),
            ("https://example.com/../x", true, false),
            ("https://example.com/%zz", true, false),
            ("https://example.com/a%0a/../b", false, true),
            ("https://example.com/a%0a", false, false),
            ("https:///x", false, false),
            ("https://example.com/x", true, true),
        ] {
            assert_eq!(
                check_submodule_url_as(url.as_bytes(), Credential),
                credential,
                "{url}"
            );
            assert_eq!(
                check_submodule_url_as(url.as_bytes(), Normalize),
                normalize,
                "{url}"
            );
        }
    }

    #[test]
    fn normalizes_urls_as_git_does() {
        let n = |u: &str| url_normalize(u.as_bytes()).map(|v| lossy(&v));
        assert_eq!(
            n("HTTPS://User@Example.COM:0443/a/./b/../c/%7e%41?x=%2f#y").as_deref(),
            Some("https://User@example.com/a/c/~A?x=%2F#y")
        );
        assert_eq!(
            n("http://h:080/").as_deref(),
            Some("http://h/"),
            "default port dropped"
        );
        assert_eq!(n("http://h:00/"), None, "port 0");
        assert_eq!(n("http://h").as_deref(), Some("http://h/"));
        assert_eq!(n("http://h/a/..").as_deref(), Some("http://h/"));
        assert_eq!(n("http://h/./").as_deref(), Some("http://h/"));
        assert_eq!(n("file:///x").as_deref(), Some("file:///x"));
        assert_eq!(n("file://:1/x"), None);
        assert_eq!(n("1http://h/"), None);
        assert_eq!(n("http:/h/"), None);
    }

    #[test]
    fn checks_in_fsck_order() {
        assert_eq!(check(b""), Ok(()));
        assert_eq!(
            check(b"[submodule \"x\"]\n\tpath = x\n\turl = https://example.com/x\n"),
            Ok(())
        );
        assert_eq!(
            check(b"[submodule \"../../x\"]\nurl=./y\n"),
            Err(Problem::Name("../../x".into()))
        );
        assert_eq!(
            check(b"[submodule \"x\"]\nurl=-u\n"),
            Err(Problem::Url("-u".into()))
        );
        assert_eq!(
            check(b"[submodule \"x\"]\npath=-p\n"),
            Err(Problem::Path("-p".into()))
        );
        assert_eq!(
            check(b"[submodule \"x\"]\nupdate=!rm -rf /\n"),
            Err(Problem::Update("!rm -rf /".into()))
        );
        // Only submodule sections, and only these keys, are checked.
        assert_eq!(
            check(b"[other \"..\"]\nurl=-x\n[submodule]\nurl=-x\n"),
            Ok(())
        );
        assert_eq!(
            check(b"[submodule \"x\"]\nbranch=-x\nupdate=rebase\n"),
            Ok(())
        );
        // A boolean url is not checked, but its name is.
        assert_eq!(check(b"[submodule \"x\"]\nurl\n"), Ok(()));
        assert_eq!(
            check(b"[submodule.]\nurl\n"),
            Err(Problem::Name(String::new()))
        );
        // Entries before a parse error are checked first.
        assert_eq!(
            check(b"[submodule \"x\"]\npath=-p\n[\n"),
            Err(Problem::Path("-p".into()))
        );
        assert_eq!(
            check(b"[submodule \"x\"]\n[\n"),
            Err(Problem::Parse { line: 2 })
        );
        // A NUL ends a name or value, as in git's C code.
        assert_eq!(check(b"[submodule \"x\"]\nurl=ok\0-x\n"), Ok(()));
        assert_eq!(check(b"[submodule \"..\0x\"]\nurl=ok\n"), Ok(()));
        assert_eq!(Problem::Url(String::new()).fsck_id(), "gitmodulesUrl");
    }
}
