import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

type Report = "idle" | "working" | "blocked" | "completed";

export default function fut(pi: ExtensionAPI) {
  const terminalId = process.env.FUT_TERMINAL_ID;
  if (!process.env.FUT_SOCKET || !terminalId) return;

  let pending = Promise.resolve();
  const report = (state: Report, ctx: ExtensionContext) => {
    const agentSessionId = ctx.sessionManager.getSessionId();
    const args = [
      "agent",
      "report",
      state,
      "--terminal-id",
      terminalId,
      "--source",
      "pi",
      "--agent-session-id",
      agentSessionId,
    ];

    pending = pending.then(async () => {
      await pi.exec("fut", args, { timeout: 2000 });
    }).catch(() => {});
    return pending;
  };

  pi.on("session_start", (_event, ctx) => report("idle", ctx));
  pi.on("agent_start", (_event, ctx) => report("working", ctx));
  pi.on("agent_settled", (_event, ctx) => report("completed", ctx));
  pi.on("tool_execution_start", (event, ctx) => {
    if (event.toolName === "ask_user") return report("blocked", ctx);
  });
  pi.on("tool_execution_end", (event, ctx) => {
    if (event.toolName === "ask_user" && !ctx.isIdle()) return report("working", ctx);
  });
  pi.on("session_shutdown", (_event, ctx) => report("idle", ctx));
}
