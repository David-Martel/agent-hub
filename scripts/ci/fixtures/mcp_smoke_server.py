import json
import os
import sys

case = os.environ["MCP_SMOKE_CASE"]


def send(value):
    print(json.dumps(value), flush=True)


for line in sys.stdin:
    request = json.loads(line)
    if request.get("method") == "initialize":
        if case == "notification":
            send(
                {
                    "jsonrpc": "2.0",
                    "method": "notifications/message",
                    "params": {"message": "synthetic notification"},
                }
            )
        if case == "invalid_json":
            print("not-json", flush=True)
            continue
        if case == "initialize_error":
            send(
                {
                    "jsonrpc": "2.0",
                    "id": 1,
                    "error": {"code": -32000, "message": "synthetic initialize error"},
                }
            )
            continue
        result = {
            "protocolVersion": "2024-11-05",
            "serverInfo": {"name": "agent-bus", "version": "fixture"},
        }
        if case == "missing_server_info":
            result.pop("serverInfo")
        if case == "missing_server_version":
            result["serverInfo"].pop("version")
        if case == "null_result":
            result = None
        response = {"jsonrpc": "2.0", "id": 1, "result": result}
        if case == "null_error":
            response["error"] = None
        send(response)
    elif request.get("method") == "tools/list":
        if case == "tools_error":
            send(
                {
                    "jsonrpc": "2.0",
                    "id": 2,
                    "error": {"code": -32000, "message": "synthetic tools error"},
                }
            )
            continue
        tools = [
            {"name": f"fixture_tool_{i}", "inputSchema": {"type": "object"}}
            for i in range(17)
        ]
        if case == "missing_tool_name":
            tools[0].pop("name")
        result = {} if case == "missing_tools" else {"tools": tools}
        send({"jsonrpc": "2.0", "id": 2, "result": result})
