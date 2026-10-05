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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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
    milestones: Arc<Mutex<Vec<String>>>,
    started_at: Instant,
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
        let milestones = Arc::new(Mutex::new(Vec::new()));
        let thread_milestones = Arc::clone(&milestones);
        let started_at = Instant::now();
        let handle = std::thread::spawn(move || {
            while !stop_flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        counter.fetch_add(1, Ordering::SeqCst);
                        record_milestone(&thread_milestones, started_at, "accept");
                        if let Err(error) =
                            serve_one(stream, &routes, &thread_milestones, started_at)
                        {
                            record_milestone(
                                &thread_milestones,
                                started_at,
                                &format!("socket-error {:?}", error.kind()),
                            );
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => {
                        record_milestone(
                            &thread_milestones,
                            started_at,
                            &format!("accept-error {:?}", error.kind()),
                        );
                        break;
                    }
                }
            }
        });
        Self {
            addr,
            handle: Some(handle),
            hit_count,
            stop,
            milestones,
            started_at,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn hits(&self) -> usize {
        self.hit_count.load(Ordering::SeqCst)
    }

    fn diagnostics(&self) -> Vec<String> {
        self.milestones
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Drop for MockHub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            // Idle accepts poll every 5ms. An active transaction has a total
            // two-second I/O budget and returns on incomplete input/errors.
            if handle.join().is_err() {
                record_milestone(&self.milestones, self.started_at, "thread-panicked");
            }
        }
    }
}

fn record_milestone(milestones: &Mutex<Vec<String>>, started_at: Instant, detail: &str) {
    milestones
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(format!("{:?}: {detail}", started_at.elapsed()));
}

fn remaining_io_budget(deadline: Instant) -> std::io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::TimedOut))
}

fn read_fixture_line(
    reader: &mut BufReader<TcpStream>,
    deadline: Instant,
) -> std::io::Result<String> {
    let mut line = Vec::new();
    loop {
        reader
            .get_ref()
            .set_read_timeout(Some(remaining_io_budget(deadline)?))?;
        let chunk = reader.fill_buf()?;
        if chunk.is_empty() {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        let count = chunk
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(chunk.len(), |position| position + 1);
        if line.len() + count > 8192 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let complete = chunk[count - 1] == b'\n';
        line.extend_from_slice(&chunk[..count]);
        reader.consume(count);
        if complete {
            return String::from_utf8(line)
                .map_err(|_error| std::io::ErrorKind::InvalidData.into());
        }
    }
}

fn consume_fixture_body(
    reader: &mut BufReader<TcpStream>,
    deadline: Instant,
    content_length: usize,
) -> std::io::Result<()> {
    if content_length > 65_536 {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut buffer = [0_u8; 1024];
    let mut remaining = content_length;
    while remaining > 0 {
        reader
            .get_ref()
            .set_read_timeout(Some(remaining_io_budget(deadline)?))?;
        let count = remaining.min(buffer.len());
        let read = reader.read(&mut buffer[..count])?;
        if read == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        remaining -= read;
    }
    Ok(())
}

fn write_fixture_response(
    stream: &mut TcpStream,
    deadline: Instant,
    response: &[u8],
) -> std::io::Result<()> {
    let mut written = 0;
    while written < response.len() {
        stream.set_write_timeout(Some(remaining_io_budget(deadline)?))?;
        let count = stream.write(&response[written..])?;
        if count == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        written += count;
    }
    stream.set_write_timeout(Some(remaining_io_budget(deadline)?))?;
    stream.flush()
}

fn serve_one(
    mut stream: TcpStream,
    routes: &[MockRoute],
    milestones: &Mutex<Vec<String>>,
    started_at: Instant,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut reader = BufReader::new(stream.try_clone()?);
    let request_line = read_fixture_line(&mut reader, deadline)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_owned();
    let full_path = parts.next().unwrap_or("").to_owned();
    let path = full_path.split('?').next().unwrap_or("").to_owned();
    let route = routes.iter().find(|r| r.method == method && r.path == path);
    let request_description = route.map_or_else(
        || "request <unmatched>".to_owned(),
        |matched| format!("request {} {}", matched.method, matched.path),
    );
    record_milestone(milestones, started_at, &request_description);

    let mut content_length: usize = 0;
    let mut header_count = 0;
    loop {
        let header_line = read_fixture_line(&mut reader, deadline)?;
        let trimmed = header_line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        header_count += 1;
        if header_count > 64 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        if let Some((name, value)) = trimmed.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value
                .trim()
                .parse()
                .map_err(|_error| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
        }
    }
    consume_fixture_body(&mut reader, deadline, content_length)?;

    let response = if let Some(r) = route {
        format!(
            "HTTP/1.1 {} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            r.status,
            r.body.len(),
            r.body
        )
    } else {
        let body = r#"{"error": "no mock route"}"#;
        format!(
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    };
    write_fixture_response(&mut stream, deadline, response.as_bytes())?;
    record_milestone(milestones, started_at, "response-complete");
    Ok(())
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
fn isolated_agent_bus(label: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-bus"));
    cmd.env("AGENT_BUS_CONFIG", isolated_config_path(label));
    cmd.env_remove("AGENT_BUS_SERVER_URL");
    cmd.env_remove("AGENT_BUS_SERVER_URLS");
    cmd.env_remove("AGENT_BUS_SERVER_CANDIDATES");
    cmd.env_remove("AGENT_BUS_AUTH_TOKEN");
    cmd.env("AGENT_BUS_HUB_CACHE_TTL_SECONDS", "0");
    cmd.env("AGENT_BUS_PROBE_CONNECT_TIMEOUT_MS", "750");
    cmd.env("AGENT_BUS_STARTUP_ENABLED", "false");
    for variable in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        cmd.env_remove(variable);
    }
    cmd.env("NO_PROXY", "localhost,127.0.0.1,::1");
    cmd.env("no_proxy", "localhost,127.0.0.1,::1");
    cmd.env("AGENT_BUS_REDIS_URL", "redis://127.0.0.1:1/0");
    cmd.env(
        "AGENT_BUS_DATABASE_URL",
        "postgresql://postgres@127.0.0.1:1/none",
    );
    cmd
}

fn agent_bus_with_hub_candidates(candidates: &[String], label: &str) -> Command {
    let mut cmd = isolated_agent_bus(label);
    cmd.env("AGENT_BUS_SERVER_URLS", candidates.join(","));
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

#[cfg(feature = "server-mode")]
#[test]
fn service_status_reports_verified_admin_tier() {
    let hub = MockHub::spawn(vec![MockRoute {
        method: "GET",
        path: "/admin/service",
        status: 200,
        body: r#"{"paused":false,"fixture":"verified-admin"}"#.to_owned(),
    }]);
    let output = isolated_agent_bus("service-status-positive")
        .args([
            "service",
            "--action",
            "status",
            "--base-url",
            &hub.url(),
            "--service-name",
            "agent-bus-disposable-missing-fixture",
            "--encoding",
            "json",
        ])
        .output()
        .expect("run service status");
    assert!(output.status.success(), "{}", stderr_of(&output));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("status JSON");
    assert_eq!(report["tier"], "server_admin");
    assert_eq!(report["admin"]["fixture"], "verified-admin");
    assert_eq!(hub.hits(), 1);
}

#[cfg(not(feature = "server-mode"))]
#[test]
fn service_status_without_server_mode_reports_metadata_without_http_probe() {
    let hub = MockHub::spawn(vec![MockRoute {
        method: "GET",
        path: "/admin/service",
        status: 200,
        body: r#"{"fixture":"must-not-be-fetched"}"#.to_owned(),
    }]);
    let output = isolated_agent_bus("service-status-feature-disabled")
        .args([
            "service",
            "--action",
            "status",
            "--base-url",
            &hub.url(),
            "--service-name",
            "agent-bus-disposable-missing-fixture",
            "--encoding",
            "json",
        ])
        .output()
        .expect("run feature-disabled service status");
    assert!(output.status.success(), "{}", stderr_of(&output));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("status JSON");
    assert_eq!(report["tier"], "server_mode_unavailable");
    assert!(report["admin"].is_null());
    assert_eq!(hub.hits(), 0);
}

#[cfg(feature = "server-mode")]
#[test]
fn service_status_refuses_rejected_admin_access() {
    let hub = MockHub::spawn(vec![MockRoute {
        method: "GET",
        path: "/admin/service",
        status: 401,
        body: r#"{"error":"fixture unauthorized"}"#.to_owned(),
    }]);
    let output = isolated_agent_bus("service-status-rejected")
        .args([
            "service",
            "--action",
            "status",
            "--base-url",
            &hub.url(),
            "--service-name",
            "agent-bus-disposable-missing-fixture",
            "--encoding",
            "json",
        ])
        .output()
        .expect("run rejected service status");
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "failure must not emit a success report"
    );
    let diagnostic = stderr_of(&output);
    assert!(
        diagnostic.contains("server-mode service status admin fetch failed"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("401"), "{diagnostic}");
    assert_eq!(hub.hits(), 1);
}

#[cfg(feature = "server-mode")]
#[test]
fn service_status_refuses_rejected_configured_authority_without_base_url_override() {
    let hub = MockHub::spawn(vec![MockRoute {
        method: "GET",
        path: "/admin/service",
        status: 401,
        body: r#"{"error":"fixture unauthorized"}"#.to_owned(),
    }]);
    let output = agent_bus_with_hub_candidates(&[hub.url()], "service-status-configured")
        .args([
            "service",
            "--action",
            "status",
            "--service-name",
            "agent-bus-disposable-missing-fixture",
            "--encoding",
            "json",
        ])
        .output()
        .expect("run configured-authority service status");
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let diagnostic = stderr_of(&output);
    assert!(
        diagnostic.contains("server-mode service status admin fetch failed"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("401"), "{diagnostic}");
    assert_eq!(hub.hits(), 1);
}

#[cfg(feature = "server-mode")]
#[test]
fn service_status_refuses_unreachable_admin_endpoint() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve disposable closed endpoint");
    let base_url = format!("http://{}", listener.local_addr().expect("fixture address"));
    drop(listener);
    let output = isolated_agent_bus("service-status-unreachable")
        .args([
            "service",
            "--action",
            "status",
            "--base-url",
            &base_url,
            "--service-name",
            "agent-bus-disposable-missing-fixture",
            "--encoding",
            "json",
        ])
        .output()
        .expect("run unreachable service status");
    assert!(!output.status.success());
    assert!(
        output.stdout.is_empty(),
        "failure must not emit a success report"
    );
    let diagnostic = stderr_of(&output);
    assert!(
        diagnostic.contains("server-mode service status admin fetch failed"),
        "{diagnostic}"
    );
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
    let mut cmd = isolated_agent_bus("local-only");
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

// ---------------------------------------------------------------------------
// send — fallover coverage (PR #81 review item 2)
// ---------------------------------------------------------------------------

#[test]
fn send_falls_over_to_the_second_candidate_when_the_first_is_dead() {
    let hub = MockHub::spawn(vec![
        health_route(),
        MockRoute {
            method: "POST",
            path: "/messages",
            status: 200,
            body: r#"{"id": "msg-1", "sender": "codex", "recipient": "all", "topic": "status", "body": "hi"}"#
                .to_owned(),
        },
    ]);

    // 127.0.0.1:1 is closed, so `send` must fail over to the live second
    // candidate instead of erroring out on the first dead one.
    let output = agent_bus_with_hub_candidates(
        &["http://127.0.0.1:1".to_owned(), hub.url()],
        "send-fallback",
    )
    .args([
        "send",
        "--from-agent",
        "codex",
        "--to-agent",
        "all",
        "--topic",
        "status",
        "--body",
        "hi",
        "--encoding",
        "json",
    ])
    .output()
    .expect("run agent-bus send");

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}\nTCP accepts: {}\nmock milestones: {:?}",
        stdout_of(&output),
        stderr_of(&output),
        hub.hits(),
        hub.diagnostics()
    );
    assert!(
        hub.hits() >= 1,
        "the reachable second candidate must have been used; mock milestones: {:?}",
        hub.diagnostics()
    );
}

#[test]
fn send_refuses_loudly_when_every_candidate_is_offline() {
    let output = agent_bus_with_hub_candidates(&["http://127.0.0.1:1".to_owned()], "send-offline")
        .args([
            "send",
            "--from-agent",
            "codex",
            "--to-agent",
            "all",
            "--topic",
            "status",
            "--body",
            "hi",
            "--encoding",
            "json",
        ])
        .output()
        .expect("run agent-bus send");

    assert!(!output.status.success());
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("offline: no authoritative hub reachable"),
        "got: {stderr}"
    );
    assert!(
        !stderr.to_lowercase().contains("redis"),
        "must not fall back to a local read/write: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// claim — authoritative-only grant (PR #81 review item 4)
// ---------------------------------------------------------------------------

#[test]
fn claim_reports_pending_when_only_a_non_authoritative_candidate_answers() {
    // The FIRST (authoritative) candidate is a dead port; the SECOND
    // candidate is a live mock hub that would happily grant the claim if
    // asked. A claim must never be granted against a non-authoritative
    // fallback -- it must report "claim pending" exactly as it would if
    // nothing answered at all, and must never even reach the second
    // candidate's /channels/arbitrate route.
    let hub = MockHub::spawn(vec![
        health_route(),
        MockRoute {
            method: "POST",
            path: "/channels/arbitrate/file.txt",
            status: 200,
            body: r#"{"resource": "file.txt", "status": "granted"}"#.to_owned(),
        },
    ]);

    let output = agent_bus_with_hub_candidates(
        &["http://127.0.0.1:1".to_owned(), hub.url()],
        "claim-non-authoritative",
    )
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

    assert!(
        !output.status.success(),
        "a non-authoritative candidate must never grant a claim"
    );
    let stderr = stderr_of(&output);
    assert!(
        stderr.contains("claim pending: no authoritative hub reachable"),
        "got: {stderr}"
    );
    // Claims resolve the authoritative role directly, without probing a
    // fallback for unrelated reads. Neither health nor arbitrate may reach
    // this non-authoritative candidate, even when it would grant the claim.
    assert_eq!(
        hub.hits(),
        0,
        "authoritative-only claims must make no request to a fallback candidate"
    );
}
