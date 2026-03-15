from __future__ import annotations

import argparse
import json
import sys
from typing import Any

from .bus import AgentBus
from .codec import compact_context, dumps_compact, dumps_pretty, minimize_message
from .server import create_mcp
from .settings import AgentBusSettings, ServerOptions

ENCODING_CHOICES = ["json", "compact", "human", "minimal"]


def _json_dump(data: Any, *, encoding: str = "json") -> None:
    if encoding == "json":
        print(dumps_pretty(data))
    elif encoding == "compact":
        print(dumps_compact(data))
    elif encoding == "minimal":
        if isinstance(data, list):
            minimized = [minimize_message(m) if isinstance(m, dict) else m for m in data]
            print(dumps_compact(minimized))
        elif isinstance(data, dict):
            print(dumps_compact(minimize_message(data)))
        else:
            print(dumps_compact(data))
    elif encoding == "human":
        if isinstance(data, (dict, list)):
            print(dumps_pretty(data))
        else:
            print(data)
    else:
        raise SystemExit(f"unsupported encoding: {encoding}")


def _build_parser() -> argparse.ArgumentParser:
    settings = AgentBusSettings()
    parser = argparse.ArgumentParser(prog="agent-bus-mcp")
    subparsers = parser.add_subparsers(dest="command", required=True)

    serve = subparsers.add_parser("serve", help="Run the MCP server")
    serve.add_argument("--transport", choices=["stdio", "streamable-http", "sse"], default="stdio")
    serve.add_argument("--host", default=settings.server_host)
    serve.add_argument("--port", type=int, default=settings.server_port)
    serve.add_argument("--streamable-http-path", default=settings.streamable_http_path)

    send = subparsers.add_parser("send", help="Post a message to the bus")
    send.add_argument("--from-agent", required=True)
    send.add_argument("--to-agent", required=True)
    send.add_argument("--topic", required=True)
    send.add_argument("--body", required=True)
    send.add_argument("--encoding", choices=ENCODING_CHOICES, default="compact")
    send.add_argument("--thread-id", default=None)
    send.add_argument("--priority", choices=["low", "normal", "high", "urgent"], default="normal")
    send.add_argument("--tag", action="append", dest="tags")
    send.add_argument("--request-ack", action="store_true")
    send.add_argument("--reply-to")
    send.add_argument("--metadata", default="{}")

    read = subparsers.add_parser("read", help="Read recent messages")
    read.add_argument("--agent")
    read.add_argument("--from-agent")
    read.add_argument("--since-minutes", type=int, default=1440)
    read.add_argument("--limit", type=int, default=50)
    read.add_argument(
        "--encoding",
        choices=ENCODING_CHOICES,
        default="compact",
        help="Output format: json (pretty), compact (no spaces), or human table (default script path)",
    )
    read.add_argument("--exclude-broadcast", action="store_true")

    watch = subparsers.add_parser("watch", help="Watch live bus traffic for an agent")
    watch.add_argument("--agent", required=True)
    watch.add_argument("--history", type=int, default=0)
    watch.add_argument("--exclude-broadcast", action="store_true")
    watch.add_argument(
        "--encoding",
        choices=ENCODING_CHOICES,
        default="compact",
        help="Output format: json (pretty), compact (no spaces), or human table",
    )
    watch.add_argument("--json", action="store_true", help="Deprecated compatibility alias for compact output.")

    ack = subparsers.add_parser("ack", help="Send an acknowledgement")
    ack.add_argument("--agent", required=True)
    ack.add_argument("--message-id", required=True)
    ack.add_argument("--body", default="ack")
    ack.add_argument("--encoding", choices=ENCODING_CHOICES, default="compact")

    presence = subparsers.add_parser("presence", help="Set agent presence")
    presence.add_argument("--agent", required=True)
    presence.add_argument("--status", default="online")
    presence.add_argument("--session-id")
    presence.add_argument("--capability", action="append", dest="capabilities")
    presence.add_argument("--ttl-seconds", type=int, default=180)
    presence.add_argument("--metadata", default="{}")
    presence.add_argument("--encoding", choices=ENCODING_CHOICES, default="compact")

    presence_list = subparsers.add_parser("presence-list", help="List active agents")
    presence_list.add_argument("--encoding", choices=ENCODING_CHOICES, default="compact")

    health = subparsers.add_parser("health", help="Check Redis bus health")
    health.add_argument("--encoding", choices=ENCODING_CHOICES, default="compact")
    return parser


def _load_json(text: str) -> dict[str, Any]:
    try:
        value = json.loads(text)
    except json.JSONDecodeError as exc:
        raise SystemExit(f"invalid JSON: {exc}") from exc
    if not isinstance(value, dict):
        raise SystemExit("metadata must decode to a JSON object")
    return value


def main(argv: list[str] | None = None) -> int:
    parser = _build_parser()
    args = parser.parse_args(argv)
    bus = AgentBus.from_env()

    try:
        if args.command == "serve":
            options = ServerOptions(host=args.host, port=args.port, streamable_http_path=args.streamable_http_path)
            mcp = create_mcp(
                bus,
                host=options.host,
                port=options.port,
                streamable_http_path=options.streamable_http_path,
            )
            mcp.run(args.transport)
            return 0

        bus.initialize(announce_startup=False)

        if args.command == "send":
            message = bus.post_message(
                sender=args.from_agent,
                recipient=args.to_agent,
                topic=args.topic,
                body=args.body,
                thread_id=args.thread_id,
                tags=args.tags,
                priority=args.priority,
                request_ack=args.request_ack,
                reply_to=args.reply_to,
                metadata=_load_json(args.metadata),
            )
            _json_dump(message, encoding=args.encoding)
            return 0

        if args.command == "read":
            payload = bus.list_messages(
                agent=args.agent,
                sender=args.from_agent,
                since_minutes=args.since_minutes,
                limit=args.limit,
                include_broadcast=not args.exclude_broadcast,
            )
            if args.encoding == "human":
                for message in payload:
                    print(
                        f"[{message['timestamp_utc']}] {message['from']} -> {message['to']} | "
                        f"{message['topic']} | {message['priority']} | {message['body']}"
                    )
            else:
                _json_dump(payload, encoding=args.encoding)
            return 0

        if args.command == "watch":
            encoding = "compact" if args.json else args.encoding
            include_broadcast = not args.exclude_broadcast
            for event in bus.watch(agent=args.agent, history=args.history, include_broadcast=include_broadcast):
                if encoding == "human":
                    if event.get("event") == "message":
                        message = event["message"]
                        print(
                            f"[{message['timestamp_utc']}] {message['from']} -> {message['to']} | "
                            f"{message['topic']} | {message['priority']} | {message['body']}"
                        )
                    elif event.get("event") == "presence":
                        presence = event["presence"]
                        print(
                            f"[{presence['timestamp_utc']}] presence {presence['agent']}={presence['status']} "
                            f"session={presence['session_id']}"
                        )
                    else:
                        print(event)
                else:
                    if encoding == "json":
                        print(dumps_pretty(event), flush=True)
                    else:
                        print(dumps_compact(event), flush=True)
            return 0

        if args.command == "ack":
            _json_dump(bus.ack_message(agent=args.agent, message_id=args.message_id, body=args.body), encoding=args.encoding)
            return 0

        if args.command == "presence":
            _json_dump(
                bus.set_presence(
                    agent=args.agent,
                    status=args.status,
                    session_id=args.session_id,
                    capabilities=args.capabilities,
                    ttl_seconds=args.ttl_seconds,
                    metadata=_load_json(args.metadata),
                ),
                encoding=args.encoding,
            )
            return 0

        if args.command == "presence-list":
            _json_dump(bus.list_presence(), encoding=args.encoding)
            return 0

        if args.command == "health":
            _json_dump(bus.health(), encoding=args.encoding)
            return 0

        parser.print_help(sys.stderr)
        return 1
    finally:
        bus.close()


if __name__ == "__main__":
    raise SystemExit(main())
