//! A blocking mock HTTP server for the `br remote` tests.
//!
//! Every `br remote` test runs against this rather than the network. It
//! records the exact ordered sequence of requests it received, because most of
//! what this project needs to assert is *which calls were issued* — that a
//! reconciled workspace issues zero writes, that paging asked for the right
//! `$skip`, that a removal was addressed by internal id rather than by
//! `idReadable`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// One request as the server saw it.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    /// Path including the query string — `fields=`, `$top` and `$skip` all
    /// live there, and several tests assert on them.
    pub path: String,
    pub body: String,
    pub auth: Option<String>,
}

type RouteKey = (String, String);

#[derive(Default)]
struct State {
    routes: HashMap<RouteKey, Vec<(u16, String)>>,
    cursors: HashMap<RouteKey, usize>,
    recorded: Vec<RecordedRequest>,
    /// Route → serve this many normal responses, then close without replying.
    drop_after: HashMap<RouteKey, usize>,
}

pub struct MockServer {
    addr: std::net::SocketAddr,
    state: Arc<Mutex<State>>,
    shutdown: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl MockServer {
    /// Bind `127.0.0.1:0` and serve until dropped.
    #[must_use]
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let state = Arc::new(Mutex::new(State::default()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let thread_state = Arc::clone(&state);
        let thread_shutdown = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_shutdown.load(Ordering::Relaxed) {
                    break;
                }
                match stream {
                    Ok(stream) => handle_connection(stream, &thread_state),
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            state,
            shutdown,
            handle: Some(handle),
        }
    }

    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Queue a single canned response for `method path`.
    pub fn on(&self, method: &str, path: &str, status: u16, body: &str) {
        self.on_sequence(method, path, vec![(status, body.to_string())]);
    }

    /// Queue successive responses for `method path`. Once exhausted the last
    /// response repeats, so a poll loop does not fall off the end.
    pub fn on_sequence(&self, method: &str, path: &str, responses: Vec<(u16, String)>) {
        let mut state = self.state.lock().expect("mock state");
        state
            .routes
            .insert((method.to_string(), path.to_string()), responses);
    }

    /// Every request received, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.state.lock().expect("mock state").recorded.clone()
    }

    /// How many requests used a state-changing method.
    #[must_use]
    pub fn write_requests(&self) -> Vec<RecordedRequest> {
        self.requests()
            .into_iter()
            .filter(|r| matches!(r.method.as_str(), "POST" | "PUT" | "DELETE" | "PATCH"))
            .collect()
    }

    /// Accept and record, then close the connection without a response.
    /// Used to simulate a mid-run interruption.
    pub fn on_drop(&self, method: &str, path: &str) {
        self.on_drop_after(method, path, 0);
    }

    /// Serve `n` normal responses for this route, then drop every request
    /// after them. This is how the first-run interruption test stops a run
    /// partway and then asserts the resume creates only the remainder.
    pub fn on_drop_after(&self, method: &str, path: &str, n: usize) {
        let mut state = self.state.lock().expect("mock state");
        state
            .drop_after
            .insert((method.to_string(), path.to_string()), n);
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // Unblock the accept loop with one throwaway connection.
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_connection(mut stream: TcpStream, state: &Arc<Mutex<State>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.trim().is_empty() {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut content_length = 0usize;
    let mut auth = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        } else if lower.starts_with("authorization:") {
            auth = line.split_once(':').map(|(_, v)| v.trim().to_string());
        }
    }

    let mut body = vec![0_u8; content_length];
    if content_length > 0 && reader.read_exact(&mut body).is_err() {
        return;
    }
    let body = String::from_utf8_lossy(&body).to_string();

    let outcome = {
        let mut state = state.lock().expect("mock state");
        state.recorded.push(RecordedRequest {
            method: method.clone(),
            path: path.clone(),
            body,
            auth,
        });
        let key = (method, path);

        // Count how many times this route has been hit so far, including now.
        let hits = state
            .recorded
            .iter()
            .filter(|r| r.method == key.0 && r.path == key.1)
            .count();
        if let Some(limit) = state.drop_after.get(&key)
            && hits > *limit
        {
            None
        } else {
            let responses = state.routes.get(&key).cloned().unwrap_or_default();
            if responses.is_empty() {
                Some((
                    404,
                    r#"{"error":"no canned response for this route"}"#.to_string(),
                ))
            } else {
                let cursor = state.cursors.entry(key).or_insert(0);
                let index = (*cursor).min(responses.len() - 1);
                *cursor += 1;
                Some(responses[index].clone())
            }
        }
    };

    let Some((status, payload)) = outcome else {
        // Close without writing anything: the client sees the connection go
        // away mid-request, which is what an interruption looks like.
        return;
    };

    let response = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    fn raw_request(base: &str, method: &str, path: &str, body: &str) -> String {
        let addr = base.trim_start_matches("http://");
        let mut stream = TcpStream::connect(addr).expect("connect");
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer t\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read");
        response
    }

    #[test]
    fn it_replays_a_canned_response_and_records_the_request() {
        let server = MockServer::start();
        server.on(
            "GET",
            "/api/issues?fields=id&$top=2",
            200,
            r#"[{"id":"3-1"}]"#,
        );

        let response = raw_request(
            &server.base_url(),
            "GET",
            "/api/issues?fields=id&$top=2",
            "",
        );
        assert!(response.starts_with("HTTP/1.1 200"), "got: {response}");
        assert!(response.contains(r#"[{"id":"3-1"}]"#), "got: {response}");

        let recorded = server.requests();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].method, "GET");
        assert_eq!(recorded[0].path, "/api/issues?fields=id&$top=2");
        assert_eq!(recorded[0].auth.as_deref(), Some("Bearer t"));
    }

    #[test]
    fn a_post_body_is_recorded_verbatim() {
        let server = MockServer::start();
        server.on("POST", "/api/issues", 200, r#"{"idReadable":"EM-1"}"#);

        let body = r#"{"summary":"probe","project":{"id":"0-1"}}"#;
        raw_request(&server.base_url(), "POST", "/api/issues", body);

        let recorded = server.requests();
        assert_eq!(recorded.len(), 1);
        assert_eq!(
            recorded[0].body, body,
            "body must be recorded byte-for-byte"
        );
    }

    #[test]
    fn an_unmatched_route_404s_and_is_still_recorded() {
        let server = MockServer::start();
        let response = raw_request(&server.base_url(), "GET", "/api/nope", "");
        assert!(response.starts_with("HTTP/1.1 404"), "got: {response}");
        // Recording the miss is the point: an assertion that fails should show
        // what was actually asked for, not an empty list.
        assert_eq!(server.requests().len(), 1);
        assert_eq!(server.requests()[0].path, "/api/nope");
    }

    #[test]
    fn a_sequence_advances_then_repeats_its_last_response() {
        let server = MockServer::start();
        server.on_sequence(
            "GET",
            "/api/count",
            vec![
                (200, r#"{"count":-1}"#.into()),
                (200, r#"{"count":0}"#.into()),
            ],
        );
        let first = raw_request(&server.base_url(), "GET", "/api/count", "");
        let second = raw_request(&server.base_url(), "GET", "/api/count", "");
        let third = raw_request(&server.base_url(), "GET", "/api/count", "");
        assert!(first.contains("-1"), "{first}");
        assert!(second.contains(r#""count":0"#), "{second}");
        assert!(
            third.contains(r#""count":0"#),
            "last response must repeat: {third}"
        );
    }

    #[test]
    fn two_servers_coexist_in_one_process() {
        let a = MockServer::start();
        let b = MockServer::start();
        assert_ne!(
            a.base_url(),
            b.base_url(),
            "port 0 must give distinct ports"
        );
    }

    #[test]
    fn a_dropped_route_closes_without_a_response() {
        let server = MockServer::start();
        server.on_drop("GET", "/api/issues");

        let addr = server.base_url().trim_start_matches("http://").to_string();
        let mut stream = TcpStream::connect(&addr).expect("connect");
        stream
            .write_all(b"GET /api/issues HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read");

        assert!(
            response.is_empty(),
            "a dropped route must write nothing, got: {response}"
        );
        assert_eq!(
            server.requests().len(),
            1,
            "the attempt must still be recorded"
        );
    }

    #[test]
    fn a_429_then_200_sequence_injects_exactly_one_retry_worth_of_fault() {
        let server = MockServer::start();
        server.on_sequence(
            "GET",
            "/api/issues",
            vec![
                (429, "rate limited".into()),
                (200, r#"[{"id":"3-1"}]"#.into()),
            ],
        );

        let first = raw_request(&server.base_url(), "GET", "/api/issues", "");
        let second = raw_request(&server.base_url(), "GET", "/api/issues", "");

        assert!(first.starts_with("HTTP/1.1 429"), "got: {first}");
        assert!(second.starts_with("HTTP/1.1 200"), "got: {second}");
        assert_eq!(server.requests().len(), 2);
    }

    #[test]
    fn each_youtrack_fixture_is_served_byte_identical() {
        use crate::youtrack_fixtures::{
            COUNT_PENDING, COUNT_SETTLED, DEFAULT_VALUES_BY_NAME, LINK_DELETE_NOT_FOUND,
            MUST_BE_UNIQUE,
        };

        let server = MockServer::start();
        server.on("POST", "/api/customFieldPrototypes", 409, MUST_BE_UNIQUE);
        server.on("POST", "/api/bundles/enum/b-1", 400, DEFAULT_VALUES_BY_NAME);
        server.on("DELETE", "/api/issueLinks/EM-5", 404, LINK_DELETE_NOT_FOUND);
        server.on_sequence(
            "POST",
            "/api/issuesGetter/count",
            vec![
                (200, COUNT_PENDING.to_string()),
                (200, COUNT_SETTLED.to_string()),
            ],
        );

        let must_be_unique =
            raw_request(&server.base_url(), "POST", "/api/customFieldPrototypes", "");
        assert!(
            must_be_unique.contains(MUST_BE_UNIQUE),
            "got: {must_be_unique}"
        );

        let default_values = raw_request(&server.base_url(), "POST", "/api/bundles/enum/b-1", "");
        assert!(
            default_values.contains(DEFAULT_VALUES_BY_NAME),
            "got: {default_values}"
        );

        let link_delete = raw_request(&server.base_url(), "DELETE", "/api/issueLinks/EM-5", "");
        assert!(
            link_delete.contains(LINK_DELETE_NOT_FOUND),
            "got: {link_delete}"
        );

        let count_pending = raw_request(&server.base_url(), "POST", "/api/issuesGetter/count", "");
        assert!(
            count_pending.contains(COUNT_PENDING),
            "got: {count_pending}"
        );
        let count_settled = raw_request(&server.base_url(), "POST", "/api/issuesGetter/count", "");
        assert!(
            count_settled.contains(COUNT_SETTLED),
            "got: {count_settled}"
        );
    }

    #[test]
    fn on_drop_after_serves_n_then_drops() {
        let server = MockServer::start();
        server.on("POST", "/api/issues", 200, r#"{"idReadable":"EM-1"}"#);
        server.on_drop_after("POST", "/api/issues", 2);

        let first = raw_request(&server.base_url(), "POST", "/api/issues", "{}");
        let second = raw_request(&server.base_url(), "POST", "/api/issues", "{}");
        let third = raw_request(&server.base_url(), "POST", "/api/issues", "{}");

        assert!(first.starts_with("HTTP/1.1 200"), "{first}");
        assert!(second.starts_with("HTTP/1.1 200"), "{second}");
        assert!(
            third.is_empty(),
            "the third call must be dropped, got: {third}"
        );
        assert_eq!(
            server.requests().len(),
            3,
            "all three attempts are recorded"
        );
    }
}
