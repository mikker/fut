# Example workspace status extension

This intentionally small extension publishes the latest supported lifecycle event for each workspace. It demonstrates packaged hook executables, Fut's hook environment, and callback through the ordinary control CLI.

Load this directory by absolute path:

```toml
extensions = ["/absolute/path/to/fut/examples/extensions/example-workspace-status"]

[ui.sidebar.left]
components = [
  { component = "workspaces", row = { right = [{ token = "workspace.extension.example-workspace-status.last_event" }] } },
]
```

The example has no installer, dependencies, background service, action, or native UI.
