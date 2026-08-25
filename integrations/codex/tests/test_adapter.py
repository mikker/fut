from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
ADAPTER = ROOT / "plugins/fut-codex/scripts/fut_codex_lifecycle.py"


class AdapterTests(unittest.TestCase):
    def invoke(
        self,
        mode: str,
        event: dict[str, object] | str,
        *,
        inside_fut: bool = True,
        fut_exit: int = 0,
    ) -> tuple[subprocess.CompletedProcess[str], list[str]]:
        with tempfile.TemporaryDirectory() as temporary:
            temporary_path = Path(temporary)
            log = temporary_path / "arguments.json"
            fake = temporary_path / "fut"
            fake.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "with open(os.environ['FUT_TEST_LOG'], 'w') as output:\n"
                "    json.dump(sys.argv[1:], output)\n"
                f"raise SystemExit({fut_exit})\n"
            )
            fake.chmod(0o755)
            environment = os.environ.copy()
            environment.update(FUT_BIN=str(fake), FUT_TEST_LOG=str(log))
            if inside_fut:
                environment.update(
                    FUT_TERMINAL_ID="00000000-0000-4000-8000-000000000001",
                    FUT_SOCKET="/tmp/fut-test.sock",
                )
            else:
                environment.pop("FUT_TERMINAL_ID", None)
                environment.pop("FUT_SOCKET", None)

            stdin = event if isinstance(event, str) else json.dumps(event)
            command = [sys.executable, str(ADAPTER), mode]
            result = subprocess.run(
                command,
                input=stdin,
                text=True,
                capture_output=True,
                env=environment,
                check=False,
            )
            arguments = json.loads(log.read_text()) if log.exists() else []
            return result, arguments

    def test_hook_events_map_to_lifecycle_reports_with_identity(self) -> None:
        cases = {
            "SessionStart": "idle",
            "UserPromptSubmit": "working",
            "PermissionRequest": "blocked",
            "PreToolUse": "working",
            "PostToolUse": "working",
        }
        for event_name, state in cases.items():
            with self.subTest(event=event_name):
                result, arguments = self.invoke(
                    "--hook",
                    {
                        "hook_event_name": event_name,
                        "session_id": "thread-1",
                        "turn_id": "turn-2",
                    },
                )
                self.assertEqual(result.returncode, 0)
                self.assertEqual(result.stdout, "")
                self.assertEqual(result.stderr, "")
                self.assertEqual(
                    arguments,
                    [
                        "agent",
                        "report",
                        state,
                        "--terminal-id",
                        "00000000-0000-4000-8000-000000000001",
                        "--source",
                        "codex",
                        "--agent-session-id",
                        "thread-1",
                        "--turn-id",
                        "turn-2",
                    ],
                )

    def test_outside_fut_and_malformed_events_are_inert(self) -> None:
        result, arguments = self.invoke(
            "--hook",
            {"hook_event_name": "UserPromptSubmit"},
            inside_fut=False,
        )
        self.assertEqual(result.returncode, 0)
        self.assertEqual(arguments, [])

        result, arguments = self.invoke("--hook", "not-json")
        self.assertEqual(result.returncode, 0)
        self.assertEqual(arguments, [])

    def test_failed_fut_report_never_fails_the_codex_hook(self) -> None:
        result, arguments = self.invoke(
            "--hook",
            {"hook_event_name": "SessionStart", "session_id": "thread-1"},
            fut_exit=9,
        )
        self.assertEqual(result.returncode, 0)
        self.assertTrue(arguments)


if __name__ == "__main__":
    unittest.main()
