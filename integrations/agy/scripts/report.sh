#!/bin/sh

# Antigravity invokes this hook with its event JSON on stdin.

state=${1-}
case "$state" in
  idle|working|blocked|completed|exited) ;;
  *)
    echo '{"decision":"allow"}'
    exit 0
    ;;
esac

# Antigravity can run in CLI, IDE, or desktop environments. Fut validates
# inherited identifiers against the reporting process ancestry.
if [ -z "${FUT_SOCKET-}" ] || [ -z "${FUT_TERMINAL_ID-}" ]; then
  echo '{"decision":"allow"}'
  exit 0
fi

fut_bin="${FUT_BIN:-fut}"
if ! command -v "$fut_bin" >/dev/null 2>&1; then
  echo '{"decision":"allow"}'
  exit 0
fi

# Common hook fields put conversationId near the start of the JSON object. Bound
# memory even when payloads carry large contexts, then drain the remainder so
# Antigravity never waits on a full stdin pipe.
payload=$(dd bs=4096 count=32 2>/dev/null)
cat >/dev/null 2>&1 || :

# Antigravity does not have a separate StopFailure hook; API errors and uncaught
# exceptions settle via Stop with an error termination reason or field.
if [ "$state" = "completed" ] && printf '%s\n' "$payload" | grep -qiE '"terminationReason"[[:space:]]*:[[:space:]]*"error"|"error"[[:space:]]*:[[:space:]]*"[^"]+"'; then
  state="blocked"
fi

agent_session_id=$(
  printf '%s\n' "$payload" \
    | sed -nE 's/.*"conversationId"[[:space:]]*:[[:space:]]*"([^"\\]*)".*/\1/p' \
    | sed -n '1p'
)
case "$agent_session_id" in
  ''|*[!A-Za-z0-9._:-]*) agent_session_id='' ;;
  *) agent_session_id=$(printf '%.128s' "$agent_session_id") ;;
esac

if [ -n "$agent_session_id" ]; then
  "$fut_bin" agent report "$state" \
    --source agy \
    --agent-session-id "$agent_session_id" \
    >/dev/null 2>&1 || :
else
  "$fut_bin" agent report "$state" \
    --source agy \
    >/dev/null 2>&1 || :
fi

echo '{"decision":"allow"}'
exit 0
