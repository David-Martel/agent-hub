//! Test-only process gate: bounded ownership plus proof of the exact selector.

use std::io::Write as _;
use std::thread;
use std::time::{Duration, Instant};
pub(super) fn complete_child_control(selector: &str) {
    let marker = std::env::var_os("AGENT_BUS_LIFECYCLE_MARKER").unwrap();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker)
        .unwrap();
    file.write_all(selector.as_bytes()).unwrap();
}

pub(super) fn bounded_test_process(selector: &str) -> Result<(), String> {
    let fixture = tempfile::tempdir().unwrap();
    let config = fixture.path().join("config.json");
    let marker = fixture.path().join("completed-selector");
    std::fs::write(&config, "{}").unwrap();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // Win32 CREATE_NO_WINDOW: these owned CUI test children must stay headless.
        command.creation_flags(0x0800_0000)
    };
    command.args([
        "--exact",
        &format!("http::pubsub_lifecycle_tests::{selector}"),
        "--nocapture",
    ]);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("AGENT_BUS_") {
            command.env_remove(key);
        }
    }
    command
        .env("AGENT_BUS_CONFIG", config)
        .env("LOCALAPPDATA", fixture.path())
        .env("AGENT_BUS_LIFECYCLE_CHILD", "1")
        .env("AGENT_BUS_LIFECYCLE_MARKER", &marker);
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        thread::sleep(Duration::from_millis(10));
    };
    if status.is_none() {
        return Err("Owned control exceeded 8s; child killed and reaped".to_owned());
    }
    if !status.unwrap().success() {
        return Err(format!("Owned control exited unsuccessfully: {status:?}"));
    }
    if std::fs::read_to_string(marker).ok().as_deref() != Some(selector) {
        return Err("Exact selector did not record successful completion".to_owned());
    }
    Ok(())
}
