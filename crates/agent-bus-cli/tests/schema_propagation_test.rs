//! Actual CLI transport requests captured by a disposable HTTP peer.
//! Stores are pinned to a closed port; no fleet backend or credentials are used.
#![cfg(feature = "server-mode")]

use std::io::{BufRead, Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

struct HttpPeer {
    url: String,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl HttpPeer {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        // Windows sockets accepted from a non-blocking listener inherit
                        // non-blocking mode; a read racing the client's write then
                        // fails with WouldBlock (#104). The other fixtures do the same.
                        socket.set_nonblocking(false).unwrap();
                        socket
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .unwrap();
                        let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        let post = line.starts_with("POST ");
                        let mut length = 0;
                        loop {
                            line.clear();
                            assert!(reader.read_line(&mut line).unwrap() > 0);
                            if line == "\r\n" {
                                break;
                            }
                            if let Some((name, value)) = line.split_once(':')
                                && name.eq_ignore_ascii_case("content-length")
                            {
                                length = value.trim().parse::<usize>().unwrap();
                            }
                        }
                        assert!(length < 300_000);
                        let mut bytes = vec![0; length];
                        reader.read_exact(&mut bytes).unwrap();
                        if post {
                            captured
                                .lock()
                                .unwrap()
                                .push(serde_json::from_slice(&bytes).unwrap());
                        }
                        let body = if post {
                            r#"{"ok":true,"sent":1,"ids":["fixture"]}"#
                        } else {
                            r#"{"ok":true,"database_ok":true,"storage_ready":true,"build_version":"fixture"}"#
                        };
                        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                }
            }
        });
        Self {
            url,
            requests,
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for HttpPeer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let joined = self.worker.take().map(JoinHandle::join);
        // Never panic again while a failing test is already unwinding: that
        // aborts the whole process (0xc0000409 on Windows) and hides the
        // original assertion.
        if !std::thread::panicking() {
            joined.transpose().expect("HTTP fixture worker panicked");
        }
    }
}

fn cli(peer: &HttpPeer, config: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-bus"));
    command
        .env("AGENT_BUS_CONFIG", config)
        .env("AGENT_BUS_SERVER_URL", &peer.url)
        .env("AGENT_BUS_REDIS_URL", "redis://127.0.0.1:1/0")
        .env(
            "AGENT_BUS_DATABASE_URL",
            "postgresql://postgres@127.0.0.1:1/offline",
        )
        .env("AGENT_BUS_STARTUP_ENABLED", "false")
        .env("AGENT_BUS_HUB_CACHE_TTL_SECONDS", "0");
    for name in [
        "AGENT_BUS_SERVER_URLS",
        "AGENT_BUS_SERVER_CANDIDATES",
        "AGENT_BUS_AUTH_TOKEN",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env_remove(name);
    }
    command
}

#[test]
fn actual_cli_http_payload_keeps_explicit_schema_instead_of_topic_reclassification() {
    let peer = HttpPeer::new();
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.json");
    std::fs::write(&config, "{}").unwrap();
    let output = cli(&peer, &config)
        .args([
            "send",
            "--from-agent",
            "test",
            "--to-agent",
            "all",
            "--topic",
            "review",
            "--schema",
            "status",
            "--body",
            "CLEAR",
            "--encoding",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let captured = peer.requests.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["schema"], "status");
    assert_eq!(captured[0]["body"], "CLEAR");
}

#[test]
fn actual_cli_http_payload_still_infers_when_schema_is_omitted() {
    let peer = HttpPeer::new();
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.json");
    std::fs::write(&config, "{}").unwrap();
    let output = cli(&peer, &config)
        .args([
            "send",
            "--from-agent",
            "test",
            "--to-agent",
            "all",
            "--topic",
            "review",
            "--body",
            "CLEAR",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let captured = peer.requests.lock().unwrap();
    assert_eq!(captured[0]["schema"], "finding");
    assert!(captured[0]["body"].as_str().unwrap().contains("FINDING:"));
}

#[test]
fn spool_creation_and_actual_http_batch_replay_keep_schema_and_original_body() {
    let peer = HttpPeer::new();
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.json");
    let spool = temp.path().join("messages.ndjson");
    std::fs::write(&config, "{}").unwrap();
    let output = cli(&peer, &config)
        .args([
            "spool-send",
            "--from-agent",
            "test",
            "--to-agent",
            "all",
            "--topic",
            "review",
            "--schema",
            "status",
            "--body",
            "CLEAR",
            "--spool",
        ])
        .arg(&spool)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let bytes = std::fs::read(&spool).unwrap();
    let record: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(record["schema"], "status");
    assert_eq!(record["body"], "CLEAR");
    let replay = cli(&peer, &config)
        .args(["spool-replay", "--spool"])
        .arg(&spool)
        .output()
        .unwrap();
    assert!(
        replay.status.success(),
        "{}",
        String::from_utf8_lossy(&replay.stderr)
    );
    let captured = peer.requests.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0]["messages"][0]["schema"], "status");
    assert_eq!(captured[0]["messages"][0]["body"], "CLEAR");
    assert_eq!(std::fs::read(&spool).unwrap(), bytes);
}

#[test]
fn invalid_spool_schema_rejects_before_creating_file_or_http_write() {
    let peer = HttpPeer::new();
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.json");
    let spool = temp.path().join("messages.ndjson");
    std::fs::write(&config, "{}").unwrap();
    let output = cli(&peer, &config)
        .args([
            "spool-send",
            "--from-agent",
            "test",
            "--to-agent",
            "all",
            "--topic",
            "review",
            "--schema",
            "bogus",
            "--body",
            "CLEAR",
            "--spool",
        ])
        .arg(&spool)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!spool.exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown schema"));
    assert!(peer.requests.lock().unwrap().is_empty());
}

#[test]
fn actual_cli_local_send_preserves_explicit_schema_in_redis_fields() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("redis://{}/0", listener.local_addr().unwrap());
    let worker = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
        let mut records = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                break;
            }
            assert!(line.starts_with('*'));
            let count: usize = line[1..].trim().parse().unwrap();
            let mut args = Vec::new();
            for _ in 0..count {
                line.clear();
                reader.read_line(&mut line).unwrap();
                assert!(line.starts_with('$'));
                let size: usize = line[1..].trim().parse().unwrap();
                let mut bytes = vec![0; size + 2];
                reader.read_exact(&mut bytes).unwrap();
                assert_eq!(&bytes[size..], b"\r\n");
                bytes.truncate(size);
                args.push(String::from_utf8(bytes).unwrap());
            }
            let response = match args[0].as_str() {
                "XADD" => "$3\r\n1-0\r\n",
                "CLIENT" => "+OK\r\n",
                "PING" => "+PONG\r\n",
                _ => ":0\r\n",
            };
            if args[0] == "XADD" {
                records.push(args);
            }
            socket.write_all(response.as_bytes()).unwrap();
        }
        records
    });
    let peer = HttpPeer::new();
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.json");
    std::fs::write(&config, "{}").unwrap();
    let output = cli(&peer, &config)
        .env("AGENT_BUS_SERVER_URL", "")
        .env("AGENT_BUS_REDIS_URL", &url)
        .args([
            "send",
            "--from-agent",
            "test",
            "--to-agent",
            "all",
            "--topic",
            "review",
            "--schema",
            "status",
            "--body",
            "CLEAR",
            "--encoding",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["metadata"]["_schema"], "status");
    assert_eq!(result["body"], "CLEAR");
    let records = worker.join().unwrap();
    assert_eq!(records.len(), 1);
    let fields = records[0][6..].as_chunks::<2>().0;
    let metadata: serde_json::Value =
        serde_json::from_str(&fields.iter().find(|pair| pair[0] == "metadata").unwrap()[1])
            .unwrap();
    assert_eq!(metadata["_schema"], "status");
    assert_eq!(
        fields.iter().find(|pair| pair[0] == "body").unwrap()[1],
        "CLEAR"
    );
    assert!(peer.requests.lock().unwrap().is_empty());
}
