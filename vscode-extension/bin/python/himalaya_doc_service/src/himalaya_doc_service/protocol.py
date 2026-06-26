"""MCP JSON-RPC 2.0 protocol types used by the Himalaya doc service.

Compatible with Himalaya's McpServerManager (rust/crates/runtime/src/mcp_stdio.rs).
Uses Content-Length framing identical to LSP.
"""

from __future__ import annotations

import json
import sys
from typing import Any


# ---------------------------------------------------------------------------
# JSON-RPC 2.0 wire types
# ---------------------------------------------------------------------------

def _make_response(req_id: Any, result: Any) -> dict:
    return {"jsonrpc": "2.0", "id": req_id, "result": result}


def _make_error(req_id: Any, code: int, message: str) -> dict:
    return {"jsonrpc": "2.0", "id": req_id, "error": {"code": code, "message": message}}


# ---------------------------------------------------------------------------
# Content-Length framing (LSP-style)
# ---------------------------------------------------------------------------

def read_message(stdin=sys.stdin) -> dict | None:
    """Read one Content-Length-framed JSON message from *stdin*."""
    # Read headers until blank line
    content_length = None
    while True:
        line = stdin.readline()
        if not line:
            return None  # EOF
        line = line.rstrip("\r\n")
        if not line:
            break  # end of headers
        if line.lower().startswith("content-length:"):
            content_length = int(line.split(":", 1)[1].strip())
    if content_length is None:
        return None
    body = stdin.read(content_length)
    if not body:
        return None
    return json.loads(body)


def write_message(msg: dict, stdout=sys.stdout):
    """Write a JSON message with Content-Length framing to *stdout*."""
    body = json.dumps(msg, ensure_ascii=False, default=str)
    payload = body.encode("utf-8")
    header = f"Content-Length: {len(payload)}\r\n\r\n"
    stdout.buffer.write(header.encode("ascii") + payload)
    stdout.buffer.flush()


# ---------------------------------------------------------------------------
# Tool result helpers
# ---------------------------------------------------------------------------

def tool_result(text: str, is_error: bool = False) -> dict:
    """Build a tools/call result payload compatible with McpToolCallResult."""
    content = [{"type": "text", "text": text}]
    result = {"content": content}
    if is_error:
        result["isError"] = True
    return result
