//! Isolated tests for #78: `claims`/`ack`/`claim`/`health` routing through a
//! configured remote hub, ordered-candidate fallback, and the explicit
//! offline behavior (never a silent local fallback presented as fleet
//! state).
//!
//! No live Redis/PostgreSQL/HTTP hub is used. The "hub" is a minimal,
//! single-purpose HTTP/1.1 mock server spawned in-process on `127.0.0.1:0`
//! (an ephemeral port) that answers canned JSON for the handful of routes
//! these tests exercise. Every subprocess additionally pins
//! `AGENT_BUS_REDIS_URL`/`AGENT_BUS_DATABASE_URL` to a closed port and an
//! isolated `AGENT_BUS_CONFIG`, so even a regression that silently
//! reintroduced a local fallback would fail loudly here rather than quietly
//! reaching a real backend.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

/// A canned response for one `(method, path)` pair.
struct MockRoute {
    method: &'static str,
    path: &'static str,
    status: u16,
    body: String,
}

/// A minimal HTTP/1.1 server matching exact `(method, path)` pairs from a
/// fixed table, ignoring query strings. Good enough to stand in for
/// `agent-bus-http` in these routing tests; not a general-purpose mock.
///
/// Uses a non-blocking accept loop with an explicit stop flag (rather than
/// blocking `accept()`) so `Drop` can always join its thread promptly —
/// otherwise, once the last request has been served, the server thread
/// would block in `accept()` forever with nothing left to unblock it, and
/// every subsequent test in this binary would hang waiting to join it.
struct MockHub {
    addr: SocketAddr,
    handle: Option<JoinHandle<()>>,
    hit_count: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl MockHub {
    fn spawn(routes: Vec<MockRoute>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock hub listener");
        listener
            .set_nonblocking(true)
            .expect("set mock hub listener non-blocking");
        let addr = listener.local_addr().expect("mock hub local addr");
        let hit_count = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let counter = Arc::clone(&hit_count);
        let stop_flag = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !stop_flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        counter.fetch_add(1, Ordering::SeqCst);
                        serve_one(stream, &routes);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            addr,
            handle: Some(handle),
            hit_count,
            stop,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn hits(&self) -> usize {
        self.hit_count.load(Ordering::SeqCst)
    }
}

impl Drop for MockHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            // The accept loop polls the stop flag at most every 5ms, so this
            // always returns promptly.
            let _ = handle.join();
        }
    }
}

fn serve_one(mut stream: TcpStream, routes: &[MockRoute]) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let full_path = parts.next().unwrap_or("").to_owned();
    let path = full_path.split('?').next().unwrap_or("").to_owned();

    let mut content_length: usize = 0;
    loop {
        let mut header_line = String::new();
        if reader.read_line(&mut header_line).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = header_line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    if content_length > 0 {
        let mut body = vec![0_u8; content_length];
        let _ = reader.read_exact(&mut body);
    }

    let route = routes.iter().find(|r| r.method == method && r.path == path);
    let response = if let Some(r) = route {
        format!(
            "HTTP/1.1 {} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            r.status,
            r.body.len(),
            r.body
        )
    } else {
        let body = format!(r#"{{"error": "no mock route for {method} {path}"}}"#);
        format!(
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    };
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn isolated_config_path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "agent-bus-hub-routing-test-{label}-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ))
}

/// A CLI subprocess with `server_urls` set to `candidates`, Redis/PG pinned
/// to a closed port (127.0.0.1:1 — nothing listens there), and an isolated
/// config file. Never touches a real backend of any kind.
fn agent_bus_with_hub_candidates(candidates: &[String], label: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-bus"));
    cmd.env("AGENT_BUS_CONFIG", isolated_config_path(label));
    cmd.env("AGENT_BUS_SERVER_URLS", candidates.join(","));
    cmd.env_remove("AGENT_BUS_SERVER_URL");
    cmd.env("AGENT_BUS_REDIS_URL", "redis://127.0.0.1:1/0");
    cmd.env(
        "AGENT_BUS_DATABASE_URL",
        "postgresql://postgres@127.0.0.1:1/none",
    );
    cmd
}

/// Every command first resolves the hub backend via a `/health` probe
/// (`active_hub_backend`), independent of whatever route the command itself
/// needs — every mock hub used by a "remote candidate answers" test needs
/// this route or `resolve_hub` will treat it as unreachable.
fn health_route() -> MockRoute {
    MockRoute {
        method: "GET",
        path: "/health",
        status: 200,
        body: r#"{"ok": true, "database_ok": true, "storage_ready": true, "build_version": "0.5.0 (test)"}"#
            .to_owned(),
    }
}

fn stdout_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// ---------------------------------------------------------------------------
// claims — remote routing (#78)
// ---------------------------------------------------------------------------

#[test]
fn claims_routes_through_the_authoritative_first_candidate() {
    let hub = MockHub::spawn(vec![
        health_route(),
        MockRoute {
            method: "GET",
            path: "/claims",
            status: 200,
            body: r#"{"claims": [{"resource": "file.txt", "agent": "codex", "status": "granted"}], "count": 1}"#.to_owned(),
        },
    ]);

    let output = agent_bus_with_hub_candidates(&[hub.url()], "claims-remote")
        .args(["claims", "--encoding", "json"])
        .output()
        .expect("run agent-bus claims");

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout_of(&output),
        stderr_of(&output)
    );
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("codex"),
        "expected remote claim in output: {stdout}"
    );
    assert!(hub.hits() >= 1, "mock hub was never contacted");
}

#[test]
fn claims_falls_over_to_the_second_candidate_when_the_first_is_dead() {
    let hub = MockHub::spawn(vec![
        health_route(),
        MockRoute {
            method: "GET",
            path: "/claims",
            status: 200,
            body: r#"{"claims": [], "count": 0}"#.to_owned(),
        },
    ]);

    // 127.0.0.1:1 is closed (privileged/unassigned), so it fails fast.
    let output = agent_bus_with_hub_candidates(
        &["http://127.0.0.1:1".to_owned(), hub.url()],
        "claims-fallback",
    )
    .args(["claims", "--encoding", "json"])
    .output()
    .expect("run agent-bus claims");

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(
        hub.hits() >= 1,
        "the reachable second candidate must have been used"
    );
}

#[test]
fn claims_refuses_loudly_when_every_candidate_is_offline() {
    let output = agent_bus_with_hub_candidates(
        &[
            "http://127.0.0.1:1".to_owned(),
            "http://127.0.0.1:2".to_owned(),
        ],
        "claims-offline",
    )
    .args(["claims", "--encoding", "json"])
    .output()
    .expect("run agent-bus claims");

    assert!(
        !output.status.success(),
        "offline claims must exit non-zero"
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("offline: no authoritative hub reachable"),
        "expected the explicit offline error, got: {stderr}"
    );
    // Negative control: the pre-#78 behavior (always read local Redis) would
    // fail with a Redis/connection error instead, never mentioning "offline"
    // or the tried candidate list — proving this is the new code path, not
    // an incidental failure.
    assert!(
        !stderr.to_lowercase().contains("redis"),
        "must not mention Redis at all -- that would mean it fell back to a local read: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// ack — remote routing (#78)
// ---------------------------------------------------------------------------

#[test]
fn ack_routes_through_the_remote_hub() {
    let hub = MockHub::spawn(vec![
        health_route(),
        MockRoute {
            method: "POST",
            path: "/messages/msg-123/ack",
            status: 200,
            body: r#"{"ack_sent": true, "ack_message_id": "ack-1", "acked_message_id": "msg-123"}"#
                .to_owned(),
        },
    ]);

    let output = agent_bus_with_hub_candidates(&[hub.url()], "ack-remote")
        .args([
            "ack",
            "--agent",
            "codex",
            "--message-id",
            "msg-123",
            "--encoding",
            "json",
        ])
        .output()
        .expect("run agent-bus ack");

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout_of(&output),
        stderr_of(&output)
    );
    assert!(stdout_of(&output).contains("ack_sent"));
    assert!(hub.hits() >= 1);
}

#[test]
fn ack_refuses_loudly_when_offline_instead_of_writing_locally() {
    let output = agent_bus_with_hub_candidates(&["http://127.0.0.1:1".to_owned()], "ack-offline")
        .args([
            "ack",
            "--agent",
            "codex",
            "--message-id",
            "msg-123",
            "--encoding",
            "json",
        ])
        .output()
        .expect("run agent-bus ack");

    assert!(!output.status.success());
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("offline: no authoritative hub reachable"),
        "got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// claim — offline must say "claim pending", never a silent local grant (#78)
// ---------------------------------------------------------------------------

#[test]
fn claim_reports_pending_not_a_hard_failure_when_offline() {
    let output = agent_bus_with_hub_candidates(&["http://127.0.0.1:1".to_owned()], "claim-offline")
        .args([
            "claim",
            "file.txt",
            "--agent",
            "codex",
            "--encoding",
            "json",
        ])
        .output()
        .expect("run agent-bus claim");

    assert!(!output.status.success());
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("claim pending: no authoritative hub reachable"),
        "operator-specified exact wording missing, got: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// health / bus_health-equivalent backend reporting (#78)
// ---------------------------------------------------------------------------

#[test]
fn health_reports_remote_backend_with_hub_build() {
    let hub = MockHub::spawn(vec![MockRoute {
        method: "GET",
        path: "/health",
        status: 200,
        body: r#"{"ok": true, "database_ok": true, "storage_ready": true, "build_version": "0.5.0 (75ec8f8)"}"#
            .to_owned(),
    }]);

    let output = agent_bus_with_hub_candidates(&[hub.url()], "health-remote")
        .args(["health", "--encoding", "json"])
        .output()
        .expect("run agent-bus health");

    let stdout = stdout_of(&output);
    assert!(
        output.status.success(),
        "stdout: {stdout}\nstderr: {}",
        stderr_of(&output)
    );
    assert!(
        stdout.contains("\"mode\":\"remote\"") || stdout.contains("\"mode\": \"remote\""),
        "got: {stdout}"
    );
    assert!(
        stdout.contains("75ec8f8"),
        "hub build must be surfaced: {stdout}"
    );
    assert!(
        stdout.contains("\"authoritative\":true") || stdout.contains("\"authoritative\": true"),
        "got: {stdout}"
    );
}

#[test]
fn health_reports_offline_explicitly_and_exits_non_zero() {
    let output =
        agent_bus_with_hub_candidates(&["http://127.0.0.1:1".to_owned()], "health-offline")
            .args(["health", "--encoding", "json"])
            .output()
            .expect("run agent-bus health");

    assert!(
        !output.status.success(),
        "an offline hub must never report healthy"
    );
    let stdout = stdout_of(&output);
    assert!(
        stdout.contains("\"mode\":\"offline\"") || stdout.contains("\"mode\": \"offline\""),
        "got: {stdout}"
    );
    assert!(
        stdout.contains("\"ok\":false") || stdout.contains("\"ok\": false"),
        "got: {stdout}"
    );
}

/// Negative control: local-only mode (no `server_urls` configured at all)
/// takes an entirely different, pre-existing code path. It still fails
/// against the closed-port Redis, but with a connection error, never the
/// #78 "offline: no authoritative hub reachable" wording -- proving the
/// two code paths are genuinely distinct rather than one silently
/// swallowing the other's identity.
#[test]
fn local_only_mode_never_reports_the_offline_hub_wording() {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-bus"));
    cmd.env("AGENT_BUS_CONFIG", isolated_config_path("local-only"));
    cmd.env_remove("AGENT_BUS_SERVER_URL");
    cmd.env_remove("AGENT_BUS_SERVER_URLS");
    cmd.env("AGENT_BUS_REDIS_URL", "redis://127.0.0.1:1/0");
    cmd.env(
        "AGENT_BUS_DATABASE_URL",
        "postgresql://postgres@127.0.0.1:1/none",
    );
    let output = cmd
        .args(["health", "--encoding", "json"])
        .output()
        .expect("run agent-bus health");

    assert!(
        !output.status.success(),
        "closed-port local Redis must fail health"
    );
    let combined = format!("{}{}", stdout_of(&output), stderr_of(&output));
    assert!(
        !combined.contains("offline: no authoritative hub reachable"),
        "local-only mode must not report the remote-hub offline wording: {combined}"
    );
    assert!(
        combined.contains("\"mode\":\"local\"") || combined.contains("\"mode\": \"local\""),
        "local-only mode must explicitly report backend mode local: {combined}"
    );
}
