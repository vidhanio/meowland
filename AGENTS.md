# Repository Guidelines

## Architecture

`meowland` is a Wayland compositor that draws windows in a terminal with the
kitty graphics protocol. `src/server/` owns the server; `src/client/` owns one
attached terminal, called a pane. Panes are independent and each shows one
window. The server stays running after panes detach.

The compositor runs on a thread of its own, in `src/wayland/`, and the server
reaches it only in messages: `src/wayland/message.rs` is the whole of the
interface. The compositor holds the Wayland clients, their surfaces, and the
frames those add up to; the server holds the panes, the control socket, the
client programs, and xwayland-satellite. Nothing on one side reaches into the
other, so composing a frame never makes an input event wait.

`Compositor` in `src/wayland/state/mod.rs` owns protocol state, windows, input
routing, and composition. Keep that state in one place. Each toplevel fills its
pane; there is no tiling. An empty pane follows the newest window once it has
pixels.

A pane has one frame. The compositor draws into it, sends it with the tiles that
changed, and cannot draw that pane again until the presenter gives it back. So
frames are dropped while a pane's terminal is behind, but the tiles due are not:
the next frame is diffed against the last one that was sent. The compositor
sends frame callbacks when it draws, so clients are paced by what the terminal
can take.

Both `wl_shm` and `zwp_linux_dmabuf_v1` produce a CPU-side `Snapshot`.
Advertise GPU buffers only when the renderer can read them back; refusing an
advertised buffer can leave a client without a window. CPU composition is shared
by both paths. Each pane's presenter thread cuts the changed tiles out, compresses
them, and writes them to the terminal's socket.

`wp_viewporter` is applied during drawing. X11 clients require
xwayland-satellite. If it is unavailable, do not pass an inherited `DISPLAY` to
clients.

## Server behavior

```sh
meowland run foot
meowland list
meowland attach 2
meowland attach
meowland server start
meowland server stop
```

`run` starts a server if needed, then sends the command over the control socket.
It shows a window opened by that command, waiting up to ten seconds. A server
has no terminal of its own and keeps running until stopped or signaled. A `run`
without a terminal still starts the client.

Windows have server-assigned IDs. `attach` takes an ID or defaults to the
focused window. Typing or clicking focuses the pane's window. `Alt+Q` sends
`xdg_toplevel.close`, or detaches an empty pane. A pane detaches when its window
closes.

The first pane showing a window determines its configured size. A window with
no pane is not configured. Focused windows receive `Activated`; windows that
requested fullscreen receive `Fullscreen`.

The control and pane sockets are owner-only files in `$XDG_RUNTIME_DIR`. The
Wayland socket is `wayland-meowland`. `src/protocol/pane.rs` defines the pane
protocol: one frame in flight per pane, with ordered frames and escapes. Only
frames are acknowledged. The terminal side uses separate threads for input,
output, and hangup checks.

Client stdout and stderr go to `$XDG_RUNTIME_DIR/meowland.log`, not the pane.
Window titles and `list` output must filter terminal control characters.
Shared-memory graphics transfers use pane-specific object names.

On stop, ask every window to close (`xdg_toplevel.close`) and give the clients
that honour it a moment to leave, then detach the panes, signal the server's
process tree with `SIGHUP`, `SIGTERM`, and `SIGKILL` — rescanning before each
signal, because unix does not stop children when their parent exits — and then
join the compositor thread. Daemonized processes outside the tree are not
included.

## Development

```sh
cargo +nightly fmt
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

`rustfmt.toml` needs nightly rustfmt. Fix nursery and pedantic clippy warnings
instead of allowing them. `unsafe_code` is denied crate-wide; keep exceptions
local and document their safety contracts. The library uses the single error
type in `src/error.rs`; `anyhow` belongs in `src/main.rs` only.

Flags override environment variables: `--gpu-buffers`
(`MEOWLAND_GPU_BUFFERS`, default `auto`), `--render-node`
(`MEOWLAND_RENDER_NODE`), `--log` (`MEOWLAND_LOG`), and `--log-level`
(`MEOWLAND_LOG_LEVEL`). Pass resolved settings to modules. `--gpu-buffers off`
disables GPU buffer offers. EGL must be loadable when GPU buffers are enabled.

Terminal key releases are unreliable. Treat each key as a press followed by a
release; the terminal supplies repeats. Preserve this behavior.

Tests live beside the logic they cover. Check startup and state plumbing by
running the compositor in a pane.

## Commits

Use a lowercase area prefix (`keys:`, `compositor:`) and backticks around code
identifiers, paths, and commands. Commit coherent changes after formatting,
clippy, and tests pass.
