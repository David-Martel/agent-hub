from __future__ import annotations

from urllib.parse import quote, urlsplit, urlunsplit

from pydantic import AliasChoices, BaseModel, Field, field_validator, model_validator
from pydantic_settings import BaseSettings, SettingsConfigDict


DEFAULT_REDIS_URL = "redis://localhost:6380/0"
DEFAULT_DATABASE_URL = "postgresql://postgres:password@localhost:5300/postgres"
LOCALHOST_HOSTS = {"localhost", "127.0.0.1", "0.0.0.0", "::1", "[::1]"}


def _normalize_local_url(value: str, *, field_name: str, allowed_schemes: set[str]) -> str:
    parsed = urlsplit(value)
    if parsed.scheme not in allowed_schemes:
        raise ValueError(f"{field_name} must use one of {sorted(allowed_schemes)}")
    if not parsed.hostname:
        raise ValueError(f"{field_name} must include a hostname")

    hostname = parsed.hostname.lower()
    if hostname in LOCALHOST_HOSTS:
        hostname = "localhost"
    if hostname != "localhost":
        raise ValueError(f"{field_name} must use localhost, not {parsed.hostname!r}")

    auth = ""
    if parsed.username:
        auth = quote(parsed.username, safe="")
        if parsed.password is not None:
            auth += f":{quote(parsed.password, safe='')}"
        auth += "@"

    netloc = f"{auth}{hostname}"
    if parsed.port is not None:
        netloc += f":{parsed.port}"
    return urlunsplit((parsed.scheme, netloc, parsed.path, parsed.query, parsed.fragment))


class AgentBusSettings(BaseSettings):
    model_config = SettingsConfigDict(
        env_prefix="AGENT_BUS_",
        case_sensitive=False,
        extra="ignore",
        validate_default=True,
    )

    redis_url: str = Field(
        default=DEFAULT_REDIS_URL,
        validation_alias=AliasChoices("AGENT_BUS_REDIS_URL", "REDIS_URL"),
    )
    database_url: str | None = Field(default=DEFAULT_DATABASE_URL, validation_alias="AGENT_BUS_DATABASE_URL")
    stream_key: str = Field(default="agent_bus:messages", validation_alias="AGENT_BUS_STREAM_KEY")
    channel_key: str = Field(default="agent_bus:events", validation_alias="AGENT_BUS_CHANNEL")
    presence_prefix: str = Field(default="agent_bus:presence:", validation_alias="AGENT_BUS_PRESENCE_PREFIX")
    message_table: str = Field(default="agent_bus.messages", validation_alias="AGENT_BUS_MESSAGE_TABLE")
    presence_event_table: str = Field(
        default="agent_bus.presence_events",
        validation_alias="AGENT_BUS_PRESENCE_EVENT_TABLE",
    )
    stream_maxlen: int = Field(default=5000, ge=100, le=100000, validation_alias="AGENT_BUS_STREAM_MAXLEN")
    redis_socket_timeout_seconds: float = Field(
        default=2.0,
        gt=0,
        le=30,
        validation_alias="AGENT_BUS_REDIS_SOCKET_TIMEOUT_SECONDS",
    )
    redis_socket_connect_timeout_seconds: float = Field(
        default=2.0,
        gt=0,
        le=30,
        validation_alias="AGENT_BUS_REDIS_SOCKET_CONNECT_TIMEOUT_SECONDS",
    )
    postgres_min_size: int = Field(default=0, ge=0, le=32, validation_alias="AGENT_BUS_POSTGRES_MIN_SIZE")
    postgres_max_size: int = Field(default=2, ge=1, le=32, validation_alias="AGENT_BUS_POSTGRES_MAX_SIZE")
    postgres_connect_timeout_seconds: float = Field(
        default=3.0,
        gt=0,
        le=30,
        validation_alias="AGENT_BUS_POSTGRES_CONNECT_TIMEOUT_SECONDS",
    )
    postgres_max_idle_seconds: float = Field(
        default=60.0,
        gt=0,
        le=3600,
        validation_alias="AGENT_BUS_POSTGRES_MAX_IDLE_SECONDS",
    )
    postgres_max_lifetime_seconds: float = Field(
        default=300.0,
        gt=0,
        le=86400,
        validation_alias="AGENT_BUS_POSTGRES_MAX_LIFETIME_SECONDS",
    )
    postgres_retry_cooldown_seconds: float = Field(
        default=30.0,
        ge=0,
        le=3600,
        validation_alias="AGENT_BUS_POSTGRES_RETRY_COOLDOWN_SECONDS",
    )
    service_agent_id: str = Field(default="agent-bus", validation_alias="AGENT_BUS_SERVICE_AGENT_ID")
    startup_enabled: bool = Field(default=True, validation_alias="AGENT_BUS_STARTUP_ENABLED")
    startup_recipient: str = Field(default="all", validation_alias="AGENT_BUS_STARTUP_RECIPIENT")
    startup_topic: str = Field(default="status", validation_alias="AGENT_BUS_STARTUP_TOPIC")
    startup_body: str = Field(
        default="agent-bus is up and running",
        validation_alias="AGENT_BUS_STARTUP_BODY",
    )
    startup_status: str = Field(default="online", validation_alias="AGENT_BUS_STARTUP_STATUS")
    server_host: str = Field(default="localhost", validation_alias="AGENT_BUS_SERVER_HOST")
    server_port: int = Field(default=8765, ge=1, le=65535, validation_alias="AGENT_BUS_SERVER_PORT")
    streamable_http_path: str = Field(default="/mcp", validation_alias="AGENT_BUS_STREAMABLE_HTTP_PATH")

    @field_validator("redis_url")
    @classmethod
    def _normalize_redis_url(cls, value: str) -> str:
        return _normalize_local_url(value, field_name="redis_url", allowed_schemes={"redis", "rediss"})

    @field_validator("database_url")
    @classmethod
    def _normalize_database_url(cls, value: str | None) -> str | None:
        if not value:
            return None
        return _normalize_local_url(
            value,
            field_name="database_url",
            allowed_schemes={"postgresql", "postgres", "psql"},
        )

    @field_validator("server_host")
    @classmethod
    def _normalize_server_host(cls, value: str) -> str:
        normalized = value.strip().lower()
        if normalized == "localhost":
            return "localhost"
        raise ValueError("server_host must be localhost")

    @field_validator(
        "stream_key",
        "channel_key",
        "presence_prefix",
        "message_table",
        "presence_event_table",
        "service_agent_id",
        "startup_recipient",
        "startup_topic",
        "startup_body",
        "startup_status",
        "streamable_http_path",
    )
    @classmethod
    def _strip_non_empty(cls, value: str) -> str:
        normalized = value.strip()
        if not normalized:
            raise ValueError("value must not be empty")
        return normalized

    @model_validator(mode="after")
    def _validate_pool_sizes(self) -> "AgentBusSettings":
        if self.postgres_max_size < self.postgres_min_size:
            raise ValueError("postgres_max_size must be greater than or equal to postgres_min_size")
        return self


class ServerOptions(BaseModel):
    host: str = "localhost"
    port: int = Field(default=8765, ge=1, le=65535)
    streamable_http_path: str = "/mcp"

    @field_validator("host")
    @classmethod
    def _normalize_host(cls, value: str) -> str:
        normalized = value.strip().lower()
        if normalized == "localhost":
            return "localhost"
        raise ValueError("host must be localhost")

    @field_validator("streamable_http_path")
    @classmethod
    def _validate_streamable_http_path(cls, value: str) -> str:
        normalized = value.strip()
        if not normalized.startswith("/"):
            raise ValueError("streamable_http_path must start with '/'")
        return normalized
