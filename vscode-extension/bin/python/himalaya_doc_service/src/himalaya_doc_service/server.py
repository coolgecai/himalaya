"""MCP JSON-RPC 2.0 server — main loop and tool routing.

Compatible with Himalaya's McpServerManager (rust/crates/runtime/src/mcp_stdio.rs).
Reads JSON-RPC requests from stdin, writes responses to stdout.
"""

from __future__ import annotations

import json
import os
import traceback
from typing import Any

from .protocol import read_message, write_message, tool_result


def _ensure_valid_cwd() -> None:
    try:
        os.getcwd()
    except FileNotFoundError:
        os.chdir("/")


_ensure_valid_cwd()

from .tools import TOOL_REGISTRY, TOOL_SCHEMAS


class McpServer:
    """MCP server that exposes document generation tools over stdio."""

    SERVER_INFO = {
        "name": "himalaya-doc-service",
        "version": "0.1.0",
    }

    PROTOCOL_VERSION = "2025-03-26"

    def __init__(self):
        self._tools = TOOL_REGISTRY

    # ------------------------------------------------------------------
    # Main loop
    # ------------------------------------------------------------------

    def run(self):
        """Read JSON-RPC messages from stdin in a loop until EOF."""
        while True:
            try:
                request = read_message()
                if request is None:
                    break
                response = self._dispatch(request)
                if response is not None:
                    write_message(response)
            except Exception:
                # Best-effort error reporting without crashing the server
                tb = traceback.format_exc()
                write_message({
                    "jsonrpc": "2.0",
                    "id": None,
                    "error": {"code": -32603, "message": f"Internal error: {tb[-500:]}"},
                })

    # ------------------------------------------------------------------
    # Dispatch
    # ------------------------------------------------------------------

    def _dispatch(self, request: dict) -> dict | None:
        method = request.get("method", "")
        req_id = request.get("id")

        if method == "initialize":
            return self._handle_initialize(req_id, request.get("params", {}))
        if method == "tools/list":
            return self._handle_list_tools(req_id, request.get("params", {}))
        if method == "tools/call":
            return self._handle_call_tool(req_id, request.get("params", {}))
        if method == "notifications/initialized":
            return None  # No response for notifications
        # Unknown method
        return self._error(req_id, -32601, f"Method not found: {method}")

    # ------------------------------------------------------------------
    # Handlers
    # ------------------------------------------------------------------

    def _handle_initialize(self, req_id: Any, params: dict) -> dict:
        return {
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "protocolVersion": self.PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": self.SERVER_INFO,
            },
        }

    def _handle_list_tools(self, req_id: Any, params: dict) -> dict:
        tools = [
            {"name": name, "description": schema["description"], "inputSchema": schema["inputSchema"]}
            for name, schema in TOOL_SCHEMAS.items()
        ]
        return {
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {"tools": tools},
        }

    def _handle_call_tool(self, req_id: Any, params: dict | None) -> dict:
        if not params:
            return self._error(req_id, -32602, "Missing params")

        tool_name = params.get("name", "")
        arguments = params.get("arguments", {})
        if isinstance(arguments, str):
            try:
                arguments = json.loads(arguments)
            except json.JSONDecodeError:
                arguments = {}

        handler = self._tools.get(tool_name)
        if handler is None:
            return {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": tool_result(
                    json.dumps({"error": f"Unknown tool: {tool_name}"}, ensure_ascii=False),
                    is_error=True,
                ),
            }

        try:
            result = handler(arguments)
            return {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": tool_result(json.dumps(result, ensure_ascii=False, default=str)),
            }
        except ValueError as exc:
            return {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": tool_result(json.dumps({"error": str(exc)}, ensure_ascii=False), is_error=True),
            }
        except Exception:
            tb = traceback.format_exc()
            return {
                "jsonrpc": "2.0",
                "id": req_id,
                "result": tool_result(
                    json.dumps({"error": f"Internal error: {tb[-500:]}"}, ensure_ascii=False),
                    is_error=True,
                ),
            }

    # ------------------------------------------------------------------
    # Helpers
    # ------------------------------------------------------------------

    def _error(self, req_id: Any, code: int, message: str) -> dict:
        return {
            "jsonrpc": "2.0",
            "id": req_id,
            "error": {"code": code, "message": message},
        }
