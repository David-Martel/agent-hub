from __future__ import annotations

from datetime import datetime, timezone
from enum import Enum
from typing import Any
from uuid import UUID, uuid4

from pydantic import AliasChoices, BaseModel, ConfigDict, Field, field_validator, model_validator


BUS_PROTOCOL_VERSION = "1.0"


def utc_now() -> datetime:
    return datetime.now(timezone.utc)


class MessagePriority(str, Enum):
    LOW = "low"
    NORMAL = "normal"
    HIGH = "high"
    URGENT = "urgent"


class MessageDraft(BaseModel):
    model_config = ConfigDict(extra="forbid", populate_by_name=True)

    sender: str = Field(validation_alias=AliasChoices("sender", "from"), serialization_alias="from")
    recipient: str = Field(validation_alias=AliasChoices("recipient", "to"), serialization_alias="to")
    topic: str
    protocol_version: str = BUS_PROTOCOL_VERSION
    body: str
    thread_id: str | None = None
    tags: list[str] = Field(default_factory=list)
    priority: MessagePriority = MessagePriority.NORMAL
    request_ack: bool = False
    reply_to: str | None = None
    metadata: dict[str, Any] = Field(default_factory=dict)

    @field_validator("sender", "recipient", "topic", "body", "reply_to", "thread_id", mode="before")
    @classmethod
    def _strip_text(cls, value: str | None) -> str | None:
        if value is None:
            return None
        normalized = value.strip()
        if not normalized:
            raise ValueError("value must not be empty")
        return normalized

    @field_validator("tags", mode="before")
    @classmethod
    def _normalize_tags(cls, value: list[str] | None) -> list[str]:
        if not value:
            return []
        return [item.strip() for item in value if isinstance(item, str) and item.strip()]

    @field_validator("metadata", mode="before")
    @classmethod
    def _normalize_metadata(cls, value: dict[str, Any] | None) -> dict[str, Any]:
        return value or {}

    @model_validator(mode="after")
    def _default_reply_to(self) -> "MessageDraft":
        if self.reply_to is None:
            self.reply_to = self.sender
        return self


class MessageRecord(MessageDraft):
    id: UUID = Field(default_factory=uuid4)
    timestamp_utc: datetime = Field(default_factory=utc_now)
    stream_id: str | None = None


class PresenceAnnouncement(BaseModel):
    model_config = ConfigDict(extra="forbid")

    agent: str
    status: str = "online"
    protocol_version: str = BUS_PROTOCOL_VERSION
    timestamp_utc: datetime = Field(default_factory=utc_now)
    session_id: str = Field(default_factory=lambda: str(uuid4()))
    capabilities: list[str] = Field(default_factory=list)
    metadata: dict[str, Any] = Field(default_factory=dict)
    ttl_seconds: int = Field(default=180, ge=1, le=86400)

    @field_validator("agent", "status", "session_id", mode="before")
    @classmethod
    def _strip_required_text(cls, value: str) -> str:
        normalized = value.strip()
        if not normalized:
            raise ValueError("value must not be empty")
        return normalized

    @field_validator("capabilities", mode="before")
    @classmethod
    def _normalize_capabilities(cls, value: list[str] | None) -> list[str]:
        if not value:
            return []
        return [item.strip() for item in value if isinstance(item, str) and item.strip()]

    @field_validator("metadata", mode="before")
    @classmethod
    def _normalize_presence_metadata(cls, value: dict[str, Any] | None) -> dict[str, Any]:
        return value or {}


class MessageQuery(BaseModel):
    model_config = ConfigDict(extra="forbid")

    agent: str | None = None
    sender: str | None = None
    since_minutes: int = Field(default=1440, ge=1, le=10080)
    limit: int = Field(default=50, ge=1, le=500)
    include_broadcast: bool = True

    @field_validator("agent", "sender", mode="before")
    @classmethod
    def _strip_optional_text(cls, value: str | None) -> str | None:
        if value is None:
            return None
        normalized = value.strip()
        return normalized or None


class AckRequest(BaseModel):
    model_config = ConfigDict(extra="forbid")

    agent: str
    message_id: str
    body: str = "ack"

    @field_validator("agent", "message_id", "body", mode="before")
    @classmethod
    def _strip_ack_fields(cls, value: str) -> str:
        normalized = value.strip()
        if not normalized:
            raise ValueError("value must not be empty")
        return normalized


class BusHealth(BaseModel):
    ok: bool
    protocol_version: str = BUS_PROTOCOL_VERSION
    redis_url: str
    database_url: str | None
    database_ok: bool | None
    database_error: str | None = None
    runtime_metadata: dict[str, Any] = Field(default_factory=dict)
    storage_ready: bool
    stream_key: str
    channel_key: str
    presence_prefix: str
    service_agent_id: str
    startup_announced: bool
    startup_message_id: str | None = None
    startup_session_id: str
