from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

from agent_bus_mcp.bus import AgentBus
from agent_bus_mcp.settings import AgentBusSettings


class FakePubSub:
    def subscribe(self, channel: str) -> None:
        self.channel = channel

    def listen(self):
        if False:
            yield {}

    def close(self) -> None:
        return None


@dataclass
class FakeRedis:
    stream_messages: list[tuple[str, dict[str, str]]] = field(default_factory=list)
    published_events: list[tuple[str, str]] = field(default_factory=list)
    kv: dict[str, str] = field(default_factory=dict)
    next_stream_id: str = "1700000000000-0"

    def ping(self) -> bool:
        return True

    def xadd(self, key: str, payload: dict[str, str], maxlen: int, approximate: bool) -> str:
        self.stream_messages.append((key, payload))
        return self.next_stream_id

    def publish(self, channel: str, payload: str) -> int:
        self.published_events.append((channel, payload))
        return 1

    def set(self, key: str, value: str, ex: int) -> bool:
        self.kv[key] = value
        return True

    def get(self, key: str) -> str | None:
        return self.kv.get(key)

    def scan_iter(self, match: str):
        prefix = match.rstrip("*")
        for key in self.kv:
            if key.startswith(prefix):
                yield key

    def xrevrange(self, key: str, count: int):
        return [
            (self.next_stream_id, payload)
            for stream_key, payload in reversed(self.stream_messages)
            if stream_key == key
        ][:count]

    def pubsub(self, ignore_subscribe_messages: bool = True) -> FakePubSub:
        return FakePubSub()

    def close(self) -> None:
        return None


class FakeCursor:
    def __init__(self, rows: list[dict[str, Any]] | None = None) -> None:
        self.rows = rows or []
        self.commands: list[tuple[str, Any]] = []

    def execute(self, sql: str, params: Any = None) -> None:
        self.commands.append((sql, params))

    def fetchone(self):
        if self.rows:
            return self.rows.pop(0)
        return {"ok": 1}

    def fetchall(self):
        return self.rows

    def __enter__(self) -> "FakeCursor":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        return None


class FakeConnection:
    def __init__(self, cursor: FakeCursor) -> None:
        self._cursor = cursor

    def cursor(self) -> FakeCursor:
        return self._cursor

    def __enter__(self) -> "FakeConnection":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        return None


class FakePool:
    def __init__(self) -> None:
        self.cursor = FakeCursor()
        self.closed = False

    def open(self) -> None:
        return None

    def wait(self, timeout: int) -> None:
        return None

    def connection(self) -> FakeConnection:
        return FakeConnection(self.cursor)

    def close(self) -> None:
        self.closed = True


def build_bus() -> tuple[AgentBus, FakeRedis, FakePool]:
    settings = AgentBusSettings(
        AGENT_BUS_REDIS_URL="redis://localhost:6379/0",
        AGENT_BUS_DATABASE_URL="postgresql://postgres:password@localhost:5300/postgres",
    )
    bus = AgentBus(settings=settings)
    fake_redis = FakeRedis()
    fake_pool = FakePool()
    bus._redis = fake_redis
    bus._pg_pool = fake_pool
    return bus, fake_redis, fake_pool


def test_initialize_announces_startup_once() -> None:
    bus, fake_redis, _ = build_bus()

    health = bus.initialize(announce_startup=True)
    assert health["ok"] is True
    assert bus._startup_announced is True
    assert len(fake_redis.stream_messages) == 1

    bus.initialize(announce_startup=True)
    assert len(fake_redis.stream_messages) == 1


def test_post_message_publishes_and_persists() -> None:
    bus, fake_redis, fake_pool = build_bus()

    message = bus.post_message(sender="codex", recipient="claude", topic="status", body="ready")

    assert message["from"] == "codex"
    assert message["to"] == "claude"
    assert fake_redis.stream_messages
    assert fake_redis.published_events
    assert any("insert into" in sql.lower() for sql, _ in fake_pool.cursor.commands)


def test_list_messages_falls_back_to_redis_when_database_errors(monkeypatch) -> None:
    bus, _, _ = build_bus()
    bus.post_message(sender="codex", recipient="claude", topic="status", body="fallback")

    def blow_up(self, *, query):
        raise RuntimeError("postgres unavailable")

    monkeypatch.setattr(AgentBus, "_list_messages_from_postgres", blow_up)

    messages = bus.list_messages(agent="claude")
    assert len(messages) == 1
    assert messages[0]["body"] == "fallback"


def test_initialize_tolerates_database_failure(monkeypatch) -> None:
    bus, _, _ = build_bus()

    def blow_up(self):
        raise RuntimeError("postgres unavailable")

    monkeypatch.setattr(AgentBus, "_ensure_pg_pool", blow_up)

    health = bus.initialize(announce_startup=False)
    assert health["ok"] is True
    assert health["database_ok"] is False
    assert "postgres unavailable" in health["database_error"]
