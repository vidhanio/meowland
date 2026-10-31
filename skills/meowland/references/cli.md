# Meowland CLI reference

`--json` is global: it is accepted before or after the subcommand name, but after `launch`'s program it belongs to the launched program. Put it first: `meowland --json <command> ...`.

## Exit status

| Status | Meaning | stderr with `--json` |
| --- | --- | --- |
| 0 | Success | empty |
| 1 | Runtime failure | `{"error": "<message>"}` |
| 2 | Invalid arguments | clap plain text |

The error message is the full cause chain joined by `: `, outermost first.

## Selectors

`focus` and `quit` take at most one selector; `launch` takes `--app-id` or `--title` only.

| Selector | Matches |
| --- | --- |
| `<ID>` (positional) | The one window with this server-assigned ID |
| `--app-id <APP_ID>` | Windows whose Wayland app ID equals the value exactly |
| `--title <TITLE>` | Windows whose title equals the value exactly |

When several windows match an app ID or title, the most recently mapped one is selected.

A window ID is `<prefix>_<suffix>`: the app ID with every character other than ASCII letters, digits, `.`, `-`, and `_` replaced by `-` (or `window` when the app ID is empty), then a random suffix, for example `GTK-Application_q8fg`. It is unique among current windows and stays fixed while the window exists. Titles change whenever the client retitles itself.

## `list`

Lists mapped windows.

```bash
meowland --json list
```

```json
[{"id":"GTK-Application_q8fg","app_id":"GTK Application","title":"~/src","state":"active"}]
```

| Field | Type | Notes |
| --- | --- | --- |
| `id` | string | Exact selector for `focus` and `quit` |
| `app_id` | string | Client-chosen; may be empty or shared by several windows |
| `title` | string | Client-chosen; may be empty or contain control characters |
| `state` | `"active"` or `"inactive"` | `active` means the window holds compositor focus; at most one does |

An empty array means the server is running with no mapped windows. JSON strings are the exact values; the plain-text output sanitizes control characters and is not safe to parse.

## `launch`

```bash
meowland --json launch [--app-id <APP_ID> | --title <TITLE>] [--no-follow] [--] <program> [args...]
```

Starts `<program>` under the server with the caller's environment and working directory. The server sets `WAYLAND_DISPLAY` and `XDG_ACTIVATION_TOKEN` for it. Success means the server spawned the process, not that a window mapped; poll `list` for it.

When stdin and stdout are both terminals, `launch` then attaches the terminal as a pane:

- By default the pane shows the window the launched client activates, and keeps waiting for the next matching window when it closes.
- `--app-id`/`--title` show the newest window matching the selector instead.
- `--no-follow` exits when the first shown window closes.

Otherwise `launch` returns as soon as the process is spawned.

## `quit`

```bash
meowland --json quit <ID>
```

Asks the selected window to close. The client decides whether and when it closes. Without a selector it targets the focused window; do not omit the selector.

Failure when nothing matches: `server rejected the request: no such window`.

## `focus`

```bash
meowland focus <ID> | --app-id <APP_ID> | --title <TITLE> [--follow]
```

Attaches the current terminal as a pane showing the selected window. Requires an interactive Kitty or Ghostty terminal; it blocks until the window closes or the user detaches. `--follow` waits for a matching window and moves to the next match whenever the shown one closes. `--follow` cannot be combined with an ID. See [panes.md](panes.md).

## `server` and `completions`

`server` runs the compositor in the foreground; see [server.md](server.md). `completions <shell>` prints a shell completion script and ignores `--json`.

## Common errors

| Message starts with | Cause | Next step |
| --- | --- | --- |
| `XDG_RUNTIME_DIR is not set` | The shell has no user runtime directory | Run from the user's login session environment |
| `cannot reach the server` | Nothing listens on `$XDG_RUNTIME_DIR/meowland-control.sock` | [server.md](server.md) |
| `server rejected the request: ` | The server refused, for example no window matched | Re-run `list` and retarget |
| `could not reach the pane socket` | `focus`/`launch` could not attach a pane | [server.md](server.md) |
| `terminal must support Kitty graphics` / `terminal must support Kitty keyboard` / `terminal does not support SGR pixel mouse reporting` | The terminal running `focus`/`launch` cannot host a pane | Use Kitty or Ghostty 1.4+ |
