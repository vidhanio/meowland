# Showing windows in terminal panes

A pane is an interactive terminal running `meowland focus` or `meowland launch`. It draws one window with the Kitty graphics protocol and sends the terminal's keyboard and mouse input to that window. The pane process owns the terminal until it exits.

## Requirements

- stdin and stdout are the same interactive terminal.
- The terminal is Kitty or Ghostty 1.4+: it must support Kitty graphics with in-place frame updates, the Kitty keyboard protocol with event types and all-key reporting, and SGR pixel mouse reporting.
- The server is reachable through `$XDG_RUNTIME_DIR/meowland-pane.sock`.

An agent's tool shell does not meet these requirements. To show a window to the user, give them the exact command for their terminal:

```bash
meowland focus <id>
```

Use `meowland focus --app-id <app id> --follow` when they want the pane to keep showing the newest window of an app across restarts.

## Inside a pane

| Default key | Action | Config key |
| --- | --- | --- |
| Alt+Q | Ask the shown window to close | `shortcuts.close_window` |
| Alt+W | Detach the pane; the window keeps running | `shortcuts.detach_pane` |

The compositor's output size is fixed by server configuration and does not follow the pane's size. Several panes may show the same window.

Pane diagnostics go to the user's systemd journal, or to `$XDG_STATE_HOME/meowland/pane.log` when journald is unavailable. Set `RUST_LOG` (default `meowland=info`) to change their verbosity.
