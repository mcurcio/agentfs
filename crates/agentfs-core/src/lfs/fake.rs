//! An in-process Git LFS server for tests: the batch API, basic uploads,
//! downloads and verify, on two loopback ports (the endpoint's, and
//! another one action hrefs can point to), recording every request.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// A request the server saw.
#[derive(Debug, Clone)]
pub(crate) struct Seen {
    pub port: u16,
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
}

impl Seen {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// What the server holds and how it behaves.
#[derive(Debug, Default)]
pub(crate) struct State {
    pub objects: HashMap<String, Vec<u8>>,
    pub seen: Vec<Seen>,
    pub verified: Vec<String>,
    /// The `Authorization` a batch request must carry, if any.
    pub require_auth: Option<String>,
    /// Action hrefs point at the second port.
    pub actions_elsewhere: bool,
    /// Headers put in every action.
    pub action_headers: BTreeMap<String, String>,
    /// Offer `verify` after uploads.
    pub verify: bool,
    /// A per-object error for every object: `(code, message)`.
    pub object_error: Option<(i64, String)>,
    /// Answer every batch with this status.
    pub batch_status: Option<u16>,
    /// Answer every batch with a redirect.
    pub redirect: bool,
    /// Serve downloads with one byte changed.
    pub corrupt_downloads: bool,
}

pub(crate) struct FakeLfs {
    pub port: u16,
    pub other_port: u16,
    pub state: Arc<Mutex<State>>,
}

impl FakeLfs {
    pub fn start() -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let main = TcpListener::bind("127.0.0.1:0").unwrap();
        let other = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = main.local_addr().unwrap().port();
        let other_port = other.local_addr().unwrap().port();
        for listener in [main, other] {
            let state = state.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    let state = state.clone();
                    std::thread::spawn(move || serve(stream, &state, port, other_port));
                }
            });
        }
        Self {
            port,
            other_port,
            state,
        }
    }

    /// The endpoint URL: `http://127.0.0.1:<port>/repo/info/lfs`.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/repo/info/lfs", self.port)
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.state.lock().unwrap().seen.clone()
    }

    pub fn object(&self, oid: &str) -> Option<Vec<u8>> {
        self.state.lock().unwrap().objects.get(oid).cloned()
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

fn json(status: u16, value: serde_json::Value) -> Response {
    Response {
        status,
        headers: vec![("Content-Type".into(), "application/vnd.git-lfs+json".into())],
        body: serde_json::to_vec(&value).unwrap(),
    }
}

fn serve(stream: TcpStream, state: &Mutex<State>, port: u16, other_port: u16) {
    let local_port = stream.local_addr().map(|a| a.port()).unwrap_or(0);
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let path = parts.next().unwrap_or("").to_owned();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 {
            return;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_owned(), v.trim().to_owned()));
        }
    }
    let seen = Seen {
        port: local_port,
        method: method.clone(),
        path: path.clone(),
        headers,
    };
    let len: usize = seen
        .header("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let response = {
        let mut state = state.lock().unwrap();
        state.seen.push(seen.clone());
        route(&mut state, &seen, &body, port, other_port)
    };
    let mut out = stream;
    let mut head = format!("HTTP/1.1 {} X\r\n", response.status);
    for (k, v) in &response.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        response.body.len()
    ));
    let _ = out.write_all(head.as_bytes());
    let _ = out.write_all(&response.body);
    let _ = out.flush();
}

fn route(state: &mut State, seen: &Seen, body: &[u8], port: u16, other_port: u16) -> Response {
    let base = format!(
        "http://127.0.0.1:{}",
        if state.actions_elsewhere {
            other_port
        } else {
            port
        }
    );
    if seen.method == "POST" && seen.path.ends_with("/objects/batch") {
        if state.redirect {
            return Response {
                status: 302,
                headers: vec![("Location".into(), format!("{base}/elsewhere/objects/batch"))],
                body: Vec::new(),
            };
        }
        if let Some(status) = state.batch_status {
            return json(status, serde_json::json!({"message": "the server says no"}));
        }
        if let Some(expected) = &state.require_auth
            && seen.header("authorization") != Some(expected.as_str())
        {
            return json(401, serde_json::json!({"message": "Credentials needed"}));
        }
        let request: serde_json::Value = serde_json::from_slice(body).unwrap();
        let operation = request["operation"].as_str().unwrap_or("").to_owned();
        let header = state.action_headers.clone();
        let objects: Vec<serde_json::Value> = request["objects"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|o| {
                let oid = o["oid"].as_str().unwrap_or("").to_owned();
                let size = o["size"].as_u64().unwrap_or(0);
                if let Some((code, message)) = &state.object_error {
                    return serde_json::json!({"oid": oid, "size": size,
                        "error": {"code": code, "message": message}});
                }
                let present = state.objects.contains_key(&oid);
                match (operation.as_str(), present) {
                    ("upload", true) => serde_json::json!({"oid": oid, "size": size}),
                    ("upload", false) => {
                        let mut actions = serde_json::json!({
                            "upload": {"href": format!("{base}/upload/{oid}"), "header": header}
                        });
                        if state.verify {
                            actions["verify"] = serde_json::json!(
                                {"href": format!("{base}/verify"), "header": header});
                        }
                        serde_json::json!({"oid": oid, "size": size, "actions": actions})
                    }
                    ("download", true) => serde_json::json!({"oid": oid, "size": size,
                        "actions": {"download":
                            {"href": format!("{base}/download/{oid}"), "header": header}}}),
                    _ => serde_json::json!({"oid": oid, "size": size,
                        "error": {"code": 404, "message": "Object does not exist"}}),
                }
            })
            .collect();
        return json(
            200,
            serde_json::json!({"transfer": "basic", "objects": objects, "hash_algo": "sha256"}),
        );
    }
    if seen.method == "PUT"
        && let Some(oid) = seen.path.strip_prefix("/upload/")
    {
        state.objects.insert(oid.to_owned(), body.to_vec());
        return json(200, serde_json::json!({}));
    }
    if seen.method == "POST" && seen.path == "/verify" {
        let request: serde_json::Value = serde_json::from_slice(body).unwrap();
        let oid = request["oid"].as_str().unwrap_or("").to_owned();
        let ok = state
            .objects
            .get(&oid)
            .is_some_and(|b| Some(b.len() as u64) == request["size"].as_u64());
        state.verified.push(oid);
        return json(if ok { 200 } else { 422 }, serde_json::json!({}));
    }
    if seen.method == "GET"
        && let Some(oid) = seen.path.strip_prefix("/download/")
        && let Some(content) = state.objects.get(oid)
    {
        let mut content = content.clone();
        if state.corrupt_downloads && !content.is_empty() {
            content[0] ^= 1;
        }
        return Response {
            status: 200,
            headers: vec![("Content-Type".into(), "application/octet-stream".into())],
            body: content,
        };
    }
    json(404, serde_json::json!({"message": "not found"}))
}
