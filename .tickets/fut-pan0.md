---
id: fut-pan0
status: closed
deps: [fut-dafg]
links: []
created: 2026-08-09T20:48:01Z
type: task
priority: 1
assignee: Mikkel Malmberg
parent: fut-m59z
tags: [agent, integration, pi]
---
# Migrate the Pi integration to the agent lifecycle contract

Update the existing Pi extension to report through the final fut agent API and provide any supported identity or turn metadata.

## Design

Keep the extension one-way and narrow: translate Pi lifecycle and ask-user hooks into Fut reports. It must not create layout, submit prompts, read output, or coordinate agents.

## Acceptance Criteria

Pi reports initial idle, working, blocked, resumed working, completed, and shutdown consistently; metadata is included only when authoritative; failures remain bounded and do not disrupt Pi; integration tests cover event ordering.


## Notes

**2026-08-09T21:47:45Z**

Migrated the Pi extension to canonical fut agent report with explicit terminal/source/Pi session metadata. Lifecycle mapping is session_start→idle, agent_start→working, ask_user start→blocked, active ask_user end→working, agent_settled→completed, session_shutdown→idle. Reports remain serialized, use a 2s timeout, swallow failures, and the extension is inert outside Fut. Pi exposes no stable lifecycle turn ID, so none is invented. Added Pi installation/mapping/limitations docs and 5 Node integration tests covering ordering, argv/metadata, filtering, failure recovery, serialization, and inertness. Validation: node --test integrations/pi/tests/fut.test.ts; cargo run --quiet -- agent report --help; git diff --check.
