from __future__ import annotations

import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import time
import unittest
import uuid

from terminal_process import run_in_terminal


ROOT = Path(__file__).resolve().parents[1]
ADAPTER = ROOT / "plugins/fut-codex/scripts/fut_codex_lifecycle.py"
PROCESS_FIXTURE = Path(__file__).with_name("terminal_process.py")
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
            FUT_BIN=FUT,
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
                'while IFS= read -r line; do eval "$line"; done',
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

    def run_report(self, command, *, input=None, attached=True, terminal_id=None):
        if not attached:
            return subprocess.run(
                command, input=input, env=self.adapter_environment,
                text=True, capture_output=True, check=False, start_new_session=True,
            )
        return self.run_in_pane(command, input=input, terminal_id=terminal_id)

    def run_in_pane(self, command, *, input=None, terminal_id=None, orphan=False):
        # Keep process behavior in the fixture: piped stdio, a separate process
        # session, and optionally reparenting outside the terminal's tree.
        result_path = Path(self.temporary.name) / f"{uuid.uuid4()}.json"
        fixture = [sys.executable, str(PROCESS_FIXTURE)]
        if orphan:
            fixture.append("--orphan")
        fixture.extend([str(result_path), json.dumps(input), json.dumps(command)])
        self.cli(
            "terminal", "run", terminal_id or self.terminal_id, shlex.join(fixture)
        )
        deadline = time.monotonic() + 5
        while not result_path.exists() and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertTrue(result_path.exists(), "report fixture did not finish")
        code, stdout, stderr = json.loads(result_path.read_text())
        return subprocess.CompletedProcess(command, code, stdout, stderr)

    def hook(self, event_name: str, *, turn_id: str | None = "turn-1", attached=True) -> None:
        event = {
            "hook_event_name": event_name,
            "session_id": "codex-session-1",
        }
        if turn_id is not None:
            event["turn_id"] = turn_id
        result = self.run_report(
            [sys.executable, str(ADAPTER), "--hook"],
            attached=attached,
            input=json.dumps(event),
        )
        self.assertEqual(result.returncode, 0)
        self.assertEqual((result.stdout, result.stderr), ("", ""))

    def notify_complete(self, turn_id: str = "turn-1", *, attached=True) -> None:
        event = json.dumps(
            {
                "type": "agent-turn-complete",
                "thread-id": "codex-session-1",
                "turn-id": turn_id,
            }
        )
        result = self.run_report(
            [FUT, "agent", "notify", "codex", event],
            attached=attached,
        )
        self.assertEqual(result.returncode, 0)
        self.assertEqual((result.stdout, result.stderr), ("", ""))

    def agent(self) -> dict[str, object]:
        return self.cli("agent", "get", self.terminal_id)["result"]["agent"]

    def test_detached_desktop_cannot_claim_or_complete_terminal_agent(self) -> None:
        before = self.cli("list")["result"]
        self.hook("UserPromptSubmit", attached=False)
        self.notify_complete(attached=False)
        self.assertEqual(self.cli("list")["result"], before)

        # Having a controlling terminal is insufficient if it is another PTY.
        result = run_in_terminal(
            [FUT, "agent", "report", "working", "--source", "codex"],
            env=self.adapter_environment, text=True, capture_output=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "automatic agent reports must originate in the target terminal", result.stderr
        )
        self.assertEqual(self.cli("list")["result"], before)

        self.hook("UserPromptSubmit")
        self.notify_complete(attached=False)
        self.assertEqual(self.agent()["activity"]["state"], "working")
        self.notify_complete()
        self.assertEqual(self.agent()["activity"]["state"], "idle")

        # Explicit targets remain available to external controllers.
        self.cli("agent", "report", "blocked", "--terminal-id", self.terminal_id)
        self.assertEqual(self.agent()["activity"]["state"], "blocked")

    def test_sibling_fut_pane_cannot_report_for_another_pane(self) -> None:
        pane = self.cli("get", self.terminal_id)["result"]["target"]["pane"]["id"]
        self.cli(
            "pane", "split", pane, "right", "--", "/bin/sh", "-c",
            'while IFS= read -r line; do eval "$line"; done',
        )
        panes = self.cli("list")["result"]["sessions"][0]["workspaces"][0]["tabs"][0]["panes"]
        sibling = next(p["terminal_id"] for p in panes if p["terminal_id"] != self.terminal_id)
        self.hook("UserPromptSubmit")
        original = self.agent()["activity"]

        wrong_target = self.run_report(
            [
                "env", f"FUT_TERMINAL_ID={self.terminal_id}", FUT,
                "agent", "report", "completed", "--source", "codex",
            ],
            terminal_id=sibling,
        )
        self.assertNotEqual(wrong_target.returncode, 0)
        self.assertEqual(self.agent()["activity"], original)

        notification = json.dumps({"type": "agent-turn-complete", "thread-id": "codex-session-1"})
        ignored = self.run_report(
            [
                "env", f"FUT_TERMINAL_ID={self.terminal_id}", FUT,
                "agent", "notify", "codex", notification,
            ],
            terminal_id=sibling,
        )
        self.assertEqual(ignored.returncode, 0)
        self.assertEqual(self.agent()["activity"], original)

        own_target = self.run_report(
            [FUT, "agent", "report", "working", "--source", "codex"],
            terminal_id=sibling,
        )
        self.assertEqual(own_target.returncode, 0, own_target.stderr)
        sibling_activity = self.cli("agent", "get", sibling)["result"]["agent"]["activity"]
        self.assertEqual(sibling_activity["state"], "working")
        self.assertEqual(self.agent()["activity"], original)

    def test_reparented_process_with_real_inherited_context_cannot_report(self) -> None:
        self.hook("SessionStart")
        original = self.agent()["activity"]
        # Inherit real Fut context, then outlive the launcher as a desktop app does.
        result = self.run_in_pane(
            [FUT, "agent", "report", "working", "--source", "codex"],
            orphan=True,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(
            "automatic agent reports must originate in the target terminal", result.stderr
        )
        self.assertEqual(self.agent()["activity"], original)

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
