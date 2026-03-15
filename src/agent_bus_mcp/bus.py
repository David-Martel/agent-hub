from __future__ import annotations

import json
import os
import platform
from dataclasses import dataclass, field
from datetime import datetime, timedelta, timezone
from typing import Any

import redis
from psycopg.rows import dict_row
from psycopg_pool import ConnectionPool

from .codec import codec_metadata, decode_stream_entry, dumps_compact, parse_compact, serialize_stream_payload
from .models import AckRequest, BusHealth, MessageDraft, MessageQuery, MessageRecord, PresenceAnnouncement
from .settings import AgentBusSettings


@dataclass(slots=True)
class AgentBus:
    settings: AgentBusSettings
    _redis: redis.Redis | None = field(default=None, init=False, repr=False)
    _pg_pool: ConnectionPool | None = field(default=None, init=False, repr=False)
    _storage_ready: bool = field(default=False, init=False, repr=False)
    _startup_announced: bool = field(default=False, init=False, repr=False)
    _startup_message_id: str | None = field(default=None, init=False, repr=False)
    _startup_session_id: str = field(default="", init=False, repr=False)
    _last_database_error: str | None = field(default=None, init=False, repr=False)
    _database_backoff_until: datetime | None = field(default=None, init=False, repr=False)

    def __post_init__(self) -> None:
        self._startup_session_id = PresenceAnnouncement(agent=self.settings.service_agent_id).session_id

    @classmethod
    def from_env(cls) -> "AgentBus":
        return cls(settings=AgentBusSettings())

    def redis(self) -> redis.Redis:
        if self._redis is None:
            self._redis = redis.Redis.from_url(
                self.settings.redis_url,
                decode_responses=True,
                socket_timeout=self.settings.redis_socket_timeout_seconds,
                socket_connect_timeout=self.settings.redis_socket_connect_timeout_seconds,
                health_check_interval=30,
                retry_on_timeout=True,
            )
        return self._redis

    def pg(self):
        if self._pg_pool is None:
            raise RuntimeError("AGENT_BUS_DATABASE_URL is not configured")
        return self._pg_pool.connection()

    def _database_retry_due(self) -> bool:
        return self._database_backoff_until is None or datetime.now(timezone.utc) >= self._database_backoff_until

    def _record_database_success(self) -> None:
        self._last_database_error = None
        self._database_backoff_until = None

    def _record_database_failure(self, exc: Exception | str) -> None:
        self._last_database_error = str(exc)
        cooldown = self.settings.postgres_retry_cooldown_seconds
        if cooldown > 0:
            self._database_backoff_until = datetime.now(timezone.utc) + timedelta(seconds=cooldown)
        else:
            self._database_backoff_until = None

    def _probe_database(self) -> bool:
        if not self.settings.database_url:
            return False
        if not self._database_retry_due():
            return False
        try:
            self._ensure_pg_pool()
            self._ensure_storage()
            with self.pg() as conn:
                with conn.cursor() as cur:
                    cur.execute("select 1 as ok")
                    row = cur.fetchone() or {}
                    database_ok = bool(row.get("ok"))
            self._record_database_success()
            return database_ok
        except Exception as exc:
            self._record_database_failure(exc)
            return False

    def initialize(self, *, announce_startup: bool = False) -> dict[str, Any]:
        redis_ok = bool(self.redis().ping())
        database_ok: bool | None = None

        if self.settings.database_url:
            database_ok = self._probe_database()

        if announce_startup and self.settings.startup_enabled and not self._startup_announced:
            capabilities = ["mcp", "redis"]
            if self.settings.database_url and database_ok:
                capabilities.append("postgres")
            self.set_presence(
                agent=self.settings.service_agent_id,
                status=self.settings.startup_status,
                session_id=self._startup_session_id,
                capabilities=capabilities,
                ttl_seconds=300,
                metadata={"service": "agent-bus", "startup": True, "database_ok": database_ok},
            )
            startup_message = self.post_message(
                sender=self.settings.service_agent_id,
                recipient=self.settings.startup_recipient,
                topic=self.settings.startup_topic,
                body=self.settings.startup_body,
                tags=["startup", "system", "health"],
                metadata={"service": "agent-bus", "session_id": self._startup_session_id},
            )
            self._startup_message_id = startup_message["id"]
            self._startup_announced = True

        return self.health(redis_ok=redis_ok, database_ok=database_ok)

    def close(self) -> None:
        if self._pg_pool is not None:
            self._pg_pool.close()
            self._pg_pool = None
        if self._redis is not None:
            self._redis.close()
        self._redis = None

    def health(self, *, redis_ok: bool | None = None, database_ok: bool | None = None) -> dict[str, Any]:
        if redis_ok is None:
            redis_ok = bool(self.redis().ping())
        if self.settings.database_url and database_ok is None:
            database_ok = self._probe_database()
        return BusHealth(
            ok=bool(redis_ok),
            redis_url=self.settings.redis_url,
            database_url=self.settings.database_url,
            database_ok=database_ok,
            database_error=self._last_database_error,
            runtime_metadata={
                "runtime": {
                    "python_version": platform.python_version(),
                    "implementation": platform.python_implementation(),
                },
                "process": {
                    "pid": os.getpid(),
                    "interpreter": platform.system(),
                },
                "codec": codec_metadata(),
            },
            storage_ready=self._storage_ready or not bool(self.settings.database_url),
            stream_key=self.settings.stream_key,
            channel_key=self.settings.channel_key,
            presence_prefix=self.settings.presence_prefix,
            service_agent_id=self.settings.service_agent_id,
            startup_announced=self._startup_announced,
            startup_message_id=self._startup_message_id,
            startup_session_id=self._startup_session_id,
        ).model_dump(mode="json")

    def post_message(
        self,
        *,
        sender: str,
        recipient: str,
        topic: str,
        body: str,
        tags: list[str] | None = None,
        thread_id: str | None = None,
        priority: str = "normal",
        request_ack: bool = False,
        reply_to: str | None = None,
        metadata: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        draft = MessageDraft(
            sender=sender,
            recipient=recipient,
            topic=topic,
            body=body,
            tags=tags or [],
            thread_id=thread_id,
            priority=priority,
            request_ack=request_ack,
            reply_to=reply_to,
            metadata=metadata or {},
        )
        record = MessageRecord(**draft.model_dump())
        payload = record.model_dump(mode="json", by_alias=True)

        stream_payload = serialize_stream_payload(payload)
        stream_id = self.redis().xadd(
            self.settings.stream_key,
            stream_payload,
            maxlen=self.settings.stream_maxlen,
            approximate=True,
        )
        payload["stream_id"] = stream_id
        self.redis().publish(
            self.settings.channel_key,
            dumps_compact({"event": "message", "message": payload}),
        )
        self._insert_message(record, stream_id=stream_id)
        return payload

    def ack_message(self, *, agent: str, message_id: str, body: str = "ack") -> dict[str, Any]:
        request = AckRequest(agent=agent, message_id=message_id, body=body)
        return self.post_message(
            sender=request.agent,
            recipient="all",
            topic="ack",
            body=request.body,
            priority="normal",
            request_ack=False,
            reply_to=request.message_id,
            metadata={"ack_for": request.message_id},
        )

    def list_messages(
        self,
        *,
        agent: str | None = None,
        sender: str | None = None,
        since_minutes: int = 1440,
        limit: int = 50,
        include_broadcast: bool = True,
    ) -> list[dict[str, Any]]:
        query = MessageQuery(
            agent=agent,
            sender=sender,
            since_minutes=since_minutes,
            limit=limit,
            include_broadcast=include_broadcast,
        )
        if self.settings.database_url:
            try:
                return self._list_messages_from_postgres(query=query)
            except Exception:
                return self._list_messages_from_redis(query=query)
        return self._list_messages_from_redis(query=query)

    def set_presence(
        self,
        *,
        agent: str,
        status: str,
        session_id: str | None = None,
        capabilities: list[str] | None = None,
        ttl_seconds: int = 180,
        metadata: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        payload = PresenceAnnouncement(
            agent=agent,
            status=status,
            session_id=session_id or PresenceAnnouncement(agent=agent).session_id,
            capabilities=capabilities or [],
            ttl_seconds=ttl_seconds,
            metadata=metadata or {},
        )
        presence_data = payload.model_dump(mode="json")
        self.redis().set(
            f"{self.settings.presence_prefix}{payload.agent}",
            dumps_compact(presence_data),
            ex=payload.ttl_seconds,
        )
        self._insert_presence_event(presence_data)
        self.redis().publish(
            self.settings.channel_key,
            dumps_compact({"event": "presence", "presence": presence_data}),
        )
        return presence_data

    def list_presence(self) -> list[dict[str, Any]]:
        results: list[dict[str, Any]] = []
        for key in self.redis().scan_iter(match=f"{self.settings.presence_prefix}*"):
            value = self.redis().get(key)
            if not value:
                continue
            try:
                results.append(parse_compact(value))
            except (ValueError, TypeError):
                continue
        results.sort(key=lambda item: item["agent"])
        return results

    def watch(self, *, agent: str, include_broadcast: bool = True, history: int = 0):
        normalized_agent = MessageQuery(agent=agent).agent
        if history > 0 and normalized_agent:
            for message in self.list_messages(
                agent=normalized_agent,
                since_minutes=10080,
                limit=history,
                include_broadcast=include_broadcast,
            ):
                yield {"event": "message", "message": message}

        pubsub = self.redis().pubsub(ignore_subscribe_messages=True)
        pubsub.subscribe(self.settings.channel_key)
        try:
            for item in pubsub.listen():
                data = item.get("data")
                if not isinstance(data, str):
                    continue
                try:
                    event = parse_compact(data)
                except (ValueError, TypeError):
                    continue
                if event.get("event") == "message":
                    message = event.get("message", {})
                    if message.get("to") == normalized_agent or (include_broadcast and message.get("to") == "all"):
                        yield event
                elif event.get("event") == "presence":
                    yield event
        finally:
            pubsub.close()

    def _ensure_pg_pool(self) -> None:
        if not self.settings.database_url or self._pg_pool is not None:
            return
        if not self._database_retry_due():
            raise RuntimeError(self._last_database_error or "database retry cooldown is active")
        self._pg_pool = ConnectionPool(
            conninfo=self.settings.database_url,
            min_size=self.settings.postgres_min_size,
            max_size=self.settings.postgres_max_size,
            open=False,
            timeout=self.settings.postgres_connect_timeout_seconds,
            max_idle=self.settings.postgres_max_idle_seconds,
            max_lifetime=self.settings.postgres_max_lifetime_seconds,
            num_workers=1,
            kwargs={
                "application_name": "agent_bus_mcp",
                "autocommit": True,
                "connect_timeout": self.settings.postgres_connect_timeout_seconds,
                "row_factory": dict_row,
            },
        )
        self._pg_pool.open()
        self._pg_pool.wait(timeout=self.settings.postgres_connect_timeout_seconds)

    def _decode_entry(self, stream_id: str, fields: dict[str, str]) -> dict[str, Any]:
        message = decode_stream_entry(fields)
        message["stream_id"] = stream_id
        message.setdefault("tags", [])
        message.setdefault("metadata", {})
        message.setdefault("request_ack", False)
        return message

    @staticmethod
    def _parse_utc(value: str) -> datetime:
        return datetime.fromisoformat(value.replace("Z", "+00:00")).astimezone(timezone.utc)

    def _ensure_storage(self) -> None:
        if not self.settings.database_url or self._storage_ready:
            return
        with self.pg() as conn:
            with conn.cursor() as cur:
                cur.execute("create schema if not exists agent_bus")
                cur.execute(
                    f"""
                    create table if not exists {self.settings.message_table} (
                        id uuid primary key,
                        timestamp_utc timestamptz not null,
                        sender text not null,
                        recipient text not null,
                        topic text not null,
                        body text not null,
                        priority text not null,
                        tags jsonb not null default '[]'::jsonb,
                        request_ack boolean not null default false,
                        reply_to text not null,
                        metadata jsonb not null default '{{}}'::jsonb,
                        stream_id text null
                    )
                    """
                )
                cur.execute(
                    f"""
                    create index if not exists agent_bus_messages_recipient_ts_idx
                    on {self.settings.message_table} (recipient, timestamp_utc desc)
                    """
                )
                cur.execute(
                    f"""
                    create index if not exists agent_bus_messages_sender_ts_idx
                    on {self.settings.message_table} (sender, timestamp_utc desc)
                    """
                )
                cur.execute(
                    f"""
                    create table if not exists {self.settings.presence_event_table} (
                        id bigserial primary key,
                        timestamp_utc timestamptz not null,
                        agent text not null,
                        status text not null,
                        session_id text not null,
                        capabilities jsonb not null default '[]'::jsonb,
                        metadata jsonb not null default '{{}}'::jsonb,
                        ttl_seconds integer not null
                    )
                    """
                )
        self._storage_ready = True

    def _insert_message(self, record: MessageRecord, *, stream_id: str) -> None:
        if not self.settings.database_url or not self._database_retry_due():
            return
        try:
            self._ensure_storage()
            with self.pg() as conn:
                with conn.cursor() as cur:
                    cur.execute(
                        f"""
                        insert into {self.settings.message_table}
                            (id, timestamp_utc, sender, recipient, topic, body, priority, tags, request_ack, reply_to, metadata, stream_id)
                        values
                            (%s, %s, %s, %s, %s, %s, %s, %s::jsonb, %s, %s, %s::jsonb, %s)
                        on conflict (id) do nothing
                        """,
                        [
                            str(record.id),
                            record.timestamp_utc,
                            record.sender,
                            record.recipient,
                            record.topic,
                            record.body,
                            record.priority.value,
                            json.dumps(record.tags),
                            record.request_ack,
                            record.reply_to,
                            json.dumps(record.metadata),
                            stream_id,
                        ],
                    )
            self._record_database_success()
        except Exception as exc:
            self._record_database_failure(exc)

    def _list_messages_from_postgres(self, *, query: MessageQuery) -> list[dict[str, Any]]:
        if not self._database_retry_due():
            raise RuntimeError(self._last_database_error or "database retry cooldown is active")
        self._ensure_pg_pool()
        self._ensure_storage()
        filters = ["timestamp_utc >= now() - make_interval(mins => %s)"]
        params: list[Any] = [query.since_minutes]
        if query.sender:
            filters.append("sender = %s")
            params.append(query.sender)
        if query.agent:
            if query.include_broadcast:
                filters.append("(recipient = %s or recipient = 'all')")
            else:
                filters.append("recipient = %s")
            params.append(query.agent)
        params.append(query.limit)
        where_sql = " and ".join(filters)
        with self.pg() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    f"""
                    select
                        id::text as id,
                        timestamp_utc,
                        sender as "from",
                        recipient as "to",
                        topic,
                        body,
                        priority,
                        tags,
                        request_ack,
                        reply_to,
                        metadata,
                        coalesce(stream_id, '') as stream_id
                    from {self.settings.message_table}
                    where {where_sql}
                    order by timestamp_utc desc
                    limit %s
                    """,
                    params,
                )
                rows = cur.fetchall()
        rows.reverse()
        return [
            {
                **row,
                "timestamp_utc": row["timestamp_utc"].astimezone(timezone.utc).isoformat(),
                "tags": row["tags"] or [],
                "metadata": row["metadata"] or {},
                "request_ack": bool(row["request_ack"]),
            }
            for row in rows
        ]

    def _list_messages_from_redis(self, *, query: MessageQuery) -> list[dict[str, Any]]:
        raw_entries = self.redis().xrevrange(self.settings.stream_key, count=max(query.limit * 5, 200))
        cutoff = datetime.now(timezone.utc) - timedelta(minutes=query.since_minutes)
        results: list[dict[str, Any]] = []

        for stream_id, fields in raw_entries:
            message = self._decode_entry(stream_id, fields)
            if self._parse_utc(message["timestamp_utc"]) < cutoff:
                continue
            if query.sender and message["from"] != query.sender:
                continue
            if query.agent:
                if message["to"] != query.agent and not (query.include_broadcast and message["to"] == "all"):
                    continue
            results.append(message)
            if len(results) >= query.limit:
                break

        results.reverse()
        return results

    def _insert_presence_event(self, payload: dict[str, Any]) -> None:
        if not self.settings.database_url or not self._database_retry_due():
            return
        try:
            self._ensure_storage()
            with self.pg() as conn:
                with conn.cursor() as cur:
                    cur.execute(
                        f"""
                        insert into {self.settings.presence_event_table}
                            (timestamp_utc, agent, status, session_id, capabilities, metadata, ttl_seconds)
                        values
                            (%s, %s, %s, %s, %s::jsonb, %s::jsonb, %s)
                        """,
                        [
                            payload["timestamp_utc"],
                            payload["agent"],
                            payload["status"],
                            payload["session_id"],
                            json.dumps(payload["capabilities"]),
                            json.dumps(payload["metadata"]),
                            payload["ttl_seconds"],
                        ],
                    )
            self._record_database_success()
        except Exception as exc:
            self._record_database_failure(exc)
