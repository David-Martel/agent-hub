//! Integration tests requiring a running Redis instance.
//! Skipped gracefully if Redis is not available.

use std::process::Command;

fn agent_bus_binary() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agent-bus"));
    cmd.env("AGENT_BUS_STREAM_KEY", "agent_bus:test:messages");
    cmd.env("AGENT_BUS_CHANNEL", "agent_bus:test:events");
    cmd.env("AGENT_BUS_PRESENCE_PREFIX", "agent_bus:test:presence:");
    cmd
}

fn redis_available() -> bool {
    agent_bus_binary()
        .args(["health", "--encoding", "compact"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn health_returns_ok_when_redis_available() {
    if !redis_available() {
        eprintln!("SKIP: Redis not available");
        return;
    }
    let output = agent_bus_binary()
        .args(["health", "--encoding", "compact"])
        .output()
        .expect("failed to run agent-bus health");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(r#""ok":true"#));
}

#[test]
fn send_and_read_round_trip() {
    if !redis_available() {
        eprintln!("SKIP: Redis not available");
        return;
    }
    let send = agent_bus_binary()
        .args([
            "send", "--from-agent", "test-sender", "--to-agent", "test-receiver",
            "--topic", "integration-test", "--body", "hello-from-integration-test",
            "--encoding", "compact",
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
            "read", "--agent", "test-receiver", "--since-minutes", "1", "--encoding", "compact",
        ])
        .output()
        .expect("read failed");
    assert!(read.status.success());
    let stdout = String::from_utf8_lossy(&read.stdout);
    assert!(stdout.contains("hello-from-integration-test"));
}

#[test]
fn presence_set_and_list() {
    if !redis_available() {
        eprintln!("SKIP: Redis not available");
        return;
    }
    let set = agent_bus_binary()
        .args([
            "presence", "--agent", "test-agent-integ", "--status", "online",
            "--ttl-seconds", "10", "--encoding", "compact",
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
        .env("AGENT_BUS_REDIS_URL", "redis://remote-host:6380/0")
        .args(["health", "--encoding", "compact"])
        .output()
        .expect("failed to run");
    assert!(
        !output.status.success(),
        "should reject non-localhost Redis URL"
    );
}
