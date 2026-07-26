//! Live `PostgreSQL` contract tests for the separately migrated history catalog.
//!
//! These tests run only when `AGENT_BUS_TEST_DATABASE_URL` is explicitly set.

use agent_bus_core::history_catalog::{
    CURRENT_HISTORY_SCHEMA_VERSION, history_catalog_status, history_migration_checksum,
    migrate_history_catalog,
};
use postgres::{Client, NoTls};

fn test_database_url() -> Option<String> {
    std::env::var("AGENT_BUS_TEST_DATABASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

#[test]
fn history_catalog_migration_is_fixed_separate_and_idempotent()
-> Result<(), Box<dyn std::error::Error>> {
    let Some(database_url) = test_database_url() else {
        eprintln!("SKIP: set AGENT_BUS_TEST_DATABASE_URL to run history catalog PostgreSQL tests");
        return Ok(());
    };
    let mut client = Client::connect(&database_url, NoTls)?;
    let database_name: String = client.query_one("select current_database()", &[])?.get(0);
    assert!(
        database_name.starts_with("agent_bus_history_"),
        "refusing to mutate non-disposable database {database_name}"
    );
    let existing_schema: Option<String> = client
        .query_one("select to_regnamespace('agent_history')::text", &[])?
        .get(0);
    assert!(
        existing_schema.is_none(),
        "history catalog test requires a fresh disposable database"
    );

    let message_table_before: Option<String> = client
        .query_one("select to_regclass('agent_bus.messages')::text", &[])?
        .get(0);
    let presence_table_before: Option<String> = client
        .query_one("select to_regclass('agent_bus.presence_events')::text", &[])?
        .get(0);

    assert_failed_migration_rolls_back(&mut client)?;
    let first = migrate_history_catalog(&mut client)?;
    let second = migrate_history_catalog(&mut client)?;
    assert_eq!(first.applied_now, [CURRENT_HISTORY_SCHEMA_VERSION]);
    assert!(
        second.applied_now.is_empty(),
        "re-running migrations must be idempotent"
    );
    assert!(second.status.current);
    assert_eq!(
        second.status.applied_versions,
        [CURRENT_HISTORY_SCHEMA_VERSION]
    );

    assert_catalog_schema(&mut client)?;

    let recorded_checksum: Vec<u8> = client
        .query_one(
            "select checksum from agent_history.schema_migrations where version = $1",
            &[&CURRENT_HISTORY_SCHEMA_VERSION],
        )?
        .get(0);
    assert_eq!(
        Some(recorded_checksum.as_slice()),
        history_migration_checksum(CURRENT_HISTORY_SCHEMA_VERSION).map(<[u8; 32]>::as_slice)
    );
    assert!(history_catalog_status(&mut client)?.current);

    assert_nullable_artifact_identity_is_unique(&mut client)?;

    let message_table_after: Option<String> = client
        .query_one("select to_regclass('agent_bus.messages')::text", &[])?
        .get(0);
    let presence_table_after: Option<String> = client
        .query_one("select to_regclass('agent_bus.presence_events')::text", &[])?
        .get(0);
    assert_eq!(message_table_after, message_table_before);
    assert_eq!(presence_table_after, presence_table_before);
    Ok(())
}

fn assert_failed_migration_rolls_back(client: &mut Client) -> Result<(), postgres::Error> {
    client.batch_execute(
        "create schema agent_history; \
         create table agent_history.sessions (deliberate_conflict integer);",
    )?;
    assert!(
        migrate_history_catalog(client).is_err(),
        "a conflicting catalog object must fail migration"
    );

    let sources: Option<String> = client
        .query_one("select to_regclass('agent_history.sources')::text", &[])?
        .get(0);
    let migration_ledger: Option<String> = client
        .query_one(
            "select to_regclass('agent_history.schema_migrations')::text",
            &[],
        )?
        .get(0);
    assert!(
        sources.is_none() && migration_ledger.is_none(),
        "failed migration must roll back bootstrap and preceding catalog DDL"
    );
    client.batch_execute("drop schema agent_history cascade")?;
    Ok(())
}

fn assert_catalog_schema(client: &mut Client) -> Result<(), postgres::Error> {
    let expected_tables = [
        "artifacts",
        "events",
        "schema_migrations",
        "sessions",
        "sources",
    ];
    let actual_tables = client
        .query(
            "select table_name from information_schema.tables \
             where table_schema = 'agent_history' and table_type = 'BASE TABLE' \
             order by table_name",
            &[],
        )?
        .into_iter()
        .map(|row| row.get::<_, String>(0))
        .collect::<Vec<_>>();
    assert_eq!(actual_tables, expected_tables);

    let expected_columns = [
        ("artifacts", "artifact_id"),
        ("artifacts", "session_id"),
        ("artifacts", "kind"),
        ("artifacts", "locator_redacted"),
        ("artifacts", "drive_file_id"),
        ("artifacts", "sha256"),
        ("artifacts", "byte_count"),
        ("artifacts", "policy_version"),
        ("artifacts", "redaction_report_jsonb"),
        ("events", "event_id"),
        ("events", "session_id"),
        ("events", "provider_event_id"),
        ("events", "timestamp_utc"),
        ("events", "event_type"),
        ("events", "role"),
        ("events", "content_redacted"),
        ("events", "metadata_jsonb"),
        ("events", "source_offset"),
        ("events", "source_position"),
        ("events", "content_sha256"),
        ("schema_migrations", "version"),
        ("schema_migrations", "name"),
        ("schema_migrations", "applied_at"),
        ("schema_migrations", "checksum"),
        ("sessions", "session_id"),
        ("sessions", "source_id"),
        ("sessions", "provider_session_id"),
        ("sessions", "parser_version"),
        ("sessions", "policy_version"),
        ("sessions", "repo"),
        ("sessions", "branch"),
        ("sessions", "started_at"),
        ("sessions", "ended_at"),
        ("sessions", "event_count"),
        ("sessions", "source_sha256"),
        ("sessions", "sanitized_sha256"),
        ("sessions", "redaction_report"),
        ("sources", "source_id"),
        ("sources", "machine_id"),
        ("sources", "provider"),
        ("sources", "source_kind"),
        ("sources", "path_hash"),
        ("sources", "parser_version"),
        ("sources", "policy_version"),
        ("sources", "cursor_json"),
        ("sources", "cursor_revision"),
        ("sources", "source_state_sha256"),
        ("sources", "first_seen_at"),
        ("sources", "last_seen_at"),
        ("sources", "status"),
    ];
    let actual_columns = client
        .query(
            "select table_name, column_name from information_schema.columns \
             where table_schema = 'agent_history' \
             order by table_name, ordinal_position",
            &[],
        )?
        .into_iter()
        .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
        .collect::<Vec<_>>();
    assert_eq!(
        actual_columns,
        expected_columns
            .into_iter()
            .map(|(table, column)| (table.to_owned(), column.to_owned()))
            .collect::<Vec<_>>()
    );
    Ok(())
}

fn assert_nullable_artifact_identity_is_unique(client: &mut Client) -> Result<(), postgres::Error> {
    let first_artifact_id = [0x51_u8; 32];
    let second_artifact_id = [0x52_u8; 32];
    let artifact_sha256 = [0x53_u8; 32];
    client.execute(
        "insert into agent_history.artifacts \
         (artifact_id, kind, sha256, byte_count, policy_version) \
         values ($1, 'sanitized-jsonl', $2, 10, 'policy-v1')",
        &[&first_artifact_id.as_slice(), &artifact_sha256.as_slice()],
    )?;
    let duplicate_artifact = client.execute(
        "insert into agent_history.artifacts \
         (artifact_id, kind, sha256, byte_count, policy_version) \
         values ($1, 'sanitized-jsonl', $2, 10, 'policy-v1')",
        &[&second_artifact_id.as_slice(), &artifact_sha256.as_slice()],
    );
    assert!(
        duplicate_artifact.is_err(),
        "nullable artifact scope fields must not permit duplicate identities"
    );
    Ok(())
}
