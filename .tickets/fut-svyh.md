---
id: fut-svyh
status: closed
deps: []
links: []
created: 2026-08-09T20:47:36Z
type: feature
priority: 1
assignee: Mikkel Malmberg
parent: fut-m59z
tags: [agent, protocol]
---
# Represent integration presence and lifecycle events explicitly

Replace the ambiguous default-idle activity shape with an explicit distinction between an unintegrated terminal and a terminal reporting agent activity. Retain the latest lifecycle event separately from current status.

## Design

Model integration presence plus current status and a revisioned last event carrying idle, working, blocked, or completed. Keep per-client attention derived and presentation-only. Allow optional integration source, agent session ID, and turn ID without requiring them for synchronization.

## Acceptance Criteria

Snapshots distinguish never-reported terminals from integrated idle agents; completed remains observable as the latest event; reports remain bounded; protocol and resource tests cover transitions and compatibility.


## Notes

**2026-08-09T21:03:59Z**

Implemented explicit AgentActivity integration presence and authoritative last_event while retaining legacy state/attention fields for existing UI consumers. Added bounded optional source, agent_session_id, and turn_id metadata to ReportAgent; resource validation rejects oversized values without revision changes, and daemon returns invalid_agent_report. Legacy activity decoding infers presence/latest events, including completed attention. Verified 324 lib tests, 81 e2e tests, cargo check --all-targets, fmt, and clippy -D warnings.

**2026-08-09T21:06:53Z**

Finalized authoritative serialization: AgentActivity no longer serializes or stores attention. Notification/unread presentation is derived from last_event; legacy attention is accepted only by custom deserialization to infer old completed/blocked snapshots. Bumped protocol 7 to 8 for the resource shape change and added a serialized-shape assertion. Reverified 324 lib and 81 e2e tests plus clippy.
