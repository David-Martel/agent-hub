//! Bounded synchronous replay durability using an owned async PG connection.
use crate::models::{Message, Presence, Sensitivity};
use crate::postgres_store::{parse_timestamp_utc, postgres_storage_sql};
use crate::settings::Settings;
use std::time::Duration;
use uuid::Uuid;

#[derive(Clone, Copy)]
pub(crate) enum Event<'a> {
    Message(&'a Message),
    Presence(&'a Presence, Uuid, &'a str),
}

pub(crate) fn persist(settings: &Settings, event: Event<'_>) -> bool {
    let Some(url) = settings.database_url.as_deref() else {
        return true;
    };
    if crate::settings::validate_identifier(&settings.message_table, "message_table").is_err()
        || crate::settings::validate_identifier(
            &settings.presence_event_table,
            "presence_event_table",
        )
        .is_err()
    {
        return false;
    }
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return false;
    };
    let work = async {
        let (client, connection) = tokio_postgres::connect(url, tokio_postgres::NoTls).await?;
        // No spawned writer/pump. Both futures are dropped together on timeout
        // or query completion, so the owned PG connection cannot outlive this call.
        let operation = async {
            client
                .batch_execute("create schema if not exists agent_bus")
                .await?;
            client
                .batch_execute(&postgres_storage_sql(settings))
                .await?;
            match event {
                Event::Message(message) => {
                    let message_id = Uuid::parse_str(&message.id)
                        .map_err(|e| anyhow::anyhow!("invalid replay message identity: {e}"))?;
                    let timestamp_utc = parse_timestamp_utc(&message.timestamp_utc)?;
                    let tags = serde_json::Value::Array(
                        message
                            .tags
                            .iter()
                            .cloned()
                            .map(serde_json::Value::String)
                            .collect(),
                    );
                    let reply_to = message.reply_to.clone().unwrap_or_default();

                    client.execute(
                &format!(
                    "insert into {} \
                     (id, timestamp_utc, protocol_version, sender, recipient, topic, body, thread_id, priority, tags, request_ack, reply_to, metadata, stream_id, \
                      client_msg_id, origin_hub, origin_seq, hlc, sensitivity) \
                     values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, \
                      $15, $16, $17, $18, $19) \
                     on conflict do nothing",
                    settings.message_table
                ),
                &[
                    &message_id,
                    &timestamp_utc,
                    &message.protocol_version,
                    &message.from,
                    &message.to,
                    &message.topic,
                    &message.body,
                    &message.thread_id,
                    &message.priority,
                    &tags,
                    &message.request_ack,
                    &reply_to,
                    &message.metadata,
                    &message.stream_id,
                    &message.client_msg_id,
                    &message.origin_hub,
                    &message.origin_seq.map(i64::try_from).transpose()?,
                    &message.hlc,
                    &message.sensitivity.map(Sensitivity::as_str),
                ],
            ).await?;
                }
                Event::Presence(presence, request_id, hub) => {
                    let timestamp_utc = parse_timestamp_utc(&presence.timestamp_utc)?;
                    let capabilities = serde_json::json!(presence.capabilities);
                    let ttl_seconds = i64::try_from(presence.ttl_seconds)?;
                    client.execute(&format!("insert into {} (timestamp_utc,protocol_version,agent,status,session_id,capabilities,metadata,ttl_seconds,replay_hub,replay_request_id) values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) on conflict do nothing",settings.presence_event_table),
                        &[&timestamp_utc,&presence.protocol_version,&presence.agent,&presence.status,&presence.session_id,&capabilities,&presence.metadata,&ttl_seconds,&hub,&request_id]).await?;
                }
            }
            Ok::<(), anyhow::Error>(())
        };
        tokio::pin!(operation);
        tokio::pin!(connection);
        tokio::select! {
            result=&mut operation=>result,
            _=&mut connection=>Err(anyhow::anyhow!("replay PG connection ended")),
        }
    };
    // Caller is synchronous (HTTP spawn_blocking or CLI); an entered runtime
    // gets block_in_place just like the existing postgres wrapper.
    let result = crate::postgres_store::run_postgres_blocking(|| {
        Ok(runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(2), work).await })
            .is_ok_and(|result| result.is_ok()))
    })
    .unwrap_or(false);
    runtime.shutdown_timeout(Duration::from_millis(100));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    #[test]
    fn outbox_stalled_pg_handshake_is_bounded_and_owned_connection_reaches_eof() {
        let listener = std::net::TcpListener::bind("localhost:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(4);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "PG fixture connection never arrived"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("owned PG fixture accept: {error}"),
                }
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut bytes = [0; 4096];
            let first = socket.read(&mut bytes).unwrap();
            assert!(first > 0, "actual handshake required");
            // No PG response: production must cancel this exact owned connection.
            loop {
                match socket.read(&mut bytes) {
                    Ok(0) => return true,
                    Ok(_) => {}
                    Err(error) => panic!("PG peer did not reach EOF: {error}"),
                }
            }
        });
        let mut settings = Settings::for_test();
        settings.database_url = Some(format!("postgresql://fixture@{address}/outbox"));
        let presence = Presence {
            agent: "fixture".to_owned(),
            status: "online".to_owned(),
            protocol_version: "1.0".to_owned(),
            timestamp_utc: "2026-01-01T00:00:00.000000Z".to_owned(),
            session_id: "fixture".to_owned(),
            capabilities: vec![],
            metadata: serde_json::json!({}),
            ttl_seconds: 1,
        };
        let start = std::time::Instant::now();
        assert!(!persist(
            &settings,
            Event::Presence(&presence, Uuid::new_v4(), "fixture")
        ));
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(1900));
        assert!(elapsed < Duration::from_secs(3));
        assert!(server.join().unwrap());
    }
}
