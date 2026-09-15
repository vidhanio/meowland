# Repository Guidelines

## Product

`meowland` is a Wayland compositor that runs inside the terminal. Clients connect
to a Wayland socket as usual, their windows are composited into a frame buffer the
compositor owns, and that buffer is drawn with the
[kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/).

Everything is CPU and shared memory. Only `wl_shm` buffers are accepted, so
`zwp_linux_dmabuf_v1` is deliberately not advertised and clients fall back to
shared memory. `Meowland` is the application state and owns the protocol globals,
toplevels, input routing and presentation. Pass that one state around rather than
building a second copy of any part of it.

## Development

```sh
cargo +nightly fmt
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

`rustfmt.toml` uses unstable options, so formatting goes through nightly rustfmt.
Clippy runs the nursery and pedantic groups; fix lints rather than allowing them.
`unsafe_code` is denied crate-wide, with one documented exception where client
shared memory is read as a slice.

Run the binary from a terminal that speaks the kitty graphics protocol (kitty,
Ghostty, WezTerm), or inside a pane that passes graphics through. It needs a
terminal on stdin and stdout, so redirecting its output makes it exit immediately.
Logs go to `$XDG_RUNTIME_DIR/meowland.log`, or wherever `MEOWLAND_LOG` points, with
`MEOWLAND_LOG_LEVEL` as the filter. Wayland clients connect with
`WAYLAND_DISPLAY=wayland-meowland`.

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
