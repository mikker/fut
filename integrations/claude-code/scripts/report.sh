#!/bin/sh

# Claude Code invokes this hook with its event JSON on stdin. It is deliberately
# a one-way lifecycle reporter: no hook output is fed back to Claude.

state=${1-}
case "$state" in
  idle|working|blocked|completed|exited) ;;
  *) exit 0 ;;
esac

# Claude Code can also load this plugin in IDE, desktop, and web environments.
# Only a local Claude process running inside Fut has both scoped identifiers.
if [ -z "${FUT_SOCKET-}" ] || [ -z "${FUT_TERMINAL_ID-}" ]; then
  exit 0
fi

if ! command -v fut >/dev/null 2>&1; then
  exit 0
fi

# Common hook fields put session_id near the start of the JSON object. Bound
# memory even when UserPromptSubmit carries a very large prompt, then drain the
# remainder so Claude never waits on a full stdin pipe.
payload=$(dd bs=4096 count=32 2>/dev/null)
cat >/dev/null 2>&1 || :
agent_session_id=$(
  printf '%s\n' "$payload" \
    | sed -nE 's/.*"session_id"[[:space:]]*:[[:space:]]*"([^"\\]*)".*/\1/p' \
    | sed -n '1p'
)
case "$agent_session_id" in
  ''|*[!A-Za-z0-9._:-]*) agent_session_id='' ;;
  *) agent_session_id=$(printf '%.128s' "$agent_session_id") ;;
esac

if [ -n "$agent_session_id" ]; then
  fut agent report "$state" \
    --terminal-id "$FUT_TERMINAL_ID" \
    --source claude-code \
    --agent-session-id "$agent_session_id" \
    >/dev/null 2>&1 || :
else
  fut agent report "$state" \
    --terminal-id "$FUT_TERMINAL_ID" \
    --source claude-code \
    >/dev/null 2>&1 || :
fi

exit 0
