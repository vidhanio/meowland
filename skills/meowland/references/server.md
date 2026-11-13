# Running the Meowland server

One server per user session listens on two sockets in `$XDG_RUNTIME_DIR`:

- `meowland-control.sock`: one-shot `launch`, `list`, and `quit` requests.
- `meowland-pane.sock`: long-lived pane connections from `focus` and `launch`.

Every Wayland client launched through Meowland is a child of the server. Stopping or restarting the server closes all of their windows. Ask before doing either.

## Socket activation (preferred)

Packages ship `meowland.socket` and `meowland.service` as systemd user units. With the socket enabled, the first CLI request starts the server.

```bash
systemctl --user status meowland.socket meowland.service
systemctl --user start meowland.socket
journalctl --user -u meowland.service --since -10min
```

The service restarts on failure. Under Home Manager, `services.meowland.enable` installs these units and `services.meowland.settings` writes the configuration.

## Foreground server

Use a foreground server only when the user asks or for an isolated experiment:

```bash
meowland server [--config <file>]
```

It logs to stderr (filter with `RUST_LOG`, default `meowland=info`) and runs until interrupted. For an experiment that must not touch the user's session, give it a private runtime directory and use the same `XDG_RUNTIME_DIR` for every CLI call against it:

```bash
export XDG_RUNTIME_DIR="$(mktemp -d)"
meowland server &
meowland --json list
```

If another server already owns the sockets in that directory, startup fails with `a meowland server is already running`; stale sockets left by a dead server are replaced.

## Configuration

The server reads `--config <file>`, then `$MEOWLAND_CONFIG`, then `$XDG_CONFIG_HOME/meowland/config.toml`. A missing default file means defaults; a missing explicit file is an error. Unknown keys are errors. Changes apply when the server restarts.

```toml
[output]
width = 1920          # physical pixels
height = 1080
scale = 1.0           # 0.5 to 4
refresh_rate = 120    # 1 to 1000 Hz

[keyboard]
layout = "us"         # XKB layouts, comma-separated
variant = ""
options = ""          # XKB options, comma-separated
repeat_delay = 600    # milliseconds
repeat_rate = 25      # per second; 0 disables repeat

[shortcuts]
close_window = "alt+q"
detach_pane = "alt+w"
```

`close_window` asks the pane's window to close. `detach_pane` releases the pane and leaves its window open without an owner, so another pane can claim it with `meowland focus`; ending a pane any other way asks its window to close.

Omitting the `[shortcuts]` table keeps both default bindings. A present table disables any binding it omits, and an empty string disables a binding. Bindings are modifiers joined to one key with `+`, such as `ctrl+shift+q`.
