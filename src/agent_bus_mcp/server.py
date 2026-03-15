from __future__ import annotations

from contextlib import asynccontextmanager
import json

from mcp.server.fastmcp import FastMCP

from .bus import AgentBus


def create_mcp(bus: AgentBus | None = None, *, host: str = "localhost", port: int = 8765, streamable_http_path: str = "/mcp") -> FastMCP:
    agent_bus = bus or AgentBus.from_env()

    @asynccontextmanager
    async def lifespan(_: FastMCP):
        agent_bus.initialize(announce_startup=True)
        try:
            yield
        finally:
            agent_bus.close()

    mcp = FastMCP(
        name="agent-bus",
        instructions=(
            "Redis-backed coordination bus for local agents. Use post_message for handoffs, "
            "list_messages to inspect inbox history, set_presence to advertise availability, and "
            "list_presence to discover other active agents."
        ),
        host=host,
        port=port,
        streamable_http_path=streamable_http_path,
        lifespan=lifespan,
    )

    @mcp.tool()
    def bus_health() -> dict:
        return agent_bus.health()

    @mcp.tool()
    def post_message(
        sender: str,
        recipient: str,
        topic: str,
        body: str,
        tags: list[str] | None = None,
        thread_id: str | None = None,
        priority: str = "normal",
        request_ack: bool = False,
        reply_to: str | None = None,
        metadata: dict | None = None,
    ) -> dict:
        return agent_bus.post_message(
            sender=sender,
            recipient=recipient,
            topic=topic,
            body=body,
            tags=tags,
            thread_id=thread_id,
            priority=priority,
            request_ack=request_ack,
            reply_to=reply_to,
            metadata=metadata,
        )

    @mcp.tool()
    def list_messages(
        agent: str | None = None,
        sender: str | None = None,
        since_minutes: int = 1440,
        limit: int = 50,
        include_broadcast: bool = True,
    ) -> list[dict]:
        return agent_bus.list_messages(
            agent=agent,
            sender=sender,
            since_minutes=since_minutes,
            limit=limit,
            include_broadcast=include_broadcast,
        )

    @mcp.tool()
    def ack_message(agent: str, message_id: str, body: str = "ack") -> dict:
        return agent_bus.ack_message(agent=agent, message_id=message_id, body=body)

    @mcp.tool()
    def set_presence(
        agent: str,
        status: str = "online",
        session_id: str | None = None,
        capabilities: list[str] | None = None,
        ttl_seconds: int = 180,
        metadata: dict | None = None,
    ) -> dict:
        return agent_bus.set_presence(
            agent=agent,
            status=status,
            session_id=session_id,
            capabilities=capabilities,
            ttl_seconds=ttl_seconds,
            metadata=metadata,
        )

    @mcp.tool()
    def list_presence() -> list[dict]:
        return agent_bus.list_presence()

    @mcp.resource("agentbus://presence")
    def presence_resource() -> str:
        return json.dumps(agent_bus.list_presence(), indent=2)

    return mcp
