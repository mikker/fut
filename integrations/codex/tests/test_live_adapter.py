from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest


ROOT = Path(__file__).resolve().parents[1]
ADAPTER = ROOT / "plugins/fut-codex/scripts/fut_codex_lifecycle.py"
FUT = os.environ.get("FUT_LIVE_BIN")


@unittest.skipUnless(FUT, "set FUT_LIVE_BIN to run against a built Fut binary")
class LiveAdapterTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        root = Path(self.temporary.name)
        self.socket = root / "fut.sock"
        self.cwd = root / "workspace"
        self.cwd.mkdir()
        self.environment = os.environ.copy()
        self.environment.update(
            HOME=str(root / "home"),
            FUT_RUNTIME_DIR=str(root / "runtime"),
            TERM="xterm-256color",
        )
        self.daemon = subprocess.Popen(
            [
                FUT,
                "--socket",
                str(self.socket),
                "daemon",
                "run",
                "--cwd",
                str(self.cwd),
                "--",
                "/bin/sh",
                "-c",
                "while IFS= read -r line; do :; done",
            ],
            env=self.environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            ping = subprocess.run(
                [FUT, "--socket", str(self.socket), "daemon", "ping"],
                env=self.environment,
                capture_output=True,
                check=False,
            )
            if ping.returncode == 0:
                break
            time.sleep(0.02)
        else:
            self.fail("Fut daemon did not become ready")

        resources = self.cli("list")["result"]
        self.terminal_id = resources["sessions"][0]["workspaces"][0]["tabs"][0][
            "panes"
        ][0]["terminal_id"]
        self.adapter_environment = self.environment.copy()
        self.adapter_environment.update(
            FUT_BIN=FUT,
            FUT_SOCKET=str(self.socket),
            FUT_TERMINAL_ID=self.terminal_id,
        )

    def tearDown(self) -> None:
        subprocess.run(
            [FUT, "--socket", str(self.socket), "daemon", "shutdown"],
            env=self.environment,
            capture_output=True,
            check=False,
        )
        try:
            self.daemon.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.daemon.kill()
            self.daemon.wait(timeout=3)
        self.temporary.cleanup()

    def cli(self, *arguments: str) -> dict[str, object]:
        result = subprocess.run(
            [FUT, "--socket", str(self.socket), "--json", *arguments],
            env=self.environment,
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout)

    def hook(self, event_name: str, *, turn_id: str | None = "turn-1") -> None:
        event = {
            "hook_event_name": event_name,
            "session_id": "codex-session-1",
        }
        if turn_id is not None:
            event["turn_id"] = turn_id
        result = subprocess.run(
            ["python3", str(ADAPTER), "--hook"],
            input=json.dumps(event),
            env=self.adapter_environment,
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0)
        self.assertEqual((result.stdout, result.stderr), ("", ""))

    def notify_complete(self, turn_id: str = "turn-1") -> None:
        event = json.dumps(
            {
                "type": "agent-turn-complete",
                "thread-id": "codex-session-1",
                "turn-id": turn_id,
            }
        )
        result = subprocess.run(
            ["python3", str(ADAPTER), "--notify", event],
            env=self.adapter_environment,
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0)

    def agent(self) -> dict[str, object]:
        return self.cli("agent", "get", self.terminal_id)["result"]["agent"]

    def test_real_report_get_and_wait_surface_observes_native_transitions(self) -> None:
        self.hook("SessionStart", turn_id=None)
        agent = self.agent()
        self.assertEqual(agent["activity"]["state"], "idle")
        self.assertEqual(agent["activity"]["integration"]["source"], "codex")
        self.assertEqual(
            agent["activity"]["integration"]["agent_session_id"],
            "codex-session-1",
        )

        self.hook("UserPromptSubmit")
        agent = self.agent()
        self.assertEqual(agent["activity"]["state"], "working")
        self.assertEqual(agent["activity"]["last_event"]["turn_id"], "turn-1")
        self.assertFalse(agent["available"])

        waiter = subprocess.Popen(
            [
                FUT,
                "--socket",
                str(self.socket),
                "--json",
                "agent",
                "wait",
                self.terminal_id,
                "--timeout",
                "3s",
            ],
            env=self.environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.notify_complete()
        stdout, stderr = waiter.communicate(timeout=4)
        self.assertEqual(waiter.returncode, 0, stderr)
        settled = json.loads(stdout)["result"]["activity"]
        self.assertEqual(settled["state"], "idle")
        self.assertEqual(settled["last_event"]["kind"], "completed")
        self.assertEqual(settled["last_event"]["turn_id"], "turn-1")

        prompted = subprocess.Popen(
            [
                FUT,
                "--socket",
                str(self.socket),
                "--json",
                "agent",
                "prompt",
                self.terminal_id,
                "CODEX_LIVE_PROMPT",
                "--wait",
                "--timeout",
                "3s",
            ],
            env=self.environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.cli(
            "terminal",
            "wait-output",
            self.terminal_id,
            "--literal",
            "CODEX_LIVE_PROMPT",
            "--timeout",
            "2s",
        )
        self.hook("UserPromptSubmit", turn_id="turn-prompt")
        self.notify_complete(turn_id="turn-prompt")
        stdout, stderr = prompted.communicate(timeout=4)
        self.assertEqual(prompted.returncode, 0, stderr)
        prompt_result = json.loads(stdout)["result"]
        self.assertTrue(prompt_result["submitted"])
        self.assertIsNotNone(prompt_result["working_revision"])
        self.assertEqual(
            prompt_result["activity"]["last_event"]["turn_id"], "turn-prompt"
        )

        self.hook("UserPromptSubmit", turn_id="turn-2")
        self.hook("PermissionRequest", turn_id="turn-2")
        blocked = self.cli(
            "agent", "wait", self.terminal_id, "--timeout", "1s"
        )["result"]["activity"]
        self.assertEqual(blocked["state"], "blocked")
        self.assertEqual(blocked["last_event"]["turn_id"], "turn-2")


if __name__ == "__main__":
    unittest.main()
