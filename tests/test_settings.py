from __future__ import annotations

import os

import pytest

from agent_bus_mcp.settings import AgentBusSettings


def test_settings_default_redis_port_is_6380(monkeypatch) -> None:
    # Isolate from env vars that override defaults
    for key in list(os.environ):
        if key.startswith("AGENT_BUS_") or key == "REDIS_URL":
            monkeypatch.delenv(key, raising=False)
    settings = AgentBusSettings()
    assert settings.redis_url == "redis://localhost:6380/0"
    assert settings.postgres_min_size == 0
    assert settings.postgres_max_size == 2


def test_settings_normalize_loopback_to_localhost() -> None:
    settings = AgentBusSettings(
        AGENT_BUS_REDIS_URL="redis://127.0.0.1:6380/0",
        AGENT_BUS_DATABASE_URL="postgresql://postgres:password@127.0.0.1:5300/postgres",
    )
    assert settings.redis_url == "redis://localhost:6380/0"
    assert settings.database_url == "postgresql://postgres:password@localhost:5300/postgres"


def test_settings_reject_non_localhost_hosts() -> None:
    with pytest.raises(ValueError):
        AgentBusSettings(AGENT_BUS_REDIS_URL="redis://192.168.1.10:6379/0")
