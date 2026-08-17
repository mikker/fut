# Ghostty window title

Keep the current Ghostty window title in sync with the active Fut session and
restore the previous title when the client detaches.

Add the extension's absolute directory to `~/.config/fut/config.toml`:

```toml
extensions = [
  "/absolute/path/to/fut/examples/extensions/ghostty-title",
]
```

Restart Fut after enabling the extension. The hook is inert outside Ghostty.
