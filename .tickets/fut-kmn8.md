---
id: fut-kmn8
status: open
deps: []
links: []
created: 2026-08-09T11:12:11Z
type: bug
priority: 3
assignee: Mikkel Malmberg
---
# Flaky: project git-timeout tests lose the 50ms race under full-suite load

project::tests::slow_git_is_killed_at_the_timeout and exited_git_with_inherited_pipe_is_killed_at_the_timeout occasionally fail during 'cargo test' (all targets in parallel): the fake git script is killed at the 50ms resolver timeout before it writes descendant.pid, so read_to_string at src/project.rs:633 hits NotFound. Passes in isolation. Seen 2026-08-09 on macOS. Fix idea: have the script write the pid file before sleeping, or wait for the pid file before asserting.

