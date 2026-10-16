//! Bounded, connection-owning Redis health probes.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use redis::io::AsyncDNSResolver;

use crate::settings::{Settings, loopback_url_candidates, refuse_live_bus_in_unit_tests};

// Matches the pool's existing fast-fail interval, but covers the complete Redis
// probe rather than just pool acquisition. PostgreSQL and backup scans are separate.
const REDIS_HEALTH_BUDGET: Duration = Duration::from_millis(750);

#[derive(Debug, Default)]
pub(crate) struct RedisHealth {
    pub(crate) ok: bool,
    pub(crate) stream_length: Option<u64>,
    pub(crate) persistence: Option<String>,
}

/// Resolve only addresses allowed by the existing local Redis settings policy.
///
/// Health deliberately avoids OS localhost/hosts-file fallback. Normal data
/// connections are unchanged. Keeping the original `ConnectionInfo` preserves
/// authentication, DB selection, protocol and hostname identity in Redis itself.
#[derive(Debug)]
struct LocalHealthResolver;

impl AsyncDNSResolver for LocalHealthResolver {
    fn resolve<'a, 'b: 'a>(
        &'a self,
        host: &'b str,
        port: u16,
    ) -> redis::RedisFuture<'a, Box<dyn Iterator<Item = SocketAddr> + Send + 'a>> {
        Box::pin(async move {
            let addresses = if host.eq_ignore_ascii_case("localhost") {
                vec![
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
                    SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), port),
                ]
            } else {
                match host.parse::<IpAddr>() {
                    Ok(ip)
                        if ip == IpAddr::V4(Ipv4Addr::LOCALHOST)
                            || ip == IpAddr::V6(Ipv6Addr::LOCALHOST) =>
                    {
                        vec![SocketAddr::new(ip, port)]
                    }
                    _ => {
                        return Err(redis::RedisError::from((
                            redis::ErrorKind::InvalidClientConfig,
                            "Redis health requires localhost, 127.0.0.1 or ::1",
                        )));
                    }
                }
            };
            Ok(Box::new(addresses.into_iter()) as Box<dyn Iterator<Item = SocketAddr> + Send + 'a>)
        })
    }
}

pub(crate) fn probe(settings: &Settings) -> RedisHealth {
    probe_with_budget(settings, REDIS_HEALTH_BUDGET)
}

fn probe_with_budget(settings: &Settings, budget: Duration) -> RedisHealth {
    let Some(deadline) = Instant::now().checked_add(budget) else {
        return RedisHealth::default();
    };
    // The public health API is synchronous and may run inside either Tokio
    // runtime flavor. A scoped worker avoids nested block_on and is always joined.
    // It performs only native async network I/O and our nonblocking resolver.
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("redis-health".to_owned())
            .spawn_scoped(scope, || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return RedisHealth::default();
                };
                let mut progress = RedisHealth::default();
                if Instant::now() < deadline {
                    runtime.block_on(async {
                        let _ = tokio::time::timeout_at(
                            tokio::time::Instant::from_std(deadline),
                            probe_async(settings, &mut progress),
                        )
                        .await;
                    });
                }
                // Timed future drop drops its canonical Redis connection, which
                // aborts its owned driver. Dropping the owned runtime completes
                // task teardown before the scoped worker joins. No blocking DNS,
                // detached task, shutdown_background or timeout-abandoned worker.
                drop(runtime);
                progress
            })
            .map_or_else(
                |_| RedisHealth::default(),
                |worker| match worker.join() {
                    Ok(result) => result,
                    Err(panic) => std::panic::resume_unwind(panic),
                },
            )
    })
}

async fn probe_async(settings: &Settings, progress: &mut RedisHealth) {
    for url in loopback_url_candidates(&settings.redis_url) {
        refuse_live_bus_in_unit_tests(&url);
        // Enforce the same host policy before Client construction as well as
        // inside the resolver, including callers supplying unvalidated Settings.
        if !local_redis_url(&url) {
            continue;
        }
        let Ok(client) = redis::Client::open(url.as_str()) else {
            continue;
        };
        let config = redis::AsyncConnectionConfig::new().set_dns_resolver(LocalHealthResolver);
        let Ok(mut connection) = client
            .get_multiplexed_async_connection_with_config(&config)
            .await
        else {
            continue;
        };
        progress.ok = redis::cmd("PING")
            .query_async::<String>(&mut connection)
            .await
            .is_ok_and(|pong| pong == "PONG");
        progress.stream_length = redis::cmd("XLEN")
            .arg(&settings.stream_key)
            .query_async::<u64>(&mut connection)
            .await
            .ok();
        progress.persistence = redis::cmd("INFO")
            .arg("persistence")
            .query_async::<String>(&mut connection)
            .await
            .ok()
            .map(|info| persistence_summary(&info));
        return;
    }
}

fn local_redis_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| {
        matches!(url.scheme(), "redis" | "rediss")
            && match url.host() {
                Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
                Some(url::Host::Ipv4(ip)) => ip == Ipv4Addr::LOCALHOST,
                Some(url::Host::Ipv6(ip)) => ip == Ipv6Addr::LOCALHOST,
                None => false,
            }
    })
}

fn persistence_summary(info: &str) -> String {
    let value = |key: &str| {
        info.lines()
            .find_map(|line| {
                let (actual, value) = line.split_once(':')?;
                (actual == key).then_some(value.trim())
            })
            .unwrap_or("unknown")
    };
    format!(
        "aof_enabled={};rdb_last_bgsave_status={};aof_last_write_status={}",
        value("aof_enabled"),
        value("rdb_last_bgsave_status"),
        value("aof_last_write_status")
    )
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    const TEST_BUDGET: Duration = Duration::from_millis(500);
    const FIXTURE_LIMIT: Duration = Duration::from_secs(4);

    #[derive(Clone, Copy, Debug)]
    enum PeerMode {
        Healthy,
        SilentSetup,
        Stall(&'static str),
        Partial(&'static str),
        TricklePing,
        SlowStages,
    }

    #[derive(Debug)]
    struct PeerResult {
        client_closed: bool,
        commands: Vec<Vec<String>>,
    }

    #[derive(Debug)]
    struct OwnedPeer {
        port: u16,
        stop: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<PeerResult>>,
    }

    impl OwnedPeer {
        fn new(mode: PeerMode) -> Self {
            // Internal disposable fixture only; configured URLs still use localhost.
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let stop = Arc::new(AtomicBool::new(false));
            let stopped = Arc::clone(&stop);
            let worker = std::thread::spawn(move || {
                let limit = Instant::now() + FIXTURE_LIMIT;
                let mut stream = loop {
                    if stopped.load(Ordering::Relaxed) || Instant::now() >= limit {
                        return PeerResult {
                            client_closed: false,
                            commands: Vec::new(),
                        };
                    }
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2));
                        }
                        Err(e) => panic!("owned accept failed: {e}"),
                    }
                };
                drop(listener);
                stream.set_read_timeout(Some(FIXTURE_LIMIT)).unwrap();
                stream.set_write_timeout(Some(FIXTURE_LIMIT)).unwrap();
                let mut parser = redis::Parser::new();
                let mut commands = Vec::new();
                loop {
                    let request = match parser.parse_value(&mut stream) {
                        Ok(value) => redis::from_redis_value::<Vec<String>>(value).unwrap(),
                        Err(e) => {
                            return PeerResult {
                                client_closed: e.is_connection_dropped(),
                                commands,
                            };
                        }
                    };
                    let name = request[0].clone();
                    commands.push(request);
                    let stalls = matches!(mode, PeerMode::SilentSetup)
                        || matches!(mode, PeerMode::Stall(command) if command == name);
                    if stalls {
                        return PeerResult {
                            client_closed: wait_for_close(&mut stream, &stopped, limit),
                            commands,
                        };
                    }
                    if matches!(mode, PeerMode::Partial(command) if command == name) {
                        let _ = stream.write_all(b"+PO");
                        return PeerResult {
                            client_closed: wait_for_close(&mut stream, &stopped, limit),
                            commands,
                        };
                    }
                    if matches!(mode, PeerMode::TricklePing) && name == "PING" {
                        // Each byte arrives before a hypothetical 500 ms per-read
                        // timeout, but the complete response exceeds the budget.
                        for byte in b"+PONG\r\n" {
                            if stream.write_all(&[*byte]).is_err() {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(120));
                        }
                    } else {
                        if matches!(mode, PeerMode::SlowStages) {
                            std::thread::sleep(Duration::from_millis(40));
                        }
                        let reply = match name.as_str() {
                            "HELLO" => b"%0\r\n".to_vec(),
                            "PING" => b"+PONG\r\n".to_vec(),
                            "XLEN" => b":7\r\n".to_vec(),
                            "INFO" => {
                                let text = "aof_enabled:1\r\nrdb_last_bgsave_status:ok\r\naof_last_write_status:ok\r\n";
                                format!("${}\r\n{text}\r\n", text.len()).into_bytes()
                            }
                            _ => b"+OK\r\n".to_vec(),
                        };
                        if stream.write_all(&reply).is_err() {
                            return PeerResult {
                                client_closed: wait_for_close(&mut stream, &stopped, limit),
                                commands,
                            };
                        }
                    }
                }
            });
            Self {
                port,
                stop,
                worker: Some(worker),
            }
        }

        fn settings(&self) -> Settings {
            let mut settings = crate::test_support::offline_settings();
            settings.redis_url = format!("redis://localhost:{}/0", self.port);
            settings.database_url = None;
            settings
        }

        fn finish(mut self) -> PeerResult {
            self.worker
                .take()
                .unwrap()
                .join()
                .expect("owned peer panicked")
        }
    }

    impl Drop for OwnedPeer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn wait_for_close(stream: &mut TcpStream, stop: &AtomicBool, limit: Instant) -> bool {
        stream
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let mut bytes = [0; 1024];
        while !stop.load(Ordering::Relaxed) && Instant::now() < limit {
            match stream.read(&mut bytes) {
                Ok(0) => return true,
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                    ) =>
                {
                    return true;
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => panic!("owned peer read failed: {e}"),
            }
        }
        false
    }

    fn assert_stall(mode: PeerMode, expect_ping: bool, expect_length: Option<u64>) {
        let peer = OwnedPeer::new(mode);
        let start = Instant::now();
        let result = probe_with_budget(&peer.settings(), TEST_BUDGET);
        let elapsed = start.elapsed();
        let settled = peer.finish();
        assert!(
            elapsed < Duration::from_secs(2),
            "aggregate deadline missed: {elapsed:?}"
        );
        assert!(settled.client_closed, "probe returned without peer closure");
        assert_eq!(result.ok, expect_ping);
        assert_eq!(result.stream_length, expect_length);
        assert!(result.persistence.is_none());
    }

    #[test]
    fn accepted_silent_setup_is_bounded_and_socket_settles() {
        assert_stall(PeerMode::SilentSetup, false, None);
    }

    #[test]
    fn silent_and_partial_ping_obey_one_budget() {
        assert_stall(PeerMode::Stall("PING"), false, None);
        assert_stall(PeerMode::Partial("PING"), false, None);
    }

    #[test]
    fn trickle_ping_does_not_restart_budget() {
        assert_stall(PeerMode::TricklePing, false, None);
    }

    #[test]
    fn later_stalls_preserve_completed_ping_and_counts() {
        assert_stall(PeerMode::Stall("XLEN"), true, None);
        assert_stall(PeerMode::Stall("INFO"), true, Some(7));
    }

    #[test]
    fn setup_stages_share_command_budget() {
        let peer = OwnedPeer::new(PeerMode::SlowStages);
        let result = probe_with_budget(&peer.settings(), Duration::from_millis(150));
        assert!(peer.finish().client_closed);
        // Four sequential replies need at least 160 ms. Scheduler delays may
        // reduce progress further, so do not require a timing-sensitive PONG.
        assert!(result.stream_length.is_none());
        assert!(result.persistence.is_none());
    }

    #[test]
    fn healthy_probe_preserves_auth_select_resp3_and_persistence() {
        for resp3 in [false, true] {
            let peer = OwnedPeer::new(PeerMode::Healthy);
            let mut settings = peer.settings();
            settings.redis_url = format!(
                "redis://fixture:fixture-only@localhost:{}/2?protocol={}",
                peer.port,
                if resp3 { 3 } else { 2 }
            );
            let result = probe_with_budget(&settings, Duration::from_secs(2));
            let seen = peer.finish();
            assert!(seen.client_closed);
            assert!(
                seen.commands
                    .iter()
                    .any(|c| c[0] == if resp3 { "HELLO" } else { "AUTH" })
            );
            assert!(
                seen.commands
                    .iter()
                    .any(|c| c.len() == 2 && c[0] == "SELECT" && c[1] == "2")
            );
            assert!(result.ok);
            assert_eq!(result.stream_length, Some(7));
            assert_eq!(
                result.persistence.as_deref(),
                Some("aof_enabled=1;rdb_last_bgsave_status=ok;aof_last_write_status=ok")
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn synchronous_probe_settles_inside_current_thread_runtime() {
        assert_stall(PeerMode::Stall("PING"), false, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn synchronous_probe_settles_inside_multi_thread_runtime() {
        assert_stall(PeerMode::Stall("PING"), false, None);
    }

    #[tokio::test]
    async fn resolver_is_pure_local_policy_without_os_dns() {
        let resolver = LocalHealthResolver;
        for host in ["localhost", "LOCALHOST", "127.0.0.1", "::1"] {
            let addresses = resolver
                .resolve(host, 12345)
                .await
                .unwrap()
                .collect::<Vec<_>>();
            let expected_length = if host.eq_ignore_ascii_case("localhost") {
                2
            } else {
                1
            };
            assert_eq!(addresses.len(), expected_length);
            assert!(
                addresses
                    .iter()
                    .all(|a| a.port() == 12345 && a.ip().is_loopback())
            );
        }
        for host in ["example.invalid", "127.0.0.2", "0.0.0.0", "::", ""] {
            assert!(resolver.resolve(host, 12345).await.is_err());
        }
    }

    #[test]
    fn auth_hello_and_select_setup_are_inside_deadline() {
        for (command, protocol) in [("AUTH", 2), ("HELLO", 3), ("SELECT", 2)] {
            let peer = OwnedPeer::new(PeerMode::Partial(command));
            let mut settings = peer.settings();
            settings.redis_url = format!(
                "redis://fixture:fixture-only@localhost:{}/2?protocol={protocol}",
                peer.port
            );
            let result = probe_with_budget(&settings, TEST_BUDGET);
            let settled = peer.finish();
            assert!(settled.client_closed);
            assert!(settled.commands.iter().any(|c| c[0] == command));
            assert!(!result.ok);
        }
    }

    #[test]
    fn invalid_and_nonlocal_settings_degrade_without_os_lookup() {
        let mut settings = crate::test_support::offline_settings();
        settings.database_url = None;
        for url in [
            "not-a-redis-url",
            "redis://example.invalid:12345/0",
            "redis://192.0.2.1:12345/0",
            "redis://[2001:db8::1]:12345/0",
        ] {
            settings.redis_url = url.to_owned();
            let result = probe_with_budget(&settings, TEST_BUDGET);
            assert!(!result.ok);
            assert!(result.stream_length.is_none());
            assert!(result.persistence.is_none());
        }
    }

    #[test]
    fn exhausted_budget_never_enters_first_candidate() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut settings = crate::test_support::offline_settings();
        settings.redis_url = format!(
            "redis://localhost:{}/0",
            listener.local_addr().unwrap().port()
        );
        let result = probe_with_budget(&settings, Duration::ZERO);
        assert!(!result.ok);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[derive(Debug)]
    struct PendingTransport {
        dropped: Arc<AtomicBool>,
        write_polled: Arc<AtomicBool>,
    }

    impl Drop for PendingTransport {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Relaxed);
        }
    }

    impl tokio::io::AsyncRead for PendingTransport {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Pending
        }
    }

    impl tokio::io::AsyncWrite for PendingTransport {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.write_polled.store(true, Ordering::Relaxed);
            std::task::Poll::Pending
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Pending
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn stalled_setup_write_cancellation_drops_owned_transport() {
        let dropped = Arc::new(AtomicBool::new(false));
        let write_polled = Arc::new(AtomicBool::new(false));
        let stream = PendingTransport {
            dropped: Arc::clone(&dropped),
            write_polled: Arc::clone(&write_polled),
        };
        let client = redis::Client::open("redis://localhost:12345/0").unwrap();
        let setup = redis::aio::MultiplexedConnection::new_with_config(
            client.get_connection_info().redis_settings(),
            stream,
            redis::AsyncConnectionConfig::new(),
        );
        assert!(tokio::time::timeout(TEST_BUDGET, setup).await.is_err());
        assert!(write_polled.load(Ordering::Relaxed));
        assert!(
            dropped.load(Ordering::Relaxed),
            "deadline left the underlying transport alive"
        );
    }

    #[test]
    fn core_dispatch_stall_keeps_degraded_health_payload() {
        let peer = OwnedPeer::new(PeerMode::SilentSetup);
        let settings = peer.settings();
        let started = Instant::now();
        let value = crate::mcp_dispatch::McpToolDispatch::new(&settings)
            .dispatch_tool("bus_health", &serde_json::Map::new())
            .unwrap();
        let elapsed = started.elapsed();
        assert!(peer.finish().client_closed);
        assert!(elapsed < Duration::from_secs(2));
        assert_eq!(value["ok"], false);
        assert_eq!(value["storage_ready"], true);
        for key in ["database_url", "database_ok", "database_error"] {
            assert_eq!(value.get(key), Some(&serde_json::Value::Null));
        }
        for key in [
            "stream_length",
            "redis_persistence",
            "pg_message_count",
            "pg_presence_count",
        ] {
            assert!(value.get(key).is_none());
        }
        assert!(
            !value
                .as_object()
                .unwrap()
                .keys()
                .any(|key| key.starts_with("cloud_"))
        );
        assert_eq!(value["codec"], "serde_json");
        assert_eq!(value["runtime"], "rust-native");
    }
}
