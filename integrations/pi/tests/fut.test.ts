import assert from "node:assert/strict";
import { afterEach, test } from "node:test";

import fut from "../fut.ts";

type Handler = (event: any, context: any) => unknown;

const originalSocket = process.env.FUT_SOCKET;
const originalTerminalId = process.env.FUT_TERMINAL_ID;

afterEach(() => {
  restoreEnv("FUT_SOCKET", originalSocket);
  restoreEnv("FUT_TERMINAL_ID", originalTerminalId);
});

function restoreEnv(name: string, value: string | undefined) {
  if (value === undefined) delete process.env[name];
  else process.env[name] = value;
}

function harness(options: { failFirst?: boolean; blockFirst?: boolean } = {}) {
  const handlers = new Map<string, Handler>();
  const calls: Array<{ command: string; args: string[]; options: unknown }> = [];
  let shouldFail = options.failFirst ?? false;
  let unblockFirst: (() => void) | undefined;
  const firstBlocked = new Promise<void>((resolve) => {
    unblockFirst = resolve;
  });

  const pi = {
    on(event: string, handler: Handler) {
      handlers.set(event, handler);
    },
    async exec(command: string, args: string[], execOptions: unknown) {
      calls.push({ command, args, options: execOptions });
      if (options.blockFirst && calls.length === 1) await firstBlocked;
      if (shouldFail) {
        shouldFail = false;
        throw new Error("fut unavailable");
      }
      return { stdout: "", stderr: "", code: 0, killed: false };
    },
  };

  const context = {
    sessionManager: { getSessionId: () => "pi-session-123" },
    isIdle: () => false,
  };

  return {
    calls,
    releaseFirst() {
      unblockFirst?.();
    },
    install() {
      fut(pi as never);
    },
    async emit(event: string, data: Record<string, unknown> = {}, ctx = context) {
      const handler = handlers.get(event);
      assert.ok(handler, `missing ${event} handler`);
      await handler({ type: event, ...data }, ctx);
    },
    handlers,
  };
}

test("reports the complete lifecycle in native event order", async () => {
  process.env.FUT_SOCKET = "/tmp/fut.sock";
  process.env.FUT_TERMINAL_ID = "terminal-42";
  const testHarness = harness();
  testHarness.install();

  await testHarness.emit("session_start", { reason: "startup" });
  await testHarness.emit("agent_start");
  await testHarness.emit("tool_execution_start", { toolName: "ask_user" });
  await testHarness.emit("tool_execution_end", { toolName: "ask_user" });
  await testHarness.emit("agent_settled");
  await testHarness.emit("session_shutdown", { reason: "quit" });

  assert.deepEqual(
    testHarness.calls.map(({ args }) => args[2]),
    ["idle", "working", "blocked", "working", "completed", "idle"],
  );
  for (const call of testHarness.calls) {
    assert.equal(call.command, "fut");
    assert.deepEqual(call.args.slice(0, 2), ["agent", "report"]);
    assert.deepEqual(call.args.slice(3), [
      "--terminal-id",
      "terminal-42",
      "--source",
      "pi",
      "--agent-session-id",
      "pi-session-123",
    ]);
    assert.equal(call.args.includes("--turn-id"), false);
    assert.deepEqual(call.options, { timeout: 2000 });
  }
});

test("does not infer lifecycle from unrelated or already-idle tool events", async () => {
  process.env.FUT_SOCKET = "/tmp/fut.sock";
  process.env.FUT_TERMINAL_ID = "terminal-42";
  const testHarness = harness();
  testHarness.install();

  await testHarness.emit("tool_execution_start", { toolName: "bash" });
  await testHarness.emit("tool_execution_end", { toolName: "ask_user" }, {
    sessionManager: { getSessionId: () => "pi-session-123" },
    isIdle: () => true,
  });

  assert.deepEqual(testHarness.calls, []);
});

test("contains command failures and continues the serialized report queue", async () => {
  process.env.FUT_SOCKET = "/tmp/fut.sock";
  process.env.FUT_TERMINAL_ID = "terminal-42";
  const testHarness = harness({ failFirst: true });
  testHarness.install();

  await assert.doesNotReject(testHarness.emit("session_start"));
  await assert.doesNotReject(testHarness.emit("agent_start"));

  assert.deepEqual(
    testHarness.calls.map(({ args }) => args[2]),
    ["idle", "working"],
  );
});

test("serializes concurrent reports without reordering them", async () => {
  process.env.FUT_SOCKET = "/tmp/fut.sock";
  process.env.FUT_TERMINAL_ID = "terminal-42";
  const testHarness = harness({ blockFirst: true });
  testHarness.install();

  const idle = testHarness.emit("session_start");
  const working = testHarness.emit("agent_start");
  await new Promise((resolve) => setImmediate(resolve));

  assert.deepEqual(testHarness.calls.map(({ args }) => args[2]), ["idle"]);
  testHarness.releaseFirst();
  await Promise.all([idle, working]);
  assert.deepEqual(testHarness.calls.map(({ args }) => args[2]), ["idle", "working"]);
});

test("is inert outside a Fut terminal", () => {
  delete process.env.FUT_SOCKET;
  delete process.env.FUT_TERMINAL_ID;
  const testHarness = harness();
  testHarness.install();

  assert.equal(testHarness.handlers.size, 0);
  assert.deepEqual(testHarness.calls, []);
});
