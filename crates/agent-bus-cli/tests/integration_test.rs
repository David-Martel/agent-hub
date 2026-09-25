//! Integration tests for the `agent-bus` CLI against real Redis/PostgreSQL.
//!
//! Every backend test is `#[ignore]`d and runs only with `-- --ignored`, against
//! the disposable backends named by `AGENT_BUS_TEST_*` (see
//! `crates/agent-bus-core/tests/support/backend_env.rs`). An unset variable or an
//! unreachable backend FAILS the test; nothing here skips or defaults to the
//! live bus.

use std::process::Command;

#[path = "../../agent-bus-core/tests/support/backend_env.rs"]
mod backend_env;

use backend_env::{DATABASE_URL_VAR, REDIS_URL_VAR, SERVER_URL_VAR, backend_url};

fn agent_bus_binary() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-bus"));
    cmd.env_remove("AGENT_BUS_SERVER_URL");
    // Keep the developer's ~/.config/agent-bus/config.json (which may name a
    // server_url or token for the real hub) out of the child's settings.
    cmd.env("AGENT_BUS_CONFIG", isolated_config_path());
    cmd.env("AGENT_BUS_REDIS_URL", backend_url(REDIS_URL_VAR));
    cmd.env("AGENT_BUS_DATABASE_URL", backend_url(DATABASE_URL_VAR));
    cmd.env("AGENT_BUS_STREAM_KEY", "agent_bus:test:messages");
    cmd.env("AGENT_BUS_CHANNEL", "agent_bus:test:events");
    cmd.env("AGENT_BUS_PRESENCE_PREFIX", "agent_bus:test:presence:");
    cmd
}

fn isolated_config_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("agent-bus-test-config-{}.json", std::process::id()))
}

/// Fail (not skip) when the configured backend cannot serve `health`.
fn require_backend() {
    let output = agent_bus_binary()
        .args(["health", "--encoding", "compact"])
        .output()
        .expect("failed to run agent-bus health");
    assert!(
        output.status.success(),
        "backend unreachable via {REDIS_URL_VAR}: agent-bus health exited {} -- stdout: {} stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL + AGENT_BUS_TEST_DATABASE_URL (see tests/support/backend_env.rs)"]
#[test]
fn health_returns_ok_when_redis_available() {
    require_backend();
    let output = agent_bus_binary()
        .args(["health", "--encoding", "compact"])
        .output()
        .expect("failed to run agent-bus health");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(r#""ok":true"#));
}

#[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL + AGENT_BUS_TEST_DATABASE_URL (see tests/support/backend_env.rs)"]
#[test]
fn send_and_read_round_trip() {
    require_backend();
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

#[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL + AGENT_BUS_TEST_DATABASE_URL (see tests/support/backend_env.rs)"]
#[test]
fn presence_set_and_list() {
    require_backend();
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
    let output = Command::new(env!("CARGO_BIN_EXE_agent-bus"))
        .env("AGENT_BUS_CONFIG", isolated_config_path())
        .env("AGENT_BUS_REDIS_URL", "redis://remote-host:16380/0")
        .args(["health", "--encoding", "compact"])
        .output()
        .expect("failed to run");
    assert!(
        !output.status.success(),
        "should reject non-localhost Redis URL"
    );
}

#[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL + AGENT_BUS_TEST_DATABASE_URL (see tests/support/backend_env.rs)"]
#[test]
fn cli_server_mode_send_and_read_round_trip() {
    require_backend();

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let send = agent_bus_binary()
        .env("AGENT_BUS_SERVER_URL", backend_url(SERVER_URL_VAR))
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
        .env("AGENT_BUS_SERVER_URL", backend_url(SERVER_URL_VAR))
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

#[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL + AGENT_BUS_TEST_DATABASE_URL (see tests/support/backend_env.rs)"]
#[test]
fn cli_server_mode_batch_send_round_trip() {
    require_backend();

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
        .env("AGENT_BUS_SERVER_URL", backend_url(SERVER_URL_VAR))
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
        .env("AGENT_BUS_SERVER_URL", backend_url(SERVER_URL_VAR))
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
