#!/usr/bin/env python3

import json
import os
from pathlib import Path
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[1]
PLUGIN = ROOT / "plugin.json"
HOOKS = ROOT / "hooks.json"
REPORTER = ROOT / "scripts" / "report.sh"


def test_plugin_shape_and_mapping():
    manifest = json.loads(PLUGIN.read_text())
    assert manifest["name"] == "fut"

    config = json.loads(HOOKS.read_text())
    assert "fut-lifecycle" in config
    lifecycle = config["fut-lifecycle"]

    # PreInvocation
    pre_inv = lifecycle["PreInvocation"]
    assert len(pre_inv) == 1
    assert pre_inv[0]["type"] == "command"
    assert pre_inv[0]["command"] == "./scripts/report.sh working"
    assert pre_inv[0]["timeout"] == 2

    # PreToolUse
    pre_tool = lifecycle["PreToolUse"]
    assert len(pre_tool) == 1
    assert pre_tool[0]["matcher"] == "ask_question"
    assert pre_tool[0]["hooks"][0]["command"] == "./scripts/report.sh blocked"

    # PostToolUse
    post_tool = lifecycle["PostToolUse"]
    assert len(post_tool) == 1
    assert post_tool[0]["matcher"] == "ask_question"
    assert post_tool[0]["hooks"][0]["command"] == "./scripts/report.sh working"

    # Stop
    stop = lifecycle["Stop"]
    assert len(stop) == 1
    assert stop[0]["type"] == "command"
    assert stop[0]["command"] == "./scripts/report.sh completed"
    assert stop[0]["timeout"] == 2


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
            '{"conversationId":"agy-outside"}',
            base,
        )
        assert outside.returncode == 0
        assert json.loads(outside.stdout) == {"decision": "allow"}
        assert not log.exists()

        inside = base | {
            "FUT_SOCKET": "/tmp/fut.sock",
            "FUT_TERMINAL_ID": "11111111-1111-1111-1111-111111111111",
        }
        large_payload = "x" * 200_000
        reported = invoke(
            REPORTER,
            "blocked",
            json.dumps({"conversationId": "conv-12345", "extra": large_payload}),
            inside,
        )
        assert reported.returncode == 0
        assert json.loads(reported.stdout) == {"decision": "allow"}
        calls = [json.loads(line) for line in log.read_text().splitlines()]
        assert calls == [[
            "agent",
            "report",
            "blocked",
            "--source",
            "agy",
            "--agent-session-id",
            "conv-12345",
        ]]

        error_completed = invoke(
            REPORTER,
            "completed",
            json.dumps({"conversationId": "conv-error", "terminationReason": "ERROR", "error": "Post \"https...\" failed"}),
            inside,
        )
        assert error_completed.returncode == 0
        calls = [json.loads(line) for line in log.read_text().splitlines()]
        assert calls[-1] == [
            "agent",
            "report",
            "blocked",
            "--source",
            "agy",
            "--agent-session-id",
            "conv-error",
        ]

        rejected_identity = invoke(
            REPORTER,
            "working",
            '{"conversationId":"not/a/safe/id"}',
            inside,
        )
        assert rejected_identity.returncode == 0
        calls = [json.loads(line) for line in log.read_text().splitlines()]
        assert calls[-1] == [
            "agent",
            "report",
            "working",
            "--source",
            "agy",
        ]

        failing = invoke(
            REPORTER,
            "completed",
            '{"conversationId":"conv-failure"}',
            inside | {"FUT_TEST_EXIT": "42"},
        )
        assert failing.returncode == 0
        assert json.loads(failing.stdout) == {"decision": "allow"}

        before = log.read_text()
        invalid = invoke(
            REPORTER,
            "controlling-agy",
            '{"conversationId":"conv-invalid"}',
            inside,
        )
        assert invalid.returncode == 0
        assert log.read_text() == before


if __name__ == "__main__":
    test_plugin_shape_and_mapping()
    test_reporter_is_scoped_bounded_and_non_disruptive()
    print("Antigravity lifecycle adapter tests passed")
