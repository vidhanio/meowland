# meowland

A Wayland compositor that shows windows in terminal panes using the kitty graphics protocol. A server process owns the display and the window list; each attached terminal pane receives composed frames and forwards input. Committed shared-memory and DMA-BUF pixels become owned snapshots so later client writes cannot change a frame in transit.

## Build and run

Requires Linux 5.3 or newer (pidfds for process cleanup) and a terminal with kitty graphics support. The Nix dev shell provides the nightly Rust toolchain (including rustfmt, clippy, rust-src and rust-analyzer); outside Nix, install nightly Rust and use it for builds and formatting. Crossterm and Smithay are Git dependencies without manual revision pins in `Cargo.toml`; `Cargo.lock` records the exact commits for reproducible builds. Use `cargo update -p crossterm -p smithay` to advance them to upstream HEAD.

```sh
nix develop
cargo build --locked
cargo run -- server                # compositor, in the foreground
cargo run -- run <program> [...]   # from another terminal: client and pane
```

Outside Nix, builds need `pkg-config` and the libxkbcommon development files; GPU integration tests also need libgbm development files. DMA-BUF import requires an accessible DRM render node and a compatible EGL/GLES 3 driver with fence synchronization. A missing or unusable graphics stack leaves the server on `wl_shm`, without advertising `zwp_linux_dmabuf_v1`.

`MEOWLAND_RENDER_NODE=auto` (the default) selects the first usable EGL render device. Set it to a render-node path such as `/dev/dri/renderD128` to select a GPU, or `off` to disable GPU initialization. Driver-loading failures are logged; Nix-built programs need graphics drivers compatible with their runtime libraries, not merely an existing `/dev/dri` node.

Nothing starts the server on your behalf: `meowland server` runs the compositor in the foreground and is refused when another server already owns the runtime sockets. `share/systemd/user/meowland.service` is a ready-made user unit for keeping one in the background, and the Nix package installs the same file under `$out/share/systemd/user/` with an absolute `ExecStart`:

```sh
systemctl --user link "$PWD/share/systemd/user/meowland.service"
systemctl --user enable --now meowland.service
```

`systemctl --user stop meowland.service`, Ctrl-C, or SIGTERM shuts the server down. With one running, `meowland run <program>` attaches to the next window when invoked from a terminal, `meowland attach [window-id]` shows an existing window, and `meowland list` lists windows. A non-terminal `run` launches the client without attaching a pane. The checked-in unit's `ExecStart=meowland` needs `meowland` on the service `PATH`; use `systemctl --user edit meowland.service` when it lives elsewhere. To keep the user manager alive across *logout*, enable lingering for your user separately (`loginctl enable-linger`); it is not changed by meowland.

Server and launched-client stdout/stderr go to the user journal: `journalctl --user -u meowland.service -f`; a foreground server writes them to its own terminal instead. Pane throughput and terminal capabilities go directly to the journal through `tracing-journald` (`journalctl --user -t meowland -f`), without writing to the terminal. `RUST_LOG`, `MEOWLAND_XWAYLAND` and `MEOWLAND_RENDER_NODE` are read from the environment of the server process (`systemctl --user set-environment` for the unit), and there is no `MEOWLAND_LOG` file.

## Code map

- `src/compositor/mod.rs`: Smithay display, window/pane ownership and command dispatch.
- `src/compositor/snapshot.rs`: bounded, owned copies of committed shared-memory buffers.
- `src/compositor/dmabuf.rs`: optional EGL/GLES import and synchronous readback into owned snapshots.
- `src/compositor/frame.rs`: independent Wayland callback clock and single-in-flight pane frames.
- `src/compositor/render.rs`, `input.rs`: surface composition/hit testing and terminal input translation.
- `src/server/mod.rs`, `transport.rs`, `process.rs`: service state, socket I/O and client lifecycle.
- `src/terminal.rs`, `kitty.rs`: terminal event loop and kitty graphics presenter/encoder.
- `src/protocol.rs`: bounded control and pane messages.
- `src/main.rs`, `src/cli/`: binary-owned CLI and tracing subscribers.
- `src/signals.rs`: termination registrations scoped to one server or pane invocation.

The compositor coalesces client commits while a pane has a frame in flight; the pane acknowledges only after its terminal write completes. Wayland frame callbacks have a separate 60 Hz clock, so a stalled terminal does not stop the client. Pane frames carry only changed rows; the presenter limits its cell diff to rows that may differ from the whole-image base, retaining earlier patch damage until it is replaced or reverted. It patches a cell-aligned diff up to a quarter of the frame and sends one whole frame — through shared memory when the terminal supports it — beyond that. A pane that never acknowledges a frame remains on that frame until it reads again or disconnects; it does not build an unbounded frame queue.

CPU composition specializes opaque copies separately from premultiplied-alpha blending. Viewport scaling reuses horizontal sample indices across rows in bounded stack storage, without allocating a frame-width lookup table. These optimizations follow the damage-local rendering approach in [terminal-browser](https://github.com/zenbu-labs/terminal-browser/tree/main/pixel/engine/crates/pixel-core/src/terminal/present); meowland keeps its existing bounded patch replacement rather than adding a retained patch pool.

DMA-BUF feedback identifies the selected DRM device and its actual supported RGB32 format/modifier pairs (ARGB/XRGB/ABGR/XBGR8888). Implicit writer fences block surface transactions before pixels or geometry become current; GPU sampling and readback complete before `wl_buffer.release`. Premultiplied alpha and `Y_INVERT` are preserved. Composition and kitty presentation remain CPU-side: this is GPU-client interoperability, not zero-copy terminal output or a fully GPU-rendered compositor. Explicit-sync protocols and interlaced buffers are not supported.

Pane sockets are received incrementally without changing their blocking write flags. Partial packets do not block terminal input, termination signals or the handshake deadline; each readiness pass consumes at most 64 KiB of body data. Both directions reject messages larger than 64 MiB, and the encoder's reusable scratch buffer is bounded to that limit plus its four-byte header.

The Crossterm Git dependency includes the upstream zero-coordinate mouse parser fix, so reports at the left or top edge cannot underflow and crash the pane. The pane integration suite checks edge press/release events all the way through to Wayland pointer coordinates.

Server socket owners remove only their own paths; startup preserves regular files and live listeners and reclaims only refused, stale sockets. Shutdown cancels unfinished handshakes, closes stalled panes after the existing grace period, and joins transport threads. Normal detach still delivers Release after any in-flight frame. Process shutdown retains pidfds across signal escalation, so already-discovered helpers remain reachable if their parent exits and they become orphaned.

Run `cargo test --all-targets --locked` for unit and process-level Wayland/server checks. Pane integration checks drive real attach processes on ptys and compare decoded pixels through whole frames, distant patches, reversions and fragmented packets; they also verify terminal restoration during incomplete packets. Server checks cover socket ownership, startup rollback and orphaned-helper shutdown. `cargo test --locked --test dmabuf -- --nocapture` checks real GPU pixels, alpha, row inversion, release/reuse and snapshot ownership, plus shm-only fallback and rejected imports. GPU-dependent scenarios report a skip when no usable EGL/GBM device exists; the GPU-off scenario always runs. `cargo bench` measures the encoder and wire codec, including changed and unchanged row bands.
