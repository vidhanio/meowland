---
name: meowland
description: "Drive Meowland, a Wayland compositor that shows graphical apps in Kitty or Ghostty terminal panes. Use when the user asks to launch, list, show, or close GUI windows through Meowland, or to run or diagnose the `meowland` server."
---

# Meowland

Meowland is a headless Wayland compositor. `meowland server` hosts Wayland clients; `meowland launch` and `meowland focus` attach a terminal pane that draws one window with the Kitty graphics protocol and forwards keyboard and mouse input to it. Every other command is a one-shot request to the server's control socket in `$XDG_RUNTIME_DIR`.

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

- Results go to stdout as one JSON document. Commands without a result (`launch`, `quit`, `focus`) print nothing on success.
- Runtime failures exit 1 and write `{"error": "<message>"}` to stderr.
- Usage errors exit 2 with clap's plain-text message, not JSON.

Exact output shapes and error causes: [references/cli.md](references/cli.md).

## Check that a server is reachable

```bash
meowland --json list
```

An error starting with `cannot reach the server` means no server is listening. Do not start one unless the user asked; see [references/server.md](references/server.md) for socket activation, the foreground server, and configuration.

## Core workflow

1. Launch the app from a non-interactive shell so the command returns immediately instead of taking over a terminal:

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

## Safety rules

- Never run `meowland quit` without a selector: it closes the window that currently has compositor focus, which is usually the user's.
- Never close windows you did not launch unless the user explicitly asked.
- Never run `launch` or `focus` inside a pseudo-terminal you cannot hand to the user. With a terminal on both stdin and stdout they take over that terminal until the window closes and require Kitty or Ghostty graphics support. See [references/panes.md](references/panes.md) to show a window to the user.
- Do not stop, restart, or reconfigure the server unless asked; every Meowland window dies with it.
