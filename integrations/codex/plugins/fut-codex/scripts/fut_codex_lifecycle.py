#!/usr/bin/env python3
"""Best-effort bridge from supported Codex events to `fut agent report`."""

from __future__ import annotations

import json
import os
import subprocess
import sys
from typing import Any

MAX_METADATA_BYTES = 256
HOOK_STATES = {
    "SessionStart": "idle",
    "UserPromptSubmit": "working",
    "PermissionRequest": "blocked",
    "PreToolUse": "working",
    "PostToolUse": "working",
}


def bounded_string(value: Any) -> str | None:
    if not isinstance(value, str) or not value:
        return None
    if len(value.encode("utf-8")) > MAX_METADATA_BYTES:
        return None
    return value


def load_event() -> dict[str, Any] | None:
    try:
        value = json.load(sys.stdin)
    except (json.JSONDecodeError, OSError, UnicodeError):
        return None
    return value if isinstance(value, dict) else None


def transition(event: dict[str, Any]) -> tuple[str, str | None, str | None] | None:
    state = HOOK_STATES.get(event.get("hook_event_name"))
    if state is None:
        return None
    return (
        state,
        bounded_string(event.get("session_id")),
        bounded_string(event.get("turn_id")),
    )


def report(state: str, session_id: str | None, turn_id: str | None) -> None:
    terminal_id = bounded_string(os.environ.get("FUT_TERMINAL_ID"))
    if terminal_id is None or not os.environ.get("FUT_SOCKET"):
        return

    command = [
        os.environ.get("FUT_BIN", "fut"),
        "agent",
        "report",
        state,
        "--terminal-id",
        terminal_id,
        "--source",
        "codex",
    ]
    if session_id is not None:
        command.extend(["--agent-session-id", session_id])
    if turn_id is not None:
        command.extend(["--turn-id", turn_id])
    try:
        subprocess.run(
            command,
            check=False,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=2,
        )
    except (OSError, subprocess.SubprocessError):
        pass


def main(arguments: list[str]) -> int:
    if arguments != ["--hook"]:
        return 0
    event = load_event()
    if event is None:
        return 0
    mapped = transition(event)
    if mapped is not None:
        report(*mapped)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
