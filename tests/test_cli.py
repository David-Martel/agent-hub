from __future__ import annotations

import pytest

from agent_bus_mcp import cli


class FakeBus:
    def __init__(self) -> None:
        self.initialized_with: list[bool] = []
        self.closed = False

    def initialize(self, *, announce_startup: bool = False):
        self.initialized_with.append(announce_startup)
        return {"ok": True}

    def health(self):
        return {"ok": True}

    def close(self) -> None:
        self.closed = True


def test_health_initializes_bus(monkeypatch, capsys) -> None:
    fake_bus = FakeBus()
    monkeypatch.setattr(cli.AgentBus, "from_env", classmethod(lambda cls: fake_bus))

    exit_code = cli.main(["health"])

    assert exit_code == 0
    assert fake_bus.initialized_with == [False]
    assert fake_bus.closed is True
    out = capsys.readouterr().out.lower()
    # Health command defaults to --encoding compact (no spaces in JSON)
    assert '"ok":true' in out or '"ok": true' in out


def test_serve_rejects_non_localhost(monkeypatch) -> None:
    fake_bus = FakeBus()
    monkeypatch.setattr(cli.AgentBus, "from_env", classmethod(lambda cls: fake_bus))

    with pytest.raises(ValueError):
        cli.main(["serve", "--host", "127.0.0.1"])
