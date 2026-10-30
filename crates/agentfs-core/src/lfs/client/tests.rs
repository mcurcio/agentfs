//! The batch client against the in-process fake server, and the
//! standalone-file transfer against a real repository.

use std::path::Path;

use super::*;
use crate::config::LfsSettings;
use crate::lfs::fake::{FakeLfs, State};

#[path = "../../../tests/support/closed_port.rs"]
mod closed_port;
use closed_port::ClosedPort;

const REF: &str = "refs/heads/agentfs/vol";

struct Setup {
    dir: tempfile::TempDir,
    cache: ObjectCache,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cache = ObjectCache::open(dir.path().join("lfs")).unwrap();
        Self { dir, cache }
    }

    /// Caches `content` and returns its pointer.
    fn object(&self, content: &[u8]) -> Pointer {
        let src = self.dir.path().join("src");
        fs::write(&src, content).unwrap();
        self.cache.insert_file(&src).unwrap()
    }

    fn credentials(&self, name: &str, content: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        fs::write(&path, content).unwrap();
        path
    }
}

fn remote(url: &str) -> RemoteSettings {
    RemoteSettings {
        name: "origin".into(),
        url: url.into(),
        push_url: None,
        max_blob_size: None,
        max_push_size: None,
        max_lfs_object_size: None,
        proxy: None,
        credentials: None,
        ssh_key: None,
        known_hosts: None,
        lfs_url: None,
        lfs_push_url: None,
        lfs_credentials: None,
        ssh_agent: None,
        ssh_key_passphrase: None,
        fork_remote: false,
        push_source: crate::config::PushSource::Branch,
    }
}

fn settings() -> Settings {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("config"), "[agentfs]\n\tvolume = v\n").unwrap();
    let mut s = Settings::load(dir.path()).unwrap();
    s.lfs = LfsSettings::default();
    s
}

fn http(server: &FakeLfs, r: &mut RemoteSettings) -> Box<dyn Transfer> {
    r.lfs_url = Some(server.url());
    transfer_for(&settings(), r, Direction::Upload).unwrap()
}

fn basic(user: &str, password: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
    )
}

#[test]
fn upload_skips_what_the_server_has_and_verifies_when_asked() {
    let s = Setup::new();
    let server = FakeLfs::start();
    server.with(|st| st.verify = true);
    let a = s.object(b"first object");
    let b = s.object(b"second object, longer");
    let mut r = remote("/unused");
    let t = http(&server, &mut r);
    let report = t.upload(&[a, b], &s.cache, REF).unwrap();
    assert_eq!(
        report,
        UploadReport {
            objects: 2,
            uploaded: 2,
            bytes: a.size + b.size
        }
    );
    assert_eq!(server.object(&a.oid.to_string()).unwrap(), b"first object");
    // Concurrent: in any order.
    let mut verified = server.with(|st| st.verified.clone());
    verified.sort();
    let mut expected = [a.oid.to_string(), b.oid.to_string()];
    expected.sort();
    assert_eq!(verified, expected);
    let batch = &server.seen()[0];
    assert_eq!(batch.path, "/repo/info/lfs/objects/batch");
    assert_eq!(batch.header("content-type"), Some(MEDIA_TYPE));
    assert_eq!(batch.header("accept"), Some(MEDIA_TYPE));
    assert!(batch.header("authorization").is_none(), "anonymous");
    let put = server
        .seen()
        .into_iter()
        .find(|s| s.method == "PUT" && s.path.ends_with(&a.oid.to_string()))
        .unwrap();
    assert_eq!(put.header("content-length"), Some("12"));
    // Again: nothing to send.
    let before = server.seen().len();
    let report = t.upload(&[a, b], &s.cache, REF).unwrap();
    assert_eq!(report.uploaded, 0);
    assert_eq!(server.seen().len(), before + 1, "one batch request, no PUT");
}

#[test]
fn download_is_hash_checked() {
    let s = Setup::new();
    let server = FakeLfs::start();
    let content = b"downloaded content".to_vec();
    let p = Pointer::of_reader(&content[..]).unwrap();
    server.with(|st| st.objects.insert(p.oid.to_string(), content.clone()));
    let mut r = remote("/unused");
    let t = http(&server, &mut r);
    server.with(|st| st.corrupt_downloads = true);
    let err = t.download(&p, &s.cache, REF).unwrap_err();
    assert_eq!(err.kind, PushFailureKind::Refused, "{err}");
    assert!(!s.cache.contains(&p));
    server.with(|st| st.corrupt_downloads = false);
    t.download(&p, &s.cache, REF).unwrap();
    assert_eq!(fs::read(s.cache.path(&p.oid)).unwrap(), content);
    // An object the server lacks.
    let missing = Pointer::of_reader(&b"nowhere"[..]).unwrap();
    let err = t.download(&missing, &s.cache, REF).unwrap_err();
    assert_eq!(err.kind, PushFailureKind::Refused);
    assert!(err.message.contains("404"), "{err}");
}

#[test]
fn failures_are_classified() {
    let s = Setup::new();
    let p = s.object(b"x");
    let mut r = remote("/unused");
    for (setup, kind, text) in [
        (
            Box::new(|st: &mut State| st.object_error = Some((422, "too large".into())))
                as Box<dyn Fn(&mut State)>,
            PushFailureKind::Refused,
            "422 too large",
        ),
        (
            Box::new(|st: &mut State| st.batch_status = Some(401)),
            PushFailureKind::Auth,
            "set remote.origin.agentfsLfsCredentials",
        ),
        (
            Box::new(|st: &mut State| st.batch_status = Some(403)),
            PushFailureKind::Auth,
            "HTTP 403: the server says no",
        ),
        (
            Box::new(|st: &mut State| st.batch_status = Some(500)),
            PushFailureKind::Unreachable,
            "HTTP 500",
        ),
        (
            Box::new(|st: &mut State| st.batch_status = Some(429)),
            PushFailureKind::Unreachable,
            "HTTP 429",
        ),
        (
            Box::new(|st: &mut State| st.batch_status = Some(404)),
            PushFailureKind::Refused,
            "HTTP 404",
        ),
        (
            Box::new(|st: &mut State| st.redirect = true),
            PushFailureKind::Refused,
            "redirects are not followed",
        ),
    ] {
        let server = FakeLfs::start();
        server.with(|st| setup(st));
        let t = http(&server, &mut r);
        let err = t.upload(&[p], &s.cache, REF).unwrap_err();
        assert_eq!(err.kind, kind, "{err}");
        assert!(err.message.contains(text), "{err}");
        // A redirect is never followed.
        assert!(server.seen().iter().all(|s| !s.path.contains("elsewhere")));
    }
    // Nothing listening.
    let closed = ClosedPort::new();
    r.lfs_url = Some(format!("http://127.0.0.1:{}/r", closed.port()));
    let t = transfer_for(&settings(), &r, Direction::Upload).unwrap();
    let err = t.upload(&[p], &s.cache, REF).unwrap_err();
    assert_eq!(err.kind, PushFailureKind::Unreachable, "{err}");
    // An object missing from the cache is a local problem.
    let server = FakeLfs::start();
    let t = http(&server, &mut r);
    let missing = Pointer::of_reader(&b"never cached"[..]).unwrap();
    let err = t.upload(&[missing], &s.cache, REF).unwrap_err();
    assert_eq!(err.kind, PushFailureKind::Local, "{err}");
}

#[test]
fn credentials_go_to_the_endpoint_origin_only() {
    let s = Setup::new();
    let p = s.object(b"secret-bound content");
    let server = FakeLfs::start();
    let expected = basic("lfsbob", "lfspw");
    server.with(|st| {
        st.require_auth = Some(expected.clone());
        st.actions_elsewhere = true;
        st.verify = true;
    });
    let mut r = remote("/unused");
    r.lfs_credentials = Some(s.credentials("lfs-token", "username=lfsbob\npassword=lfspw\n"));
    let t = http(&server, &mut r);
    t.upload(&[p], &s.cache, REF).unwrap();
    let seen = server.seen();
    let batch = seen.iter().find(|s| s.path.ends_with("/batch")).unwrap();
    assert_eq!(batch.port, server.port);
    assert_eq!(batch.header("authorization"), Some(expected.as_str()));
    // The actions point at another port: another origin, so no
    // credentials.
    for other in seen.iter().filter(|s| !s.path.ends_with("/batch")) {
        assert_eq!(other.port, server.other_port, "{other:?}");
        assert_eq!(other.header("authorization"), None, "{other:?}");
    }

    // Actions on the endpoint's own origin get them, unless the server
    // sent its own Authorization, which is used as given.
    let server = FakeLfs::start();
    server.with(|st| st.require_auth = Some(expected.clone()));
    let t = http(&server, &mut r);
    t.upload(&[p], &s.cache, REF).unwrap();
    let put = server
        .seen()
        .into_iter()
        .find(|s| s.method == "PUT")
        .unwrap();
    assert_eq!(put.header("authorization"), Some(expected.as_str()));
    let server = FakeLfs::start();
    server.with(|st| {
        st.require_auth = Some(expected.clone());
        st.action_headers
            .insert("Authorization".into(), "Bearer from-server".into());
        st.action_headers.insert("X-Extra".into(), "1".into());
    });
    let t = http(&server, &mut r);
    t.upload(&[p], &s.cache, REF).unwrap();
    let put = server
        .seen()
        .into_iter()
        .find(|s| s.method == "PUT")
        .unwrap();
    assert_eq!(put.header("authorization"), Some("Bearer from-server"));
    assert_eq!(put.header("x-extra"), Some("1"));

    // A wrong credential is an auth failure naming the key.
    r.lfs_credentials = Some(s.credentials("bad", "username=lfsbob\npassword=nope\n"));
    let server = FakeLfs::start();
    server.with(|st| st.require_auth = Some(expected.clone()));
    let t = http(&server, &mut r);
    let err = t.upload(&[p], &s.cache, REF).unwrap_err();
    assert_eq!(err.kind, PushFailureKind::Auth);
    assert!(err.message.contains("agentfsLfsCredentials"), "{err}");
    assert!(!err.message.contains("nope"), "{err}");
    // A missing credential file too.
    r.lfs_credentials = Some(s.dir.path().join("absent"));
    let t = http(&server, &mut r);
    let err = t.upload(&[p], &s.cache, REF).unwrap_err();
    assert_eq!(err.kind, PushFailureKind::Auth, "{err}");
}

#[test]
fn the_git_credentials_are_reused_only_on_the_push_urls_origin() {
    let s = Setup::new();
    let p = s.object(b"derived");
    let server = FakeLfs::start();
    let alice = basic("alice", "pw1");
    server.with(|st| st.require_auth = Some(alice.clone()));
    // The remote's own URL on the server's origin: the derived endpoint is
    // `<url>.git/info/lfs`, with the git credentials.
    let mut r = remote(&format!("http://127.0.0.1:{}/repo", server.port));
    r.credentials = Some(s.credentials("git", "username=alice\npassword=pw1\n"));
    let t = transfer_for(&settings(), &r, Direction::Upload).unwrap();
    t.upload(&[p], &s.cache, REF).unwrap();
    let batch = &server.seen()[0];
    assert_eq!(batch.path, "/repo.git/info/lfs/objects/batch");
    assert_eq!(batch.header("authorization"), Some(alice.as_str()));
    // The same server named by another origin (`localhost` is not
    // `127.0.0.1`): the git credentials are not offered.
    r.lfs_url = Some(format!("http://localhost:{}/repo/info/lfs", server.port));
    let t = transfer_for(&settings(), &r, Direction::Upload).unwrap();
    let err = t.upload(&[p], &s.cache, REF).unwrap_err();
    assert_eq!(err.kind, PushFailureKind::Auth, "{err}");
    let last = server.seen().pop().unwrap();
    assert_eq!(last.header("authorization"), None);
}

#[test]
fn messages_never_carry_url_credentials() {
    let s = Setup::new();
    let p = s.object(b"x");
    let server = FakeLfs::start();
    server.with(|st| st.batch_status = Some(500));
    let mut r = remote("/unused");
    r.lfs_url = Some(format!(
        "http://bob:hunter2@127.0.0.1:{}/repo/info/lfs",
        server.port
    ));
    let t = transfer_for(&settings(), &r, Direction::Upload).unwrap();
    let err = t.upload(&[p], &s.cache, REF).unwrap_err();
    assert!(!err.message.contains("hunter2"), "{err}");
    assert!(err.message.contains("***@127.0.0.1"), "{err}");
    // The URL's password is used as the credential.
    assert_eq!(
        server.seen()[0].header("authorization"),
        Some(basic("bob", "hunter2").as_str())
    );
}

#[test]
fn https_needs_the_feature() {
    let mut r = remote("/unused");
    r.lfs_url = Some("https://lfs.example/r".into());
    let result = transfer_for(&settings(), &r, Direction::Upload);
    if cfg!(feature = "https") {
        assert!(result.is_ok());
    } else {
        let err = result.err().unwrap();
        assert_eq!(err.kind, PushFailureKind::Local);
        assert!(err.message.contains("`https` cargo feature"), "{err}");
    }
}

#[test]
fn certificate_errors_are_recognised() {
    for text in [
        "error:0A000086:SSL routines:tls_post_process_server_certificate:certificate verify failed",
        "self-signed certificate in certificate chain",
        "unable to get local issuer certificate",
    ] {
        assert!(is_certificate_error(text), "{text}");
    }
    assert!(!is_certificate_error("Connection refused (os error 111)"));
}

fn bare(path: &Path) {
    git2::Repository::init_bare(path).unwrap();
}

#[test]
fn file_remotes_use_the_standalone_layout() {
    let s = Setup::new();
    let remote_dir = s.dir.path().join("remote.git");
    bare(&remote_dir);
    let a = s.object(b"to a file remote");
    for url in [
        remote_dir.display().to_string(),
        format!("file://{}", remote_dir.display()),
    ] {
        let t = transfer_for(&settings(), &remote(&url), Direction::Upload).unwrap();
        let report = t.upload(&[a], &s.cache, REF).unwrap();
        let hex = a.oid.to_string();
        let object = remote_dir.join(format!("lfs/objects/{}/{}/{hex}", &hex[..2], &hex[2..4]));
        assert_eq!(fs::read(&object).unwrap(), b"to a file remote");
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&object).unwrap().permissions().mode();
        assert_eq!(mode & 0o044, 0o044, "readable by cloners: {mode:o}");
        assert!(report.uploaded <= 1);
        // And back, into an empty cache.
        let other = Setup::new();
        let down = transfer_for(&settings(), &remote(&url), Direction::Download).unwrap();
        down.download(&a, &other.cache, REF).unwrap();
        assert_eq!(
            fs::read(other.cache.path(&a.oid)).unwrap(),
            b"to a file remote"
        );
    }
    // A missing object is refused; a missing cache object is local.
    let t = transfer_for(
        &settings(),
        &remote(remote_dir.to_str().unwrap()),
        Direction::Download,
    )
    .unwrap();
    let absent = Pointer::of_reader(&b"absent"[..]).unwrap();
    assert_eq!(
        t.download(&absent, &s.cache, REF).unwrap_err().kind,
        PushFailureKind::Refused
    );
    assert_eq!(
        t.upload(&[absent], &s.cache, REF).unwrap_err().kind,
        PushFailureKind::Local
    );
}

#[test]
fn object_timeouts_grow_with_size() {
    assert_eq!(object_timeout(0), REQUEST_TIMEOUT);
    assert_eq!(
        object_timeout(1 << 30),
        REQUEST_TIMEOUT + Duration::from_secs(4096)
    );
}

fn http_with(server: &FakeLfs, r: &mut RemoteSettings, concurrency: usize) -> Box<dyn Transfer> {
    r.lfs_url = Some(server.url());
    let mut s = settings();
    s.lfs.concurrent_transfers = concurrency;
    transfer_for(&s, r, Direction::Upload).unwrap()
}

#[test]
fn uploads_run_concurrently_up_to_the_setting() {
    let s = Setup::new();
    let objects: Vec<Pointer> = (0..12)
        .map(|i| s.object(format!("object number {i}").as_bytes()))
        .collect();
    for (n, want) in [(4, 4), (1, 1)] {
        let server = FakeLfs::start();
        server.with(|st| st.delay = Duration::from_millis(60));
        let mut r = remote("/unused");
        let t = http_with(&server, &mut r, n);
        let report = t.upload(&objects, &s.cache, REF).unwrap();
        assert_eq!(report.uploaded, 12);
        assert_eq!(server.with(|st| st.max_in_flight), want, "concurrency {n}");
        for p in &objects {
            assert!(server.object(&p.oid.to_string()).is_some());
        }
        // Downloads too, into an empty cache.
        let other = Setup::new();
        server.with(|st| st.max_in_flight = 0);
        let results = t.download_many(&objects, &other.cache, REF);
        assert!(results.iter().all(Result::is_ok), "{results:?}");
        assert_eq!(server.with(|st| st.max_in_flight), want, "concurrency {n}");
        assert!(objects.iter().all(|p| other.cache.contains(p)));
    }
}

/// Sends `SIGUSR2` to the thread that made it, every 200 µs, until
/// dropped (or, [`SignalStorm::at_this_thread_for`], for a while). The handler does nothing and is installed with `SA_RESTART`,
/// as signal-hook installs the daemon's: Linux still fails a socket read
/// that has a receive timeout with `EINTR` (signal(7)), as it does after a
/// stop and continue with no handler at all. Only this thread is
/// signalled, so tests running beside it are not.
struct SignalStorm {
    done: Arc<std::sync::atomic::AtomicBool>,
    sender: Option<std::thread::JoinHandle<usize>>,
}

impl SignalStorm {
    fn at_this_thread() -> Self {
        Self::at_this_thread_for(Duration::MAX)
    }

    fn at_this_thread_for(limit: Duration) -> Self {
        extern "C" fn nothing(_: libc::c_int) {}
        // SAFETY: a zeroed sigaction is valid; the handler is
        // async-signal-safe (it does nothing).
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = nothing as *const () as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            assert_eq!(
                libc::sigaction(libc::SIGUSR2, &action, std::ptr::null_mut()),
                0
            );
        }
        // SAFETY: pthread_self has no preconditions.
        let target = unsafe { libc::pthread_self() } as usize;
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop = done.clone();
        let sender = std::thread::spawn(move || {
            let mut sent = 0;
            let started = std::time::Instant::now();
            while !stop.load(Ordering::SeqCst) && started.elapsed() < limit {
                // SAFETY: the target thread outlives this one: Drop joins
                // it before the target can return.
                assert_eq!(
                    unsafe { libc::pthread_kill(target as libc::pthread_t, libc::SIGUSR2) },
                    0
                );
                sent += 1;
                std::thread::sleep(Duration::from_micros(200));
            }
            sent
        });
        Self {
            done,
            sender: Some(sender),
        }
    }
}

impl Drop for SignalStorm {
    fn drop(&mut self) {
        self.done.store(true, Ordering::SeqCst);
        if let Some(sender) = self.sender.take() {
            let _ = sender.join();
        }
    }
}

/// A signal is not a failure to reach the server (agentfs-lh8): every
/// read and write of a transfer survives a storm of them. Concurrency 1
/// keeps every request on this thread, the one signalled; the server's
/// delay keeps each read blocked long enough to be interrupted.
#[test]
fn transfers_survive_signals() {
    let s = Setup::new();
    let objects: Vec<Pointer> = (0..4)
        .map(|i| s.object(format!("signalled object {i}").as_bytes()))
        .collect();
    let server = FakeLfs::start();
    server.with(|st| st.delay = Duration::from_millis(30));
    let mut r = remote("/unused");
    let t = http_with(&server, &mut r, 1);
    let storm = SignalStorm::at_this_thread();
    let report = t.upload(&objects, &s.cache, REF);
    let other = Setup::new();
    let results = t.download_many(&objects, &other.cache, REF);
    drop(storm);
    assert_eq!(report.unwrap().uploaded, 4);
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    assert!(objects.iter().all(|p| other.cache.contains(p)));
}

/// A read retried after a signal stays within what is left of its
/// timeout (agentfs-lh8): a request to a server that never answers fails
/// at its deadline while signals keep coming, not only once they stop.
#[test]
fn signals_never_stretch_a_deadline() {
    // Connections complete in the backlog; nothing ever answers.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/hook", silent.local_addr().unwrap());
    let storm = SignalStorm::at_this_thread_for(Duration::from_secs(6));
    let started = std::time::Instant::now();
    let e = post(&url, b"{}", &[], Duration::from_secs(1)).unwrap_err();
    let took = started.elapsed();
    drop(storm);
    assert!(matches!(e, PostError::Unreachable { .. }), "{e}");
    assert!(
        took < Duration::from_secs(4),
        "took {took:?}: signals stretched a 1 s deadline ({e})"
    );
}

#[test]
fn one_failed_object_leaves_the_others_uploaded() {
    let s = Setup::new();
    let objects: Vec<Pointer> = (0..6)
        .map(|i| s.object(format!("object {i}").as_bytes()))
        .collect();
    let server = FakeLfs::start();
    let bad = objects[0].oid.to_string();
    server.with(|st| {
        st.put_status.insert(bad.clone(), 503);
        st.delay = Duration::from_millis(20);
    });
    let mut r = remote("/unused");
    let t = http_with(&server, &mut r, 8);
    let err = t.upload(&objects, &s.cache, REF).unwrap_err();
    assert_eq!(err.kind, PushFailureKind::Unreachable, "{err}");
    assert!(err.message.contains(&bad), "{err}");
    // All were in flight together: the others are on the server.
    for p in &objects[1..] {
        assert!(server.object(&p.oid.to_string()).is_some());
    }
    // The next attempt sends only the one that failed.
    server.with(|st| st.put_status.clear());
    let report = t.upload(&objects, &s.cache, REF).unwrap();
    assert_eq!(report.uploaded, 1);
    assert_eq!(report.objects, 6);
}

#[test]
fn only_objects_the_server_lacks_need_a_cached_copy() {
    let s = Setup::new();
    let server = FakeLfs::start();
    let content = b"on the server, not in the cache".to_vec();
    let on_server = Pointer::of_reader(&content[..]).unwrap();
    server.with(|st| st.objects.insert(on_server.oid.to_string(), content));
    let cached = s.object(b"cached here");
    let mut r = remote("/unused");
    let t = http(&server, &mut r);
    let report = t.upload(&[on_server, cached], &s.cache, REF).unwrap();
    assert_eq!(report.uploaded, 1);
    // One the server lacks and the cache lacks: local, before any PUT,
    // with the hint.
    let lost = Pointer::of_reader(&b"lost from the cache"[..]).unwrap();
    let other = s.object(b"another one to send");
    let before = server.seen().len();
    let err = t.upload(&[lost, other], &s.cache, REF).unwrap_err();
    assert_eq!(err.kind, PushFailureKind::Local, "{err}");
    assert!(err.message.contains("agentfs lfs fetch --all"), "{err}");
    assert!(err.message.contains(&lost.oid.to_string()), "{err}");
    assert_eq!(err.missing, [lost.oid]);
    let after: Vec<_> = server.seen()[before..].to_vec();
    assert!(after.iter().all(|s| s.method != "PUT"), "{after:?}");
}

#[test]
fn presence_follows_the_batch_answers() {
    let s = Setup::new();
    let server = FakeLfs::start();
    let content = b"present".to_vec();
    let here = Pointer::of_reader(&content[..]).unwrap();
    server.with(|st| st.objects.insert(here.oid.to_string(), content));
    let absent = s.object(b"absent from the server");
    let mut r = remote("/unused");
    let t = http(&server, &mut r);
    for direction in [Direction::Upload, Direction::Download] {
        assert_eq!(
            t.present(&[here, absent], direction, REF).unwrap(),
            [true, false],
            "{direction:?}"
        );
    }
    // Nothing was transferred.
    assert!(server.seen().iter().all(|s| s.path.ends_with("/batch")));
    // Any other per-object error fails the call.
    server.with(|st| st.object_error = Some((500, "broken".into())));
    let err = t.present(&[here], Direction::Download, REF).unwrap_err();
    assert_eq!(err.kind, PushFailureKind::Unreachable, "{err}");
    // A file remote: the file with the right size.
    let remote_dir = s.dir.path().join("remote.git");
    bare(&remote_dir);
    let file = transfer_for(
        &settings(),
        &remote(remote_dir.to_str().unwrap()),
        Direction::Upload,
    )
    .unwrap();
    file.upload(&[absent], &s.cache, REF).unwrap();
    assert_eq!(
        file.present(&[absent, here], Direction::Download, REF)
            .unwrap(),
        [true, false]
    );
}

#[test]
fn download_many_keeps_going_past_a_missing_object() {
    let s = Setup::new();
    let server = FakeLfs::start();
    let mut pointers = Vec::new();
    for i in 0..4 {
        let content = format!("served {i}").into_bytes();
        let p = Pointer::of_reader(&content[..]).unwrap();
        server.with(|st| st.objects.insert(p.oid.to_string(), content));
        pointers.push(p);
    }
    let missing = Pointer::of_reader(&b"not on the server"[..]).unwrap();
    pointers.insert(1, missing);
    let mut r = remote("/unused");
    let t = http(&server, &mut r);
    let results = t.download_many(&pointers, &s.cache, REF);
    assert_eq!(results.len(), 5);
    assert_eq!(
        results[1].as_ref().unwrap_err().kind,
        PushFailureKind::Refused
    );
    for (i, p) in pointers.iter().enumerate() {
        assert_eq!(results[i].is_ok(), i != 1);
        assert_eq!(s.cache.contains(p), i != 1);
    }
    // A server that cannot be reached fails every object.
    server.with(|st| st.batch_status = Some(503));
    let other = Setup::new();
    let results = t.download_many(&pointers, &other.cache, REF);
    assert!(
        results
            .iter()
            .all(|r| r.as_ref().unwrap_err().kind == PushFailureKind::Unreachable)
    );
}

/// A server holding `content`, and a transfer to it.
fn serving(content: &[u8]) -> (FakeLfs, Box<dyn Transfer>, Pointer) {
    let server = FakeLfs::start();
    let p = Pointer::of_reader(content).unwrap();
    server.with(|st| st.objects.insert(p.oid.to_string(), content.to_vec()));
    let mut r = remote("/unused");
    let t = http(&server, &mut r);
    (server, t, p)
}

/// The `Range` header of each download request, in order.
fn ranges(server: &FakeLfs) -> Vec<Option<String>> {
    server
        .seen()
        .into_iter()
        .filter(|s| s.method == "GET")
        .map(|s| s.header("range").map(str::to_owned))
        .collect()
}

fn big(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 % 251) as u8).collect()
}

#[test]
fn downloads_resume_with_range() {
    let s = Setup::new();
    let content = big(200_000);
    let (server, t, p) = serving(&content);
    // Cut off after 70000 bytes: the retry asks for the rest.
    server.with(|st| st.truncate_next = vec![70_000]);
    t.download(&p, &s.cache, REF).unwrap();
    assert_eq!(fs::read(s.cache.path(&p.oid)).unwrap(), content);
    assert_eq!(ranges(&server), [None, Some("bytes=70000-".into())]);
    assert_eq!(fs::read_dir(s.cache.incomplete_dir()).unwrap().count(), 0);

    // Every try cut off: the partial stays for the next call.
    let s = Setup::new();
    let (server, t, p) = serving(&content);
    server.with(|st| st.truncate_next = vec![50_000, 1000, 0]);
    let err = t.download(&p, &s.cache, REF).unwrap_err();
    assert_eq!(err.kind, PushFailureKind::Unreachable, "{err}");
    assert!(err.message.contains("resumes"), "{err}");
    let partial = s.cache.incomplete_dir().join(p.oid.to_string());
    assert_eq!(fs::metadata(&partial).unwrap().len(), 51_000);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&partial).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        ranges(&server),
        [
            None,
            Some("bytes=50000-".into()),
            Some("bytes=51000-".into())
        ]
    );
    // Another cache handle (another command) resumes it.
    let again = ObjectCache::open(s.dir.path().join("lfs")).unwrap();
    assert!(partial.exists(), "open clears tmp/, never incomplete/");
    t.download(&p, &again, REF).unwrap();
    assert_eq!(
        ranges(&server).last().unwrap().as_deref(),
        Some("bytes=51000-")
    );
    assert_eq!(fs::read(again.path(&p.oid)).unwrap(), content);
    assert!(!partial.exists());
}

/// Leaves `n` bytes of `content` as `p`'s partial download.
fn leave_partial(s: &Setup, p: &Pointer, bytes: &[u8]) {
    fs::write(s.cache.incomplete_dir().join(p.oid.to_string()), bytes).unwrap();
}

#[test]
fn odd_range_answers_start_again_from_zero() {
    let content = big(100_000);
    // lfs-test-server's Content-Range (the remaining length as the total)
    // is accepted: only the start is checked.
    let s = Setup::new();
    let (server, t, p) = serving(&content);
    server.with(|st| st.range_quirk = true);
    leave_partial(&s, &p, &content[..30_000]);
    t.download(&p, &s.cache, REF).unwrap();
    assert_eq!(ranges(&server), [Some("bytes=30000-".into())]);
    assert_eq!(fs::read(s.cache.path(&p.oid)).unwrap(), content);
    // A server that ignores Range sends everything again: kept whole.
    let s = Setup::new();
    let (server, t, p) = serving(&content);
    server.with(|st| st.ignore_range = true);
    leave_partial(&s, &p, &content[..30_000]);
    t.download(&p, &s.cache, REF).unwrap();
    assert_eq!(ranges(&server), [Some("bytes=30000-".into())]);
    assert_eq!(fs::read(s.cache.path(&p.oid)).unwrap(), content);
    // A 206 that starts elsewhere: the partial is dropped and the whole
    // object asked for once, without Range.
    let s = Setup::new();
    let (server, t, p) = serving(&content);
    server.with(|st| st.wrong_range_start = true);
    leave_partial(&s, &p, &content[..30_000]);
    t.download(&p, &s.cache, REF).unwrap();
    assert_eq!(ranges(&server), [Some("bytes=30000-".into()), None]);
    assert_eq!(fs::read(s.cache.path(&p.oid)).unwrap(), content);
    // A partial longer than the object is emptied first.
    let s = Setup::new();
    let (server, t, p) = serving(&content);
    let mut long = content.clone();
    long.push(0);
    leave_partial(&s, &p, &long);
    t.download(&p, &s.cache, REF).unwrap();
    assert_eq!(ranges(&server), [None]);
    assert_eq!(fs::read(s.cache.path(&p.oid)).unwrap(), content);
}

#[test]
fn a_corrupt_partial_is_discarded() {
    let s = Setup::new();
    let content = big(100_000);
    let (server, t, p) = serving(&content);
    let mut wrong = content[..40_000].to_vec();
    wrong[10] ^= 1;
    leave_partial(&s, &p, &wrong);
    let err = t.download(&p, &s.cache, REF).unwrap_err();
    assert_eq!(err.kind, PushFailureKind::Refused, "{err}");
    assert!(!s.cache.contains(&p));
    assert_eq!(fs::read_dir(s.cache.incomplete_dir()).unwrap().count(), 0);
    // The next try starts from zero.
    t.download(&p, &s.cache, REF).unwrap();
    assert_eq!(ranges(&server), [Some("bytes=40000-".into()), None]);
    assert_eq!(fs::read(s.cache.path(&p.oid)).unwrap(), content);
}

#[test]
fn downloads_are_tried_three_times() {
    let s = Setup::new();
    let content = big(10_000);
    let (server, t, p) = serving(&content);
    server.with(|st| st.truncate_next = vec![0, 0, 0, 0]);
    let err = t.download(&p, &s.cache, REF).unwrap_err();
    assert_eq!(err.kind, PushFailureKind::Unreachable, "{err}");
    assert_eq!(ranges(&server).len(), DOWNLOAD_TRIES);
    // A refusal is not retried.
    let s = Setup::new();
    let (server, t, p) = serving(&content);
    server.with(|st| st.corrupt_downloads = true);
    assert_eq!(
        t.download(&p, &s.cache, REF).unwrap_err().kind,
        PushFailureKind::Refused
    );
    assert_eq!(ranges(&server).len(), 1);
}

#[test]
fn concurrent_downloads_of_one_object_take_turns() {
    let s = Setup::new();
    let content = big(300_000);
    let (server, t, p) = serving(&content);
    server.with(|st| st.delay = Duration::from_millis(50));
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| t.download(&p, &s.cache, REF).unwrap());
        }
    });
    assert_eq!(fs::read(s.cache.path(&p.oid)).unwrap(), content);
    // One downloaded it; the others found it cached once they had the lock.
    assert_eq!(ranges(&server).len(), 1);
}

/// An HTTP CONNECT proxy on a loopback port that records the targets it
/// tunnels to.
struct ConnectProxy {
    port: u16,
    targets: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl ConnectProxy {
    fn start() -> Self {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let targets = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = targets.clone();
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(mut client) = client else { continue };
                let seen = seen.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(client.try_clone().unwrap());
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    loop {
                        let mut h = String::new();
                        if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                            break;
                        }
                    }
                    let mut parts = line.split_whitespace();
                    let (Some("CONNECT"), Some(target)) = (parts.next(), parts.next()) else {
                        let _ = client.write_all(b"HTTP/1.1 405 Only CONNECT\r\n\r\n");
                        return;
                    };
                    seen.lock().unwrap().push(target.to_owned());
                    let Ok(upstream) = std::net::TcpStream::connect(target) else {
                        let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
                        return;
                    };
                    client
                        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                        .unwrap();
                    let mut up_read = upstream.try_clone().unwrap();
                    let mut client_write = client.try_clone().unwrap();
                    let back = std::thread::spawn(move || {
                        let _ = std::io::copy(&mut up_read, &mut client_write);
                        let _ = client_write.shutdown(std::net::Shutdown::Write);
                    });
                    // What the reader buffered past the headers is part of
                    // the tunnel.
                    let mut upstream_write = upstream;
                    let _ = upstream_write.write_all(reader.buffer());
                    let mut rest = reader.into_inner();
                    let _ = std::io::copy(&mut rest, &mut upstream_write);
                    let _ = upstream_write.shutdown(std::net::Shutdown::Write);
                    let _ = back.join();
                });
            }
        });
        Self { port, targets }
    }

    fn targets(&self) -> Vec<String> {
        self.targets.lock().unwrap().clone()
    }
}

#[test]
fn lfs_requests_go_through_the_remotes_proxy() {
    let s = Setup::new();
    let p = s.object(b"through the proxy");
    let server = FakeLfs::start();
    let proxy = ConnectProxy::start();
    let mut r = remote("/unused");
    r.proxy = Some(crate::proxy::ProxySetting {
        url: format!("http://127.0.0.1:{}", proxy.port),
        key: "remote.origin.proxy".into(),
    });
    let t = http(&server, &mut r);
    t.upload(&[p], &s.cache, REF).unwrap();
    let other = Setup::new();
    t.download(&p, &other.cache, REF).unwrap();
    assert!(other.cache.contains(&p));
    let target = format!("127.0.0.1:{}", server.port);
    let targets = proxy.targets();
    assert!(
        !targets.is_empty() && targets.iter().all(|t| *t == target),
        "{targets:?}"
    );
    // No proxy (an empty remote.<name>.proxy): direct.
    let before = proxy.targets().len();
    r.proxy = None;
    let t = http(&server, &mut r);
    let third = Setup::new();
    t.download(&p, &third.cache, REF).unwrap();
    assert_eq!(proxy.targets().len(), before);
    // A proxy that is not there: unreachable.
    let closed = ClosedPort::new();
    r.proxy = Some(crate::proxy::ProxySetting {
        url: format!("http://127.0.0.1:{}", closed.port()),
        key: "http.proxy".into(),
    });
    let t = http(&server, &mut r);
    let err = t
        .upload(&[s.object(b"another")], &s.cache, REF)
        .unwrap_err();
    assert_eq!(err.kind, PushFailureKind::Unreachable, "{err}");
}

/// A POST that gets no answer never prints the URL's userinfo, whichever
/// webhook made it (bead agentfs-5sg, fix round).
#[test]
fn a_failed_post_never_prints_the_urls_password() {
    let closed = ClosedPort::new();
    let url = format!("http://u:s3cret@127.0.0.1:{}/hook", closed.port());
    let e = post(&url, b"{}", &[], Duration::from_secs(5)).unwrap_err();
    assert!(matches!(e, PostError::Unreachable { .. }), "{e}");
    let shown = e.to_string();
    assert!(!shown.contains("s3cret"), "{shown}");
    assert!(shown.contains("http://***@127.0.0.1:"), "{shown}");
}
