create table agent_history.sources (
    source_id bytea primary key
        check (octet_length(source_id) = 32),
    machine_id text not null,
    provider text not null,
    source_kind text not null,
    path_hash bytea not null
        check (octet_length(path_hash) = 32),
    parser_version text not null,
    policy_version text not null,
    cursor_json jsonb not null default '{}'::jsonb,
    cursor_revision bigint not null default 0
        check (cursor_revision >= 0),
    source_state_sha256 bytea
        check (source_state_sha256 is null or octet_length(source_state_sha256) = 32),
    first_seen_at timestamptz not null default now(),
    last_seen_at timestamptz not null default now(),
    status text not null,
    unique (machine_id, provider, source_kind, path_hash),
    check (last_seen_at >= first_seen_at)
);

create table agent_history.sessions (
    session_id bytea primary key
        check (octet_length(session_id) = 32),
    source_id bytea not null
        references agent_history.sources (source_id) on delete restrict,
    provider_session_id text not null,
    parser_version text not null,
    policy_version text not null,
    repo text,
    branch text,
    started_at timestamptz,
    ended_at timestamptz,
    event_count bigint not null default 0
        check (event_count >= 0),
    source_sha256 bytea
        check (source_sha256 is null or octet_length(source_sha256) = 32),
    sanitized_sha256 bytea
        check (sanitized_sha256 is null or octet_length(sanitized_sha256) = 32),
    redaction_report jsonb not null default '{}'::jsonb,
    unique (source_id, provider_session_id, parser_version, policy_version),
    check (started_at is null or ended_at is null or ended_at >= started_at)
);

create table agent_history.events (
    event_id bytea primary key
        check (octet_length(event_id) = 32),
    session_id bytea not null
        references agent_history.sessions (session_id) on delete restrict,
    provider_event_id text not null,
    timestamp_utc timestamptz,
    event_type text not null,
    role text,
    content_redacted text not null,
    metadata_jsonb jsonb not null default '{}'::jsonb,
    source_offset bigint
        check (source_offset is null or source_offset >= 0),
    source_position jsonb not null default '{}'::jsonb,
    content_sha256 bytea not null
        check (octet_length(content_sha256) = 32),
    unique (session_id, provider_event_id)
);

create table agent_history.artifacts (
    artifact_id bytea primary key
        check (octet_length(artifact_id) = 32),
    session_id bytea
        references agent_history.sessions (session_id) on delete restrict,
    kind text not null,
    locator_redacted text,
    drive_file_id text,
    sha256 bytea not null
        check (octet_length(sha256) = 32),
    byte_count bigint not null
        check (byte_count >= 0),
    policy_version text not null,
    redaction_report_jsonb jsonb not null default '{}'::jsonb
);

create index history_sources_machine_provider_status_idx
    on agent_history.sources (machine_id, provider, status);
create index history_sources_last_seen_idx
    on agent_history.sources (last_seen_at);
create index history_sessions_source_started_idx
    on agent_history.sessions (source_id, started_at);
create index history_sessions_repo_started_idx
    on agent_history.sessions (repo, started_at)
    where repo is not null;
create index history_events_session_source_offset_idx
    on agent_history.events (session_id, source_offset);
create index history_events_timestamp_idx
    on agent_history.events (timestamp_utc)
    where timestamp_utc is not null;
create index history_artifacts_session_idx
    on agent_history.artifacts (session_id);
create unique index history_artifacts_session_locator_identity_idx
    on agent_history.artifacts (session_id, kind, locator_redacted, sha256, policy_version)
    where session_id is not null and locator_redacted is not null;
create unique index history_artifacts_session_identity_idx
    on agent_history.artifacts (session_id, kind, sha256, policy_version)
    where session_id is not null and locator_redacted is null;
create unique index history_artifacts_global_locator_identity_idx
    on agent_history.artifacts (kind, locator_redacted, sha256, policy_version)
    where session_id is null and locator_redacted is not null;
create unique index history_artifacts_global_identity_idx
    on agent_history.artifacts (kind, sha256, policy_version)
    where session_id is null and locator_redacted is null;
