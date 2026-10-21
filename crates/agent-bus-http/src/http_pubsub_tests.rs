//! Private RESP fixtures exercise actual worker/server lifecycle, never fleet stores.

use super::*;
use std::io::Write as _;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Instant;

#[path = "http_lifecycle_process.rs"]
mod process;
use process::{bounded_test_process, complete_child_control};

struct RedisFixture {
    addr: std::net::SocketAddr,
    stopped: Arc<AtomicBool>,
    subscriptions: Arc<AtomicUsize>,
    connections: Arc<AtomicUsize>,
    payload: Arc<Mutex<Option<String>>>,
    disconnects: Arc<AtomicUsize>,
    blackhole: Arc<AtomicBool>,
    reject_connections: Arc<AtomicBool>,
    rejected: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}

impl RedisFixture {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let subscriptions = Arc::new(AtomicUsize::new(0));
        let connections = Arc::new(AtomicUsize::new(0));
        let payload = Arc::new(Mutex::new(None));
        let disconnects = Arc::new(AtomicUsize::new(0));
        let blackhole = Arc::new(AtomicBool::new(false));
        let reject_connections = Arc::new(AtomicBool::new(false));
        let rejected = Arc::new(AtomicUsize::new(0));
        let stop = Arc::clone(&stopped);
        let subscribed = Arc::clone(&subscriptions);
        let connected = Arc::clone(&connections);
        let data = Arc::clone(&payload);
        let drops = Arc::clone(&disconnects);
        let hangs = Arc::clone(&blackhole);
        let rejects = Arc::clone(&reject_connections);
        let rejected_count = Arc::clone(&rejected);
        let thread = thread::spawn(move || {
            let mut clients = Vec::new();
            while !stop.load(Ordering::Acquire) {
                if let Ok((stream, _)) = listener.accept() {
                    connected.fetch_add(1, Ordering::Release);
                    if rejects.load(Ordering::Acquire) {
                        drop(stream);
                        rejected_count.fetch_add(1, Ordering::Release);
                        continue;
                    }
                    let stop = Arc::clone(&stop);
                    let subscribed = Arc::clone(&subscribed);
                    let data = Arc::clone(&data);
                    let drops = Arc::clone(&drops);
                    let hangs = Arc::clone(&hangs);
                    clients.push(thread::spawn(move || {
                        if hangs.load(Ordering::Acquire) {
                            while !stop.load(Ordering::Acquire) {
                                thread::sleep(Duration::from_millis(5));
                            }
                        } else {
                            serve_redis(stream, &stop, &subscribed, &data, &drops);
                        }
                    }));
                } else {
                    thread::sleep(Duration::from_millis(5));
                }
            }
            for client in clients {
                client.join().unwrap();
            }
        });
        Self {
            addr,
            stopped,
            subscriptions,
            connections,
            payload,
            disconnects,
            blackhole,
            reject_connections,
            rejected,
            thread: Some(thread),
        }
    }

    fn url(&self) -> String {
        format!("redis://{}/0", self.addr)
    }
}

impl Drop for RedisFixture {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn serve_redis(
    mut stream: TcpStream,
    stop: &AtomicBool,
    subscribed: &AtomicUsize,
    payload: &Mutex<Option<String>>,
    disconnects: &AtomicUsize,
) {
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut parser = redis::Parser::new();
    while !stop.load(Ordering::Acquire) {
        let value = match parser.parse_value(&mut stream) {
            Ok(value) => value,
            Err(error) if error.is_timeout() => continue,
            Err(_) => break,
        };
        let args: Vec<String> = redis::from_redis_value(value).unwrap();
        let reply = match args[0].to_uppercase().as_str() {
            "PING" => "+PONG\r\n".to_owned(),
            "SUBSCRIBE" => {
                subscribed.fetch_add(1, Ordering::Release);
                format!(
                    "*3\r\n$9\r\nsubscribe\r\n${}\r\n{}\r\n:1\r\n",
                    args[1].len(),
                    args[1]
                )
            }
            "UNSUBSCRIBE" => "*3\r\n$11\r\nunsubscribe\r\n$0\r\n\r\n:0\r\n".to_owned(),
            "PUNSUBSCRIBE" => "*3\r\n$12\r\npunsubscribe\r\n$0\r\n\r\n:0\r\n".to_owned(),
            _ => "+OK\r\n".to_owned(),
        };
        if stream.write_all(reply.as_bytes()).is_err() {
            break;
        }
        if args[0].eq_ignore_ascii_case("SUBSCRIBE") {
            if disconnects
                .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                .is_ok()
            {
                break;
            }
            if let Some(data) = payload.lock().unwrap().as_ref() {
                let event = format!(
                    "*3\r\n$7\r\nmessage\r\n${}\r\n{}\r\n${}\r\n{}\r\n",
                    args[1].len(),
                    args[1],
                    data.len(),
                    data
                );
                if stream.write_all(event.as_bytes()).is_err() {
                    break;
                }
            }
        }
    }
}

fn isolated_settings(redis_url: String) -> Settings {
    // Literal settings avoid reading host config/auth/cloud state even in the child.
    Settings {
        redis_url,
        database_url: None,
        stream_key: "fixture:messages".to_owned(),
        channel_key: "fixture:events".to_owned(),
        presence_prefix: "fixture:presence:".to_owned(),
        message_table: "fixture.messages".to_owned(),
        presence_event_table: "fixture.presence".to_owned(),
        stream_maxlen: 100,
        service_agent_id: "fixture".to_owned(),
        service_name: "Fixture".to_owned(),
        startup_enabled: false,
        startup_recipient: "all".to_owned(),
        startup_topic: "status".to_owned(),
        startup_body: "fixture".to_owned(),
        server_host: "127.0.0.1".to_owned(),
        session_id: None,
        server_url: None,
        server_urls: Vec::new(),
        hub_candidates: Vec::new(),
        probe_connect_timeout_ms: 750,
        hub_cache_ttl_seconds: 0,
        hub_config_error: None,
        network_locations: Vec::new(),
        network_location_error: None,
        hub_identity: None,
        machine_safe: false,
        auth_token: Some("synthetic-private-fixture".to_owned()),
        allow_remote: false,
        cloud_url: None,
        cloud_token_file: None,
        cloud_sync_state_file: None,
    }
}

// Protects: an accepted HTTP stop must let Tokio and the owned process exit.
// Detects: an idle Redis bridge retained by Runtime::drop, not just a closed listener.
// Needs: only a private loopback RESP fixture and a bounded, reaped test child.
// Breadcrumb: crates/agent-bus-http/src/http.rs (start_http_server / spawn_agent_notification_bridge.)
#[test]
fn idle_pubsub_runtime_exits_after_http_stop() {
    shutdown_process_control("idle", "idle_pubsub_runtime_exits_after_http_stop");
}

// Protects: quiet /events bodies cannot hold Axum graceful shutdown open.
// Detects: cancelling only the bridge while an idle per-client worker survives.
// Needs: the same private HTTP/RESP child, retaining a real authenticated SSE response.
// Breadcrumb: crates/agent-bus-http/src/http.rs (http_sse_handler / start_http_server.)
#[test]
fn quiet_sse_runtime_exits_after_http_stop() {
    shutdown_process_control("sse", "quiet_sse_runtime_exits_after_http_stop");
}

// Protects: Redis loss during service stop must not leave an infinite reconnect loop.
// Detects: cancellation omitted from the retry path after a real subscribed disconnect.
// Needs: private RESP fixture drops the subscribed socket, then proves a rejected reconnect before HTTP stop.
// Breadcrumb: crates/agent-bus-http/src/http.rs (spawn_agent_notification_bridge.)
#[test]
fn reconnecting_runtime_exits_after_http_stop() {
    shutdown_process_control("reconnect", "reconnecting_runtime_exits_after_http_stop");
}

// Protects: cancelling the HTTP server future still releases its blocking work.
// Detects: ownership cleanup that only runs after a normal graceful-shutdown return.
// Needs: the private actual server child, dropped after a proven Redis subscription.
// Breadcrumb: crates/agent-bus-http/src/http.rs (PubSubWorkers::drop.)
#[test]
fn dropping_server_future_releases_pubsub_runtime() {
    shutdown_process_control("drop", "dropping_server_future_releases_pubsub_runtime");
}

fn shutdown_process_control(mode: &str, selector: &str) {
    if std::env::var_os("AGENT_BUS_LIFECYCLE_CHILD").is_some() {
        http_stop_fixture_child(mode);
        complete_child_control(selector);
        return;
    }
    bounded_test_process(selector).unwrap();
}

// Protects/detects/needs: child half of the bounded process-exit selector above.
// Breadcrumb: crates/agent-bus-http/src/http_pubsub_tests.rs (idle_pubsub_runtime_exits_after_http_stop.)
fn http_stop_fixture_child(mode: &str) {
    assert_eq!(
        std::env::var("AGENT_BUS_LIFECYCLE_CHILD").as_deref(),
        Ok("1")
    );
    let redis = RedisFixture::new();
    if mode == "reconnect" {
        redis.disconnects.store(1, Ordering::Release);
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let _occupied_listener = if mode.starts_with("occupied") {
        Some(listener)
    } else {
        drop(listener);
        None
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        if mode.starts_with("occupied") {
            if mode == "occupied-fault" {
                let url = redis.url();
                // Deliberate regression control confined to this watchdog child:
                // an unowned idle subscriber must prevent runtime exit.
                tokio::task::spawn_blocking(move || {
                    let mut conn = redis::Client::open(url).unwrap().get_connection().unwrap();
                    let mut pubsub = conn.as_pubsub();
                    pubsub.subscribe("fixture:events").unwrap();
                    while pubsub.get_message().is_ok() {}
                });
                wait_for(|| redis.subscriptions.load(Ordering::Acquire) > 0).await;
            }
            let error = start_http_server(isolated_settings(redis.url()), port).await.unwrap_err();
            assert!(error.to_string().contains("failed to bind HTTP server"));
            return;
        }
        let stop_request =
            async {
                tokio::time::timeout(Duration::from_secs(3), async {
                    while redis.subscriptions.load(Ordering::Acquire) == 0 {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
                if mode == "drop" { return; }
                let mut sse = if mode == "sse" {
                    let response = reqwest::Client::new()
                        .get(format!("http://127.0.0.1:{port}/events"))
                        .bearer_auth("synthetic-private-fixture")
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(response.status(), StatusCode::OK);
                    Some(response)
                } else {
                    None
                };
                if mode == "sse" {
                    tokio::time::timeout(Duration::from_secs(3), async {
                        while redis.subscriptions.load(Ordering::Acquire) < 2 {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    })
                    .await
                    .unwrap();
                }
                if mode == "reconnect" {
                    redis.reject_connections.store(true, Ordering::Release);
                    // A real subscribed socket has disconnected; require a
                    // subsequent refused handshake, not just a stopped fixture.
                    wait_for(|| redis.rejected.load(Ordering::Acquire) > 0).await;
                }
                let response = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/admin/service/control"))
            .bearer_auth("synthetic-private-fixture")
            .json(&serde_json::json!({"action":"stop", "flush":false, "requested_by":"fixture"}))
            .send().await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(
                    response.json::<serde_json::Value>().await.unwrap()["ok"],
                    true
                );
                if let Some(response) = sse.as_mut() {
                    assert!(
                        response.chunk().await.unwrap().is_none(),
                        "stopped SSE must close without another Redis message"
                    );
                }
            };
        if mode == "drop" {
            tokio::select! {
                result = start_http_server(isolated_settings(redis.url()), port) => panic!("server stopped before cancellation: {result:?}"),
                () = stop_request => {},
            }
        } else {
            let (result, ()) = tokio::join!(
                start_http_server(isolated_settings(redis.url()), port), stop_request
            );
            result.unwrap();
        }
    });
    drop(runtime); // This waits forever in the unfixed implementation; parent reaps it.
}

fn worker_state(redis: &RedisFixture) -> AppState {
    let settings = isolated_settings(redis.url());
    AppState {
        redis: RedisPool::new_lazy(&settings).unwrap(),
        settings: Arc::new(settings),
        agent_connections: Arc::new(RwLock::new(HashMap::new())),
        sse_subscriber_count: Arc::new(SseSubscriberCount::default()),
        control_status: Arc::new(RwLock::new(ServerControlStatus::new("fixture", "Fixture"))),
        shutdown_signal: Arc::new(Notify::new()),
        pubsub_workers: Arc::new(PubSubWorkers::default()),
        cloud_sync: None,
    }
}

async fn wait_for(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(4), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

// Protects: reconnect retains actual direct/broadcast notification forwarding.
// Detects: a lifecycle fix that stops delivery or never reconnects after socket loss.
// Needs: one forced private Redis disconnect, real RESP message and two agent receivers.
// Breadcrumb: crates/agent-bus-http/src/http.rs (spawn_agent_notification_bridge / push_notifications_to_agent_connections.)
#[tokio::test]
async fn bridge_reconnects_and_forwards_actual_message() {
    let redis = RedisFixture::new();
    redis.disconnects.store(1, Ordering::Release);
    *redis.payload.lock().unwrap() = Some(serde_json::json!({"event":"message", "message": {
        "id":"fixture-message", "timestamp_utc":"2026-10-21T00:00:00Z", "protocol_version":"1.0",
        "from":"fixture-sender", "to":"fixture-agent", "topic":"status", "body":"owned positive control"
    }}).to_string());
    let state = worker_state(&redis);
    let (direct_tx, mut direct_rx) = tokio::sync::mpsc::channel(4);
    let (all_tx, mut all_rx) = tokio::sync::mpsc::channel(4);
    state
        .agent_connections
        .write()
        .await
        .insert("fixture-agent".to_owned(), vec![direct_tx]);
    state
        .agent_connections
        .write()
        .await
        .insert("all".to_owned(), vec![all_tx]);
    spawn_agent_notification_bridge(&state);
    let direct = tokio::time::timeout(Duration::from_secs(4), direct_rx.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let broadcast = tokio::time::timeout(Duration::from_secs(4), all_rx.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(format!("{direct:?}").contains("fixture-message"));
    assert!(format!("{broadcast:?}").contains("fixture-message"));
    assert!(redis.subscriptions.load(Ordering::Acquire) >= 2);
    tokio::time::timeout(Duration::from_secs(4), state.pubsub_workers.shutdown())
        .await
        .unwrap()
        .unwrap();
}

// Protects: a quiet disconnected SSE client releases its worker and subscriber count.
// Detects: idle get_message ignoring receiver closure, or accumulating completed handles.
// Needs: repeated private /events handlers; no global stop until all disconnect controls pass.
// Breadcrumb: crates/agent-bus-http/src/http.rs (http_sse_handler / PubSubWorkers::spawn.)
#[tokio::test]
async fn quiet_sse_disconnect_releases_workers_and_reaps_finished_entries() {
    let redis = RedisFixture::new();
    let state = worker_state(&redis);
    for count in 1..=3 {
        let sse = http_sse_handler(
            State(state.clone()),
            Query(SseQuery {
                agent: None,
                broadcast: true,
            }),
        )
        .await;
        wait_for(|| redis.subscriptions.load(Ordering::Acquire) >= count).await;
        assert!(state.sse_subscriber_count.any());
        drop(sse);
        wait_for(|| !state.sse_subscriber_count.any()).await;
        assert!(state.pubsub_workers.tasks.lock().unwrap().len() <= 1);
    }
    state.pubsub_workers.shutdown().await.unwrap();
    assert!(state.pubsub_workers.tasks.lock().unwrap().is_empty());
}

// Protects: stop while Redis accepts TCP but never completes its handshake.
// Detects: a bounded read loop paired with an unbounded initial connection/setup.
// Needs: a private blackhole RESP fixture and a real registered bridge.
// Breadcrumb: crates/agent-bus-http/src/http.rs (open_pubsub_connection / PubSubWorkers::shutdown.)
#[tokio::test]
async fn stalled_redis_handshake_is_bounded_and_joined() {
    let redis = RedisFixture::new();
    redis.blackhole.store(true, Ordering::Release);
    let state = worker_state(&redis);
    spawn_agent_notification_bridge(&state);
    wait_for(|| redis.connections.load(Ordering::Acquire) > 0).await;
    assert_eq!(redis.subscriptions.load(Ordering::Acquire), 0);
    tokio::time::timeout(Duration::from_secs(3), state.pubsub_workers.shutdown())
        .await
        .unwrap()
        .unwrap();
}

// Protects: stop wakes retry immediately rather than waiting for the reconnect delay.
// Detects: replacing infinite retry with a non-interruptible fixed sleep.
// Needs: only the shared cooperative control and one owned joined thread.
// Breadcrumb: crates/agent-bus-http/src/http.rs (PubSubControl::wait_for_retry.)
#[test]
fn retry_wait_is_interrupted_by_stop() {
    if std::env::var_os("AGENT_BUS_LIFECYCLE_CHILD").is_none() {
        bounded_test_process("retry_wait_is_interrupted_by_stop").unwrap();
        return;
    }
    let control = Arc::new(PubSubControl::default());
    let worker_control = Arc::clone(&control);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        worker_control.wait_for_retry_ready(|| ready_tx.send(()).unwrap());
        done_tx.send(()).unwrap();
    });
    ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let start = Instant::now();
    control.stop();
    done_rx.recv_timeout(Duration::from_millis(500)).unwrap();
    worker.join().unwrap();
    assert!(start.elapsed() < Duration::from_millis(500));
    complete_child_control("retry_wait_is_interrupted_by_stop");
}

// Protects: cancellation releases a Pub/Sub worker blocked by a full SSE channel.
// Detects: blocking_send or send().await without cancellation on a proven-full queue.
// Needs: the exact production sender helper, a capacity-one full queue and persistent stop.
// Breadcrumb: crates/agent-bus-http/src/http.rs (send_pubsub_event.)
#[tokio::test]
async fn full_sse_queue_is_cancelled_without_dropping_receiver() {
    let control = Arc::new(PubSubControl::default());
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    tx.try_send(Ok(Event::default().data("retained-first-event")))
        .unwrap();
    assert_eq!(tx.capacity(), 0);
    let mut cancelled = control.signal.subscribe();
    let send = send_pubsub_event(
        &tx,
        &mut cancelled,
        Event::default().data("blocked-second-event"),
    );
    tokio::pin!(send);
    tokio::select! {
        result = &mut send => panic!("full queue unexpectedly completed before stop: {result}"),
        () = tokio::task::yield_now() => {},
    }
    control.stop();
    assert!(
        !tokio::time::timeout(Duration::from_millis(500), &mut send)
            .await
            .unwrap()
    );
    assert!(format!("{:?}", rx.recv().await.unwrap().unwrap()).contains("retained-first-event"));
    assert!(
        rx.try_recv().is_err(),
        "cancelled event must not replace queued data"
    );
}

// Protects: /events still emits actual Redis payloads with the configured recipient filter.
// Detects: cancellation plumbing that closes or suppresses an otherwise healthy SSE stream.
// Needs: private real RESP SUBSCRIBE/message and the handler's actual response body stream.
// Breadcrumb: crates/agent-bus-http/src/http.rs (http_sse_handler.)
#[tokio::test]
async fn sse_forwards_actual_filtered_redis_payload() {
    use tokio_stream::StreamExt as _;
    let redis = RedisFixture::new();
    *redis.payload.lock().unwrap() = Some(
        r#"{"event":"message","message":{"to":"fixture-agent","body":"sse-positive"}}"#.to_owned(),
    );
    let state = worker_state(&redis);
    let response = http_sse_handler(
        State(state.clone()),
        Query(SseQuery {
            agent: Some("fixture-agent".to_owned()),
            broadcast: false,
        }),
    )
    .await
    .into_response();
    let mut body = response.into_body().into_data_stream();
    let bytes = tokio::time::timeout(Duration::from_secs(3), body.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        String::from_utf8(bytes.to_vec())
            .unwrap()
            .contains("sse-positive")
    );
    drop(body);
    wait_for(|| !state.sse_subscriber_count.any()).await;
    state.pubsub_workers.shutdown().await.unwrap();
}

// Protects: failed listener startup returns its actual bind error and releases runtime ownership.
// Detects: a failed-bind path retaining a blocking worker, rather than claiming unobservable spawn order.
// Needs: an occupied private loopback port, healthy RESP fixture and reaped process watchdog.
// Breadcrumb: crates/agent-bus-http/src/http.rs (start_http_server.)
#[test]
fn occupied_listener_returns_error_and_releases_runtime() {
    shutdown_process_control(
        "occupied",
        "occupied_listener_returns_error_and_releases_runtime",
    );
}

// Protects: the failed-bind process gate can actually reject a retained worker.
// Detects: replacing bounded process/runtime observation with an immediate counter assertion.
// Needs: one deliberately unowned idle subscriber in a private child killed/reaped by the watchdog.
// Breadcrumb: crates/agent-bus-http/src/http_lifecycle_process.rs (bounded_test_process).
#[test]
fn failed_bind_watchdog_rejects_retained_subscriber() {
    if std::env::var_os("AGENT_BUS_LIFECYCLE_CHILD").is_some() {
        http_stop_fixture_child("occupied-fault");
        panic!("deliberately retained subscriber unexpectedly released runtime");
    }
    let error =
        bounded_test_process("failed_bind_watchdog_rejects_retained_subscriber").unwrap_err();
    assert_eq!(error, "Owned control exceeded 8s; child killed and reaped");
}

// Protects: an empty libtest --exact match cannot falsely certify lifecycle behavior.
// Detects: accepting exit zero without proof that the exact intended selector completed.
// Needs: an actual intentionally absent child selector and its missing private completion marker.
// Breadcrumb: crates/agent-bus-http/src/http_lifecycle_process.rs (bounded_test_process / complete_child_control).
#[test]
fn absent_child_selector_is_rejected_despite_zero_exit() {
    let error = bounded_test_process("deliberately_absent_lifecycle_selector").unwrap_err();
    assert_eq!(error, "Exact selector did not record successful completion");
}

// Protects: a transport failure remains the primary cause when worker cleanup also fails.
// Detects: returning cleanup failure first and losing the original listener error.
// Needs: the exact result-combination helper with distinct synthetic simultaneous errors.
// Breadcrumb: crates/agent-bus-http/src/http.rs (finish_http_server).
#[test]
fn simultaneous_transport_and_cleanup_errors_preserve_transport_cause() {
    let error = finish_http_server(
        Err(std::io::Error::other("synthetic transport failure")),
        Err(anyhow::anyhow!("synthetic cleanup failure")),
    )
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<std::io::Error>().unwrap().to_string(),
        "synthetic transport failure"
    );
    assert!(format!("{error:#}").contains("synthetic cleanup failure"));
    assert!(finish_http_server(Ok(()), Ok(())).is_ok());
    assert!(
        finish_http_server(Ok(()), Err(anyhow::anyhow!("cleanup-only")))
            .unwrap_err()
            .to_string()
            .contains("cleanup-only")
    );
}

// Protects: late SSE registrations after cancellation cannot leak their subscriber count.
// Detects: a rejected spawn dropping neither its sender nor its counter guard.
// Needs: a stopped worker registry and the actual /events handler with no external I/O.
// Breadcrumb: crates/agent-bus-http/src/http.rs (PubSubWorkers::spawn / SseCountGuard.)
#[tokio::test]
async fn stopped_registry_rejects_late_sse_without_counter_or_worker() {
    use tokio_stream::StreamExt as _;
    let redis = RedisFixture::new();
    let state = worker_state(&redis);
    state.pubsub_workers.shutdown().await.unwrap();
    let response = http_sse_handler(
        State(state.clone()),
        Query(SseQuery {
            agent: None,
            broadcast: true,
        }),
    )
    .await
    .into_response();
    assert!(!state.sse_subscriber_count.any());
    assert!(state.pubsub_workers.tasks.lock().unwrap().is_empty());
    assert_eq!(redis.connections.load(Ordering::Acquire), 0);
    assert!(
        response
            .into_body()
            .into_data_stream()
            .next()
            .await
            .is_none()
    );
}
