//! Separately migrated `PostgreSQL` catalog for sanitized agent history.

use postgres::Client;
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

const HISTORY_SCHEMA: &str = "agent_history";
const MIGRATION_TABLE: &str = "agent_history.schema_migrations";

// Stable process-independent lock key. Changing it would break serialization
// with older binaries, so it is deliberately a fixed protocol constant.
const HISTORY_MIGRATION_LOCK_KEY: i64 = 0x4147_4849_5354_4f52;

const MIGRATION_BOOTSTRAP_SQL: &str = "
create schema if not exists agent_history;
create table if not exists agent_history.schema_migrations (
    version bigint primary key,
    name text not null unique,
    applied_at timestamptz not null default now(),
    checksum bytea not null check (octet_length(checksum) = 32)
);
";

const INITIAL_CATALOG_SQL: &str = include_str!("../migrations/history/0001_initial_catalog.sql");
const INITIAL_CATALOG_CHECKSUM: [u8; 32] = [
    0xf0, 0xbb, 0x4a, 0x1f, 0x07, 0xb7, 0x36, 0x2d, 0x79, 0x05, 0x5f, 0x15, 0x16, 0x38, 0xb0, 0x45,
    0x69, 0xfe, 0x35, 0xc2, 0x4e, 0x92, 0x64, 0x10, 0xff, 0x7c, 0xe0, 0xe3, 0x12, 0x76, 0xf9, 0x42,
];

const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "0001_initial_catalog",
    checksum: &INITIAL_CATALOG_CHECKSUM,
    sql: INITIAL_CATALOG_SQL,
}];

/// Latest history catalog schema version known to this binary.
pub const CURRENT_HISTORY_SCHEMA_VERSION: i64 = 1;

#[derive(Clone, Copy, Debug)]
struct Migration {
    version: i64,
    name: &'static str,
    checksum: &'static [u8; 32],
    sql: &'static str,
}

/// Failure while validating or migrating the history catalog.
#[derive(Debug, Error)]
pub enum HistoryCatalogError {
    /// `PostgreSQL` rejected a query or migration.
    #[error("history catalog database error: {0}")]
    Database(#[from] postgres::Error),

    /// A compiled migration does not match its pinned build-time checksum.
    #[error(
        "history migration {version} manifest checksum is invalid: expected {expected}, computed {computed}"
    )]
    InvalidManifestChecksum {
        /// Migration version.
        version: i64,
        /// Pinned checksum.
        expected: String,
        /// Checksum computed from the compiled SQL.
        computed: String,
    },

    /// The database maps a known version to a different migration name.
    #[error(
        "fatal history migration identity drift at version {version}: expected name {expected}, database has {actual}"
    )]
    IdentityDrift {
        /// Migration version.
        version: i64,
        /// Migration name compiled into this binary.
        expected: String,
        /// Migration name recorded in `PostgreSQL`.
        actual: String,
    },

    /// The database contains a known migration version with different SQL.
    #[error(
        "fatal history migration checksum drift at version {version}: expected {expected}, database has {actual}"
    )]
    ChecksumDrift {
        /// Migration version.
        version: i64,
        /// Checksum compiled into this binary.
        expected: String,
        /// Checksum recorded in `PostgreSQL`.
        actual: String,
    },
}

/// Read-only description of the catalog migration state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HistoryCatalogStatus {
    /// `PostgreSQL` schema name.
    pub schema: String,
    /// Whether the migration metadata table exists.
    pub installed: bool,
    /// Whether every known migration is applied and no unknown versions exist.
    pub current: bool,
    /// Latest migration version known to this binary.
    pub latest_available_version: i64,
    /// Known migration versions recorded by `PostgreSQL`.
    pub applied_versions: Vec<i64>,
    /// Known migration versions not yet recorded.
    pub pending_versions: Vec<i64>,
    /// Database migration versions not known to this binary.
    pub unknown_versions: Vec<i64>,
}

/// Result of applying all pending history catalog migrations.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HistoryMigrationReport {
    /// Versions applied by this invocation.
    pub applied_now: Vec<i64>,
    /// State after the migration transaction committed.
    pub status: HistoryCatalogStatus,
}

/// Compute a stable source identifier.
#[must_use]
pub fn deterministic_source_id(
    machine_id: &str,
    provider: &str,
    source_kind: &str,
    path_hash: &[u8; 32],
) -> [u8; 32] {
    deterministic_id(
        b"agent-history/source/v1",
        &[
            machine_id.as_bytes(),
            provider.as_bytes(),
            source_kind.as_bytes(),
            path_hash,
        ],
    )
}

/// Compute a stable parser- and policy-specific provider-session identifier.
#[must_use]
pub fn deterministic_session_id(
    source_id: &[u8; 32],
    provider_session_id: &str,
    parser_version: &str,
    policy_version: &str,
) -> [u8; 32] {
    deterministic_id(
        b"agent-history/session/v1",
        &[
            source_id,
            provider_session_id.as_bytes(),
            parser_version.as_bytes(),
            policy_version.as_bytes(),
        ],
    )
}

/// Compute a stable provider-event identifier within a versioned session.
#[must_use]
pub fn deterministic_event_id(session_id: &[u8; 32], provider_event_id: &str) -> [u8; 32] {
    deterministic_id(
        b"agent-history/event/v1",
        &[session_id, provider_event_id.as_bytes()],
    )
}

/// Compute a stable identity for a sanitized artifact.
///
/// Optional session and locator fields are presence-tagged so a missing value
/// cannot collide with an explicitly empty value.
#[must_use]
pub fn deterministic_artifact_id(
    session_id: Option<&[u8; 32]>,
    kind: &str,
    locator_redacted: Option<&str>,
    sha256: &[u8; 32],
    policy_version: &str,
) -> [u8; 32] {
    let (session_presence, session_value): (&[u8], &[u8]) = match session_id {
        Some(value) => (b"\x01", value),
        None => (b"\x00", b""),
    };
    let (locator_presence, locator_value): (&[u8], &[u8]) = match locator_redacted {
        Some(value) => (b"\x01", value.as_bytes()),
        None => (b"\x00", b""),
    };

    deterministic_id(
        b"agent-history/artifact/v1",
        &[
            session_presence,
            session_value,
            kind.as_bytes(),
            locator_presence,
            locator_value,
            sha256,
            policy_version.as_bytes(),
        ],
    )
}

/// Return the pinned binary checksum for a known migration version.
#[must_use]
pub fn history_migration_checksum(version: i64) -> Option<&'static [u8; 32]> {
    MIGRATIONS
        .iter()
        .find(|migration| migration.version == version)
        .map(|migration| migration.checksum)
}

/// Inspect migration state without creating or modifying schema objects.
///
/// # Errors
///
/// Returns an error for `PostgreSQL` failures, invalid compiled migration
/// manifests, or fatal identity/checksum drift in any known applied migration.
pub fn history_catalog_status(
    client: &mut Client,
) -> Result<HistoryCatalogStatus, HistoryCatalogError> {
    validate_migration_manifest()?;

    let migration_table: Option<String> = client
        .query_one("select to_regclass($1)::text", &[&MIGRATION_TABLE])?
        .get(0);
    if migration_table.is_none() {
        return Ok(HistoryCatalogStatus {
            schema: HISTORY_SCHEMA.to_owned(),
            installed: false,
            current: false,
            latest_available_version: CURRENT_HISTORY_SCHEMA_VERSION,
            applied_versions: Vec::new(),
            pending_versions: MIGRATIONS
                .iter()
                .map(|migration| migration.version)
                .collect(),
            unknown_versions: Vec::new(),
        });
    }

    let rows = client.query(
        "select version, name, checksum from agent_history.schema_migrations order by version",
        &[],
    )?;
    status_from_rows(rows.iter().map(|row| (row.get(0), row.get(1), row.get(2))))
}

/// Apply every pending history catalog migration in one serialized transaction.
///
/// This is an explicit-only entry point; normal `PostgreSQL` storage setup never
/// calls it.
///
/// # Errors
///
/// Returns an error for `PostgreSQL` failures, invalid compiled migration
/// manifests, or fatal identity/checksum drift in any known applied migration.
pub fn migrate_history_catalog(
    client: &mut Client,
) -> Result<HistoryMigrationReport, HistoryCatalogError> {
    validate_migration_manifest()?;

    let mut transaction = client.transaction()?;
    transaction.query_one(
        "select pg_advisory_xact_lock($1)",
        &[&HISTORY_MIGRATION_LOCK_KEY],
    )?;
    transaction.batch_execute(MIGRATION_BOOTSTRAP_SQL)?;

    let mut applied_now = Vec::new();
    for migration in MIGRATIONS {
        let stored = transaction.query_opt(
            "select name, checksum from agent_history.schema_migrations where version = $1",
            &[&migration.version],
        )?;
        if let Some(row) = stored {
            let name: String = row.get(0);
            let checksum: Vec<u8> = row.get(1);
            ensure_migration_matches(migration, &name, &checksum)?;
            continue;
        }

        transaction.batch_execute(migration.sql)?;
        let checksum: &[u8] = migration.checksum;
        transaction.execute(
            "insert into agent_history.schema_migrations (version, name, checksum) \
             values ($1, $2, $3)",
            &[&migration.version, &migration.name, &checksum],
        )?;
        applied_now.push(migration.version);
    }
    transaction.commit()?;

    let status = history_catalog_status(client)?;
    Ok(HistoryMigrationReport {
        applied_now,
        status,
    })
}

fn deterministic_id(domain: &[u8], components: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    update_length_prefixed(&mut hasher, domain);
    for component in components {
        update_length_prefixed(&mut hasher, component);
    }
    hasher.finalize().into()
}

fn update_length_prefixed(hasher: &mut Sha256, value: &[u8]) {
    let length = u64::try_from(value.len()).expect("usize always fits u64 on supported targets");
    hasher.update(length.to_be_bytes());
    hasher.update(value);
}

fn migration_checksum(sql: &str) -> [u8; 32] {
    Sha256::digest(sql.as_bytes()).into()
}

fn bytes_to_hex(value: &[u8]) -> String {
    use std::fmt::Write as _;

    value.iter().fold(
        String::with_capacity(value.len() * 2),
        |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to String cannot fail");
            output
        },
    )
}

fn validate_migration_manifest() -> Result<(), HistoryCatalogError> {
    for migration in MIGRATIONS {
        let computed = migration_checksum(migration.sql);
        if computed != *migration.checksum {
            return Err(HistoryCatalogError::InvalidManifestChecksum {
                version: migration.version,
                expected: bytes_to_hex(migration.checksum),
                computed: bytes_to_hex(&computed),
            });
        }
    }
    Ok(())
}

fn ensure_migration_matches(
    migration: &Migration,
    name: &str,
    checksum: &[u8],
) -> Result<(), HistoryCatalogError> {
    if name != migration.name {
        return Err(HistoryCatalogError::IdentityDrift {
            version: migration.version,
            expected: migration.name.to_owned(),
            actual: name.to_owned(),
        });
    }
    if checksum != migration.checksum {
        return Err(HistoryCatalogError::ChecksumDrift {
            version: migration.version,
            expected: bytes_to_hex(migration.checksum),
            actual: bytes_to_hex(checksum),
        });
    }
    Ok(())
}

fn status_from_rows(
    rows: impl IntoIterator<Item = (i64, String, Vec<u8>)>,
) -> Result<HistoryCatalogStatus, HistoryCatalogError> {
    let rows = rows.into_iter().collect::<Vec<_>>();
    for migration in MIGRATIONS {
        if let Some((_, name, checksum)) = rows
            .iter()
            .find(|(version, _, _)| *version == migration.version)
        {
            ensure_migration_matches(migration, name, checksum)?;
        }
    }

    let applied_versions = MIGRATIONS
        .iter()
        .filter(|migration| {
            rows.iter()
                .any(|(version, _, _)| *version == migration.version)
        })
        .map(|migration| migration.version)
        .collect::<Vec<_>>();
    let pending_versions = MIGRATIONS
        .iter()
        .filter(|migration| {
            !rows
                .iter()
                .any(|(version, _, _)| *version == migration.version)
        })
        .map(|migration| migration.version)
        .collect::<Vec<_>>();
    let unknown_versions = rows
        .iter()
        .filter(|(version, _, _)| {
            !MIGRATIONS
                .iter()
                .any(|migration| migration.version == *version)
        })
        .map(|(version, _, _)| *version)
        .collect::<Vec<_>>();

    Ok(HistoryCatalogStatus {
        schema: HISTORY_SCHEMA.to_owned(),
        installed: true,
        current: pending_versions.is_empty() && unknown_versions.is_empty(),
        latest_available_version: CURRENT_HISTORY_SCHEMA_VERSION,
        applied_versions,
        pending_versions,
        unknown_versions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_ids_are_stable_and_domain_separated() {
        let path_hash = [0x11; 32];
        let source = deterministic_source_id("machine-a", "codex", "jsonl", &path_hash);
        assert_eq!(
            source,
            [
                0x6c, 0x1f, 0x86, 0xdd, 0x26, 0x69, 0x10, 0xf5, 0x06, 0xa0, 0x0a, 0x0c, 0xea, 0x22,
                0x02, 0x34, 0x1a, 0x6f, 0x2d, 0xfc, 0xa6, 0x79, 0xd4, 0x72, 0x85, 0x99, 0xde, 0x03,
                0x0d, 0x06, 0x86, 0xf3,
            ]
        );
        let session = deterministic_session_id(&source, "session-7", "parser-v2", "policy-v3");
        assert_eq!(
            session,
            [
                0x60, 0xf2, 0x29, 0x5c, 0xee, 0xea, 0x94, 0x86, 0x51, 0x30, 0x51, 0xbd, 0xcd, 0xcf,
                0xfb, 0xc0, 0x7f, 0x61, 0x72, 0x0a, 0x26, 0xba, 0xe9, 0x82, 0x84, 0x6b, 0x7c, 0x41,
                0x06, 0x49, 0xee, 0xdd,
            ]
        );
        assert_eq!(
            deterministic_event_id(&session, "event-9"),
            [
                0xed, 0x27, 0x4c, 0x67, 0xeb, 0xa6, 0xa7, 0x04, 0x43, 0xed, 0x07, 0x98, 0xd5, 0x91,
                0x5c, 0x67, 0x44, 0xc2, 0x12, 0x62, 0xaf, 0x95, 0xee, 0x91, 0xe2, 0xcf, 0xd0, 0xbf,
                0x2a, 0x50, 0xeb, 0xb4,
            ]
        );
        assert_ne!(
            source,
            deterministic_session_id(&path_hash, "machine-acodexjsonl", "parser-v2", "policy-v3")
        );
        assert_ne!(
            deterministic_session_id(&source, "session-7", "parser-v2", "policy-v3"),
            deterministic_event_id(&source, "session-7")
        );
    }

    #[test]
    fn session_identity_includes_parser_and_policy_versions() {
        let source = [0x22; 32];
        assert_ne!(
            deterministic_session_id(&source, "session", "parser-v1", "policy-v1"),
            deterministic_session_id(&source, "session", "parser-v2", "policy-v1")
        );
        assert_ne!(
            deterministic_session_id(&source, "session", "parser-v1", "policy-v1"),
            deterministic_session_id(&source, "session", "parser-v1", "policy-v2")
        );
    }

    #[test]
    fn artifact_identity_is_stable_and_preserves_optional_field_presence() {
        let session = [0x33; 32];
        let sha256 = [0x44; 32];
        let artifact = deterministic_artifact_id(
            Some(&session),
            "sanitized-jsonl",
            Some("drive:item"),
            &sha256,
            "policy-v1",
        );
        assert_eq!(
            artifact,
            deterministic_artifact_id(
                Some(&session),
                "sanitized-jsonl",
                Some("drive:item"),
                &sha256,
                "policy-v1",
            )
        );
        assert_ne!(
            deterministic_artifact_id(None, "sanitized-jsonl", None, &sha256, "policy-v1"),
            deterministic_artifact_id(None, "sanitized-jsonl", Some(""), &sha256, "policy-v1",)
        );
        assert_ne!(
            artifact,
            deterministic_artifact_id(
                Some(&session),
                "sanitized-jsonl",
                Some("drive:item"),
                &sha256,
                "policy-v2",
            )
        );
    }

    #[test]
    fn length_prefixes_prevent_component_boundary_collisions() {
        assert_ne!(
            deterministic_id(b"test", &[b"ab", b"c"]),
            deterministic_id(b"test", &[b"a", b"bc"])
        );
        assert_ne!(
            deterministic_id(b"test", &[b"", b"abc"]),
            deterministic_id(b"test", &[b"abc", b""])
        );
    }

    #[test]
    fn compiled_migration_matches_pinned_checksum() {
        validate_migration_manifest().expect("migration SQL checksum must match manifest");
    }

    #[test]
    fn migration_contract_preserves_incomplete_provider_records() {
        assert!(
            INITIAL_CATALOG_SQL
                .contains("source_state_sha256 is null or octet_length(source_state_sha256) = 32")
        );
        assert!(INITIAL_CATALOG_SQL.contains("first_seen_at timestamptz not null default now()"));
        assert!(INITIAL_CATALOG_SQL.contains("started_at timestamptz"));
        assert!(INITIAL_CATALOG_SQL.contains("source_position jsonb"));
        assert!(INITIAL_CATALOG_SQL.contains("locator_redacted text"));
        assert!(!INITIAL_CATALOG_SQL.contains("local_uri"));
    }

    #[test]
    fn status_rejects_known_version_checksum_drift() {
        let error = status_from_rows([(1, "0001_initial_catalog".to_owned(), vec![0; 32])])
            .expect_err("a known migration with a different checksum must be fatal");
        assert!(matches!(
            error,
            HistoryCatalogError::ChecksumDrift { version: 1, .. }
        ));
    }

    #[test]
    fn status_rejects_known_version_identity_drift() {
        let error =
            status_from_rows([(1, "renamed".to_owned(), INITIAL_CATALOG_CHECKSUM.to_vec())])
                .expect_err("a known migration with a different name must be fatal");
        assert!(matches!(
            error,
            HistoryCatalogError::IdentityDrift { version: 1, .. }
        ));
    }

    #[test]
    fn status_reports_pending_and_unknown_versions() {
        let status = status_from_rows([(2, "future".to_owned(), vec![0xff; 32])])
            .expect("unknown versions are reported");
        assert!(!status.current);
        assert_eq!(status.pending_versions, [1]);
        assert_eq!(status.unknown_versions, [2]);
    }
}
