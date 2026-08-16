#!/usr/bin/env python3

import json
import os
from pathlib import Path
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[1]
HOOKS = ROOT / "hooks" / "hooks.json"
REPORTER = ROOT / "scripts" / "report.sh"


def handlers(config, event):
    for group in config["hooks"][event]:
        for handler in group["hooks"]:
            yield group.get("matcher"), handler


def test_plugin_shape_and_mapping():
    manifest = json.loads((ROOT / ".claude-plugin" / "plugin.json").read_text())
    assert manifest["name"] == "fut"
    config = json.loads(HOOKS.read_text())
    expected = {
        "SessionStart": [("startup|resume|clear|fork", "idle")],
        "UserPromptSubmit": [(None, "working")],
        "PostToolUse": [(None, "working")],
        "PostToolUseFailure": [(None, "working")],
        "PermissionDenied": [(None, "working")],
        "ElicitationResult": [(None, "working")],
        "Notification": [
            ("permission_prompt|elicitation_dialog|agent_needs_input", "blocked"),
            ("idle_prompt", "completed"),
        ],
        "Stop": [(None, "completed")],
        "StopFailure": [(None, "blocked")],
        "SessionEnd": [(None, "exited")],
    }
    actual = {}
    for event in config["hooks"]:
        actual[event] = []
        for matcher, handler in handlers(config, event):
            assert handler["type"] == "command"
            assert handler["command"] == "${CLAUDE_PLUGIN_ROOT}/scripts/report.sh"
            assert handler["timeout"] == 2
            assert "async" not in handler
            assert len(handler["args"]) == 1
            actual[event].append((matcher, handler["args"][0]))
    assert actual == expected


def invoke(reporter, state, payload, env):
    return subprocess.run(
        [str(reporter), state],
        input=payload,
        text=True,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=3,
        check=False,
    )


def test_reporter_is_scoped_bounded_and_non_disruptive():
    with tempfile.TemporaryDirectory() as directory:
        temporary = Path(directory)
        log = temporary / "calls.jsonl"
        mock = temporary / "fut"
        mock.write_text(
            "#!/bin/sh\n"
            "python3 -c 'import json, os, sys; "
            "open(os.environ[\"FUT_TEST_LOG\"], \"a\").write(json.dumps(sys.argv[1:]) + \"\\n\")' \"$@\"\n"
            "exit \"${FUT_TEST_EXIT:-0}\"\n"
        )
        mock.chmod(0o755)
        base = os.environ.copy()
        base.pop("FUT_SOCKET", None)
        base.pop("FUT_TERMINAL_ID", None)
        base.update(
            PATH=f"{temporary}:{base['PATH']}",
            FUT_TEST_LOG=str(log),
        )

        outside = invoke(
            REPORTER,
            "working",
            '{"session_id":"claude-outside"}',
            base,
        )
        assert outside.returncode == 0
        assert outside.stdout == outside.stderr == ""
        assert not log.exists()

        inside = base | {
            "FUT_SOCKET": "/tmp/fut.sock",
            "FUT_TERMINAL_ID": "11111111-1111-1111-1111-111111111111",
        }
        large_prompt = "x" * 200_000
        reported = invoke(
            REPORTER,
            "blocked",
            json.dumps({"session_id": "session-abc", "prompt": large_prompt}),
            inside,
        )
        assert reported.returncode == 0
        assert reported.stdout == reported.stderr == ""
        calls = [json.loads(line) for line in log.read_text().splitlines()]
        assert calls == [[
            "agent",
            "report",
            "blocked",
            "--terminal-id",
            inside["FUT_TERMINAL_ID"],
            "--source",
            "claude-code",
            "--agent-session-id",
            "session-abc",
        ]]

        rejected_identity = invoke(
            REPORTER,
            "working",
            '{"session_id":"not/a/safe/id"}',
            inside,
        )
        assert rejected_identity.returncode == 0
        calls = [json.loads(line) for line in log.read_text().splitlines()]
        assert calls[-1] == [
            "agent",
            "report",
            "working",
            "--terminal-id",
            inside["FUT_TERMINAL_ID"],
            "--source",
            "claude-code",
        ]

        failing = invoke(
            REPORTER,
            "completed",
            '{"session_id":"session-failure"}',
            inside | {"FUT_TEST_EXIT": "42"},
        )
        assert failing.returncode == 0
        assert failing.stdout == failing.stderr == ""

        before = log.read_text()
        invalid = invoke(
            REPORTER,
            "controlling-claude",
            '{"session_id":"session-invalid"}',
            inside,
        )
        assert invalid.returncode == 0
        assert log.read_text() == before


if __name__ == "__main__":
    test_plugin_shape_and_mapping()
    test_reporter_is_scoped_bounded_and_non_disruptive()
    print("Claude Code lifecycle adapter tests passed")
