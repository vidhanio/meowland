# Meowland CLI reference

`--json` is global: it is accepted before or after the subcommand name, but after `launch`'s program it belongs to the launched program. Put it first: `meowland --json <command> ...`.

## Exit status

| Status | Meaning | stderr with `--json` |
| --- | --- | --- |
| 0 | Success | empty |
| 1 | Runtime failure | `{"error": "<message>"}` |
| 2 | Invalid arguments | clap plain text |

The error message is the full cause chain joined by `: `, outermost first.

## Window IDs

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
| `id` | string | Exact selector for `quit` |
| `app_id` | string | Client-chosen; may be empty or shared by several windows |
| `title` | string | Client-chosen; may be empty or contain control characters |
| `state` | `"active"` or `"inactive"` | `active` means the window holds compositor focus; at most one does |

An empty array means the server is running with no mapped windows. JSON strings are the exact values; the plain-text output sanitizes control characters and is not safe to parse.

## `launch`

```bash
meowland --json launch [--] <program> [args...]
```

Starts `<program>` under the server with the caller's environment and working directory. The server sets `WAYLAND_DISPLAY` and `XDG_ACTIVATION_TOKEN` for it. Success means the server spawned the process, not that a window mapped; poll `list` for it.

## `quit`

```bash
meowland --json quit <ID>
```

Asks the selected window to close. The client decides whether and when it closes. Without a selector it targets the focused window; do not omit the selector.

`--app-id <APP_ID>` or `--title <TITLE>` select by exact value instead, picking the most recently mapped match; prefer the ID.

Failure when nothing matches: `server rejected the request: no such window`.

## Common errors

| Message starts with | Cause | Next step |
| --- | --- | --- |
| `XDG_RUNTIME_DIR is not set` | The shell has no user runtime directory | Run from the user's login session environment |
| `cannot reach the server` | Nothing listens on `$XDG_RUNTIME_DIR/meowland-control.sock` | [server.md](server.md) |
| `server rejected the request: ` | The server refused, for example no window matched | Re-run `list` and retarget |
