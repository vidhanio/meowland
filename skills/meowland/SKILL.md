---
name: meowland
description: "Drive Meowland, a Wayland compositor that shows graphical apps in Kitty or Ghostty terminal panes. Use when the user asks to launch, list, or close GUI windows through Meowland, or to run or diagnose the `meowland` server."
---

# Meowland

Meowland is a headless Wayland compositor. `meowland server` hosts Wayland clients, and the other commands you run are one-shot requests to the server's control socket in `$XDG_RUNTIME_DIR`.

## Learn the installed CLI

The installed binary is the authority for syntax:

```bash
meowland --help
meowland <command> --help
```

## Always use `--json`

Pass `--json` before the subcommand on every call you parse:

```bash
meowland --json list
```

- Results go to stdout as one JSON document. `launch` and `quit` print nothing on success.
- Runtime failures exit 1 and write `{"error": "<message>"}` to stderr.
- Usage errors exit 2 with clap's plain-text message, not JSON.

Exact output shapes and error causes: [references/cli.md](references/cli.md).

## Check that a server is reachable

```bash
meowland --json list
```

An error starting with `cannot reach the server` means no server is listening. Do not start one unless the user asked; see [references/server.md](references/server.md) for socket activation, the foreground server, and configuration.

## Core workflow

1. Launch the app. From a non-interactive shell the command returns as soon as the server has spawned it:

   ```bash
   meowland --json launch -- <program> [args...]
   ```

   The app runs under the server, not under your shell, with your environment and working directory plus `WAYLAND_DISPLAY` pointing at Meowland.

2. Wait for the window to map, then read its ID from `list`. Windows appear asynchronously; poll `list` briefly rather than assuming the first call shows them:

   ```bash
   meowland --json list | jq -r '.[] | select(.app_id == "<app id>") | .id'
   ```

3. Target that exact `id` for later commands. App IDs and titles are client-chosen and may match several windows; IDs are unique for a window's lifetime and never guessed from examples.

4. Close only windows you launched:

   ```bash
   meowland --json quit <id>
   ```

   `quit` asks the client to close; the client may refuse or prompt. Re-run `list` to confirm the window is gone.

## Showing a window to the user

You cannot display a window yourself: a pane needs an interactive Kitty or Ghostty 1.4+ terminal. Give the user the command to run there:

```bash
meowland focus <id>
```

or `meowland focus --app-id <app id> --follow` to keep showing the newest window of that app across restarts. Never run `focus` yourself.

## Safety rules

- Never run `meowland quit` without a selector: it closes the window that currently has compositor focus, which is usually the user's.
- Never close windows you did not launch unless the user explicitly asked.
- Do not stop, restart, or reconfigure the server unless asked; every Meowland window dies with it.
