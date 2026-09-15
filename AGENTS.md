# Repository Guidelines

## Product

`meowland` is a Wayland compositor that runs inside the terminal. Clients connect
to a Wayland socket as usual, their windows are composited into a frame buffer the
compositor owns, and that buffer is drawn with the
[kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/).

Everything is CPU and shared memory. `wl_shm` is the path every client can take.
`zwp_linux_dmabuf_v1` exists so clients that render on the GPU can hand their
buffers over, but their pixels are still copied on the CPU, which only works if
the driver put the buffer somewhere the CPU can reach. Video memory is not such
a place, and a client that takes the offer and is then refused has no window at
all, so the offer is opt-in (`MEOWLAND_GPU_BUFFERS`). Only linear layouts are
advertised, and only when they are. `Meowland` is the application state and owns
the protocol globals, toplevels, input routing and presentation. Pass that one
state around rather than building a second copy of any part of it.

## Development

```sh
cargo +nightly fmt
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

`rustfmt.toml` uses unstable options, so formatting goes through nightly rustfmt.
Clippy runs the nursery and pedantic groups; fix lints rather than allowing them.
`unsafe_code` is denied crate-wide, with documented exceptions where a client
buffer is read through a raw pointer: shared memory as a slice, and a mapped GPU
buffer as a slice bounded by the mapping.

Each module declares its own error type with `thiserror`, so a caller can tell
its failures apart. `anyhow` belongs to `main.rs` alone - the binary is the one
place that only has to report what went wrong, not handle it.

Run the binary from a terminal that speaks the kitty graphics protocol (kitty,
Ghostty, WezTerm), or inside a pane that passes graphics through. It needs a
terminal on stdin and stdout, so redirecting its output makes it exit immediately.
Logs go to `$XDG_RUNTIME_DIR/meowland.log`, or wherever `MEOWLAND_LOG` points, with
`MEOWLAND_LOG_LEVEL` as the filter. Wayland clients connect with
`WAYLAND_DISPLAY=wayland-meowland`.

`MEOWLAND_GPU_BUFFERS` decides whether clients are offered GPU buffers at all:
unset or `off` means they are not, `auto` offers every render node on the
machine, and any other value is the `/dev/dri/renderD…` node to offer. It is off
by default because a client that takes the offer and whose buffer the compositor
then cannot read has no window, which is worse than the slow path it would
otherwise take. Measure before turning it on.

Terminals and multiplexers do not reliably report key releases, so a press is
treated as a whole keystroke: press it, release it, and let the terminal's own
auto-repeat produce the repeats. Keep that rule. A client left holding a key the
terminal never released repeats it forever.

Tests live next to the logic they cover. The startup and state plumbing have none
on purpose: check those by running the compositor in a pane.

## Commits

Prefix the subject with the area you touched, in lowercase (`keys:`,
`compositor:`), and put backticks around code identifiers, paths and commands.
Commit a coherent change once `cargo +nightly fmt`,
`cargo clippy --all-targets -- -D warnings` and `cargo test` pass.
