import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

type Report = "idle" | "working" | "blocked" | "completed";

export default function fut(pi: ExtensionAPI) {
  if (!process.env.FUT_TERMINAL_ID) return;

  let pending = Promise.resolve();
  const report = (state: Report) => {
    pending = pending.then(async () => {
      await pi.exec("fut", ["terminal", "report", state], { timeout: 2000 });
    }).catch(() => {});
    return pending;
  };

  pi.on("session_start", () => report("idle"));
  pi.on("agent_start", () => report("working"));
  pi.on("agent_settled", () => report("completed"));
  pi.on("tool_execution_start", (event) => {
    if (event.toolName === "ask_user") return report("blocked");
  });
  pi.on("tool_execution_end", (event, ctx) => {
    if (event.toolName === "ask_user" && !ctx.isIdle()) return report("working");
  });
  pi.on("session_shutdown", () => report("idle"));
}
