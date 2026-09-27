//! Integration coverage for explicitly provisioned disposable services.
//! Run through scripts/ci isolated-service harness; ordinary tests ignore live cases.

mod support;

use std::process::Command;

fn agent_bus_binary() -> Command {
    support::agent_bus_binary()
}

fn require_service() {
    support::require_http_blocking(&support::blocking_http_client());
}

#[test]
#[ignore = "requires explicit disposable target; use isolated integration harness"]
fn health_returns_ok_when_redis_available() {
    require_service();
    let output = agent_bus_binary()
        .args(["health", "--encoding", "compact"])
        .output()
        .expect("failed to run agent-bus health");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(r#""ok":true"#));
}

#[test]
#[ignore = "requires explicit disposable target; use isolated integration harness"]
fn send_and_read_round_trip() {
    require_service();
    let send = agent_bus_binary()
        .args([
            "send",
            "--from-agent",
            "test-sender",
            "--to-agent",
            "test-receiver",
            "--topic",
            "integration-test",
            "--body",
            "hello-from-integration-test",
            "--encoding",
            "compact",
        ])
        .output()
        .expect("send failed");
    assert!(
        send.status.success(),
        "send failed: {}",
        String::from_utf8_lossy(&send.stderr)
    );

    let read = agent_bus_binary()
        .args([
            "read",
            "--agent",
            "test-receiver",
            "--since-minutes",
            "1",
            "--encoding",
            "compact",
        ])
        .output()
        .expect("read failed");
    assert!(read.status.success());
    let stdout = String::from_utf8_lossy(&read.stdout);
    assert!(stdout.contains("hello-from-integration-test"));
}

#[test]
#[ignore = "requires explicit disposable target; use isolated integration harness"]
fn presence_set_and_list() {
    require_service();
    let set = agent_bus_binary()
        .args([
            "presence",
            "--agent",
            "test-agent-integ",
            "--status",
            "online",
            "--ttl-seconds",
            "10",
            "--encoding",
            "compact",
        ])
        .output()
        .expect("presence set failed");
    assert!(set.status.success());

    let list = agent_bus_binary()
        .args(["presence-list", "--encoding", "compact"])
        .output()
        .expect("presence-list failed");
    assert!(list.status.success());
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("test-agent-integ"));
}

#[test]
fn invalid_settings_rejected() {
    let config = tempfile::NamedTempFile::new().expect("isolated invalid-settings config");
    std::fs::write(config.path(), b"{}").expect("empty test config");
    let output = Command::new(env!("CARGO_BIN_EXE_agent-bus"))
        .env("AGENT_BUS_CONFIG", config.path())
        .env("AGENT_BUS_ALLOW_REMOTE", "false")
        .env("AGENT_BUS_STARTUP_ENABLED", "false")
        .env("AGENT_BUS_REDIS_URL", "redis://remote-host:6380/0")
        .args(["health", "--encoding", "compact"])
        .output()
        .expect("failed to run");
    assert!(
        !output.status.success(),
        "should reject non-localhost Redis URL"
    );
}

#[test]
#[ignore = "requires explicit disposable target; use isolated integration harness"]
fn cli_server_mode_send_and_read_round_trip() {
    require_service();

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let send = agent_bus_binary()
        .env("AGENT_BUS_SERVER_URL", support::base_url())
        .args([
            "send",
            "--from-agent",
            &format!("cli-svr-snd-{ts}"),
            "--to-agent",
            &format!("cli-svr-recv-{ts}"),
            "--topic",
            "server-mode-test",
            "--body",
            "hello-via-server-mode",
            "--encoding",
            "compact",
        ])
        .output()
        .expect("send failed");

    assert!(
        send.status.success(),
        "server-mode send failed: {}",
        String::from_utf8_lossy(&send.stderr)
    );

    let read = agent_bus_binary()
        .env("AGENT_BUS_SERVER_URL", support::base_url())
        .args([
            "read",
            "--agent",
            &format!("cli-svr-recv-{ts}"),
            "--since-minutes",
            "1",
            "--encoding",
            "compact",
        ])
        .output()
        .expect("read failed");

    assert!(read.status.success());
    let stdout = String::from_utf8_lossy(&read.stdout);
    assert!(stdout.contains("hello-via-server-mode"));
}

#[test]
#[ignore = "requires explicit disposable target; use isolated integration harness"]
fn cli_server_mode_batch_send_round_trip() {
    require_service();

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let recipient = format!("cli-svr-batch-recv-{ts}");
    let batch_file = std::env::temp_dir().join(format!("agent-bus-batch-{ts}.ndjson"));
    std::fs::write(
        &batch_file,
        format!(
            "{{\"sender\":\"cli-svr-batch\",\"recipient\":\"{recipient}\",\"topic\":\"server-mode-batch\",\"body\":\"batch-one\"}}\n\
             {{\"sender\":\"cli-svr-batch\",\"recipient\":\"{recipient}\",\"topic\":\"server-mode-batch\",\"body\":\"batch-two\"}}\n"
        ),
    )
    .expect("failed to write batch fixture");

    let batch_path = batch_file.to_string_lossy().into_owned();
    let send = agent_bus_binary()
        .env("AGENT_BUS_SERVER_URL", support::base_url())
        .args(["batch-send", "--file", &batch_path, "--encoding", "compact"])
        .output()
        .expect("batch-send failed");
    let _ = std::fs::remove_file(&batch_file);

    assert!(
        send.status.success(),
        "server-mode batch-send failed: {}",
        String::from_utf8_lossy(&send.stderr)
    );
    let stdout = String::from_utf8_lossy(&send.stdout);
    assert!(stdout.contains(r#""sent":2"#));

    let read = agent_bus_binary()
        .env("AGENT_BUS_SERVER_URL", support::base_url())
        .args([
            "read",
            "--agent",
            &recipient,
            "--since-minutes",
            "1",
            "--encoding",
            "compact",
        ])
        .output()
        .expect("read failed");

    assert!(read.status.success());
    let stdout = String::from_utf8_lossy(&read.stdout);
    assert!(stdout.contains("batch-one"));
    assert!(stdout.contains("batch-two"));
}
