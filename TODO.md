# TODO

Open work comes first. Completed items are condensed under [Done](#done), each with a
note on any decision a future change should respect. See the README for the
architecture diagrams. Paths are relative to `crates/` unless stated otherwise.

Out of scope by decision: protocol versioning (the project is pre-release), pinning
the nightly toolchain, and deduplicating the systemd units (the Home Manager module
needs its own copy).

## Inventory

About 20.5k lines of Rust in 16 crates, plus the Python benchmark harness.

| Crate | Lines | Role |
|---|---|---|
| `meowland` | 463 | CLI (`usage-rs`), TOML config loading, logging, `anyhow` error reporting |
| `meowland-server` | 1130 | Owns the `calloop` loop: sockets, socket activation, child processes, shutdown |
| `meowland-compositor` | 4340 | Smithay state (`Protocols`, `Input`, `Windows`), shell, seat, input injection, keymap, frame pacing |
| `meowland-loop` | 216 | `CompositorHandle` projection: register calloop sources against a projected state |
| `meowland-panes` | 1543 | Window catalog and pane attachments, generic over a `Presentation` trait (no Smithay) |
| `meowland-selection` | 840 | Wayland ↔ terminal clipboard bridge behind a `Host` trait |
| `meowland-readback` | 665 | DMA-BUF acquire fences, GPU submit and completion behind a `Host` trait |
| `meowland-render` | 2601 | CPU composition, damage, shm snapshots, EGL/GLES DMA-BUF backend |
| `meowland-protocol` | 2029 | Postcard wire types, sync and async framing, shared frames, clipboard limits |
| `meowland-terminal` | 2128 | Pane client: probe, session loop, presentation, terminal modes |
| `meowland-terminal-clipboard` | 1213 | OSC 5522 (Kitty) and OSC 52 clipboard state machines |
| `meowland-input` | 1558 | VT parsing, SGR pixel mouse, Kitty key events, oversized-control recovery |
| `meowland-kitty` | 355 | Kitty graphics encoder (zlib, base64, chunking, rectangle edits) |
| `meowland-async` | 298 | calloop-local async I/O, timers, scheduling, and signals |
| `meowland-core` | 463 | `FrameSize`, `UiScale`, `RefreshRate`, latency percentiles |
| `meowland-bench` | 615 | Benchmark workload binary and the `cargo bench` shim |

Also in the repo: `benchmarks/` (Python harness, `smoke.py`, `GUIDE.md`),
`share/systemd/user` (socket and service units), `flake.nix` (crane build, Home
Manager module, treefmt), `.github/workflows/ci.yaml` (fmt, clippy, tests, docs,
headless smoke test).

## Open

### In progress

- [ ] **Reusable shared-frame slots** (branch `todo/terminal-5`, not merged; needs a
  design decision). Replaces the fresh POSIX shm object per frame (~0.45 ms of a
  4.6 ms full-1080p frame is page faults) with a two-slot ring of `t=f` files freed
  only by the terminal's `OK` reply for that exact transfer (`q=1`, image id +
  placement id). Timeouts and errors retire the slot. `t=s` objects remain the
  fallback, then inline RGB. Four review rounds fixed reply correlation, slot release
  on fallback, stale repaints, and probe independence. The fourth round requested
  changes again, with two blocking holes:
  - **Image-id namespaces can collide across processes.** The per-process counter
    with PID-derived high bits repeats for PIDs equal mod 32768, after PID reuse, and
    across PID namespaces; the reviewer reproduced a premature release by forcing
    equal PIDs. Kitty `a=q` does not look up stored images, and `a=d` cannot remove
    replies already queued in the PTY, so neither is a barrier. Options: a locked,
    persistent, monotonic allocator in the runtime directory shared by all pane
    processes, which fails closed to objects or inline on any coordination failure
    and is scoped to one host and user; a real terminal-input barrier (a unique
    marker, then draining replies through it); or accepting probabilistic safety
    explicitly and documenting it.
  - **`Resync` resets the producer's transfer counter.** An ordinary keyframe request
    on the last transfer restarts `p=1` under an unchanged image id. Simplest fix:
    never reset; on exhaustion, retire the ring and disable slots for that
    attachment.

  If the complexity is not worth ~10% of a full-frame update, close the branch and
  keep the one-shot `t=s` objects. See `HANDOFF.md` for the decision options.

### Performance

- [ ] **Restrict DMA-BUF readback to the damage.** The GPU blit and
  `copy_framebuffer` still read the whole buffer. Smithay's renderer has no
  per-rectangle mapping export, so this needs a separate target texture and copy per
  damaged region.

### Testing

AGENTS.md allows purposeful tests only: every test must guard a real invariant, edge
case, regression, or contract.

- [ ] **End-to-end incremental rendering.** Blending, viewports, transforms, clipped
  blits, and the opaque-cover decision helper are covered. Damage-limited
  recomposition matching full recomposition still needs a `Renderer::update` fixture
  with real `WlSurface` commits.
- [ ] **Fuzz the parsers.** Add `cargo fuzz` targets for `meowland-input` (`Parser`,
  `csi`, `mouse`), the probe parser, and `Packet::parse` once the Nix nightly
  sanitizer flags are set up.
- [ ] **CLI error output.** The Wayland bind and init error normalization is
  tested at the conversion helpers. A process-level test that starts the CLI with a
  runtime directory short enough for the control socket but too long for the Wayland
  socket would also guard the wiring and assert the cause prints once.

### Owner

- [ ] **Repo hygiene.** These are local files and a history rewrite, not code changes:
  - `docs/` is an empty directory.
  - `result` is a stale symlink in the working tree.
  - Early history contains commits named `commit`, `comit`, and `cmt`.

## Done

### Correctness

- Non-US layouts: panes forward Kitty key identities; the compositor resolves them
  through the active XKB layout (`keymap.rs`, levels 0 and 1), commits text for
  printable keys the layout cannot type, and uses Kitty associated text.
- Pointer input: typed buttons including back and forward, and horizontal scroll.
  `Modifiers` is a bitflags type with a documented Kitty conversion.
- Configures are sent only when pending state changed (`send_pending_configure`).
- All TTY writes go through one ordered output queue. Closing drains it under a
  deadline, then restores terminal modes.
- Oversized escape sequences are discarded using the VT parser's own state
  (including C1, CAN/SUB, ST split at any boundary, and the SGR mouse fast path).
  Recovery fails closed with an input error after a byte cap or a 1 s deadline,
  checked again after late reads; the deadline uses a test-controllable clock.
- Pane logs fall back to `$XDG_STATE_HOME/meowland/pane.log` when journald is
  unavailable.

### Architecture

- The compositor is split into `meowland-loop`, `meowland-panes`,
  `meowland-selection`, and `meowland-readback`, and its state is grouped into
  `Protocols`, `Input`, and `Windows`. `Attachments<P>` keeps its window index and
  due count consistent through every public mutation; there is no `&mut` access to
  indexed entries.
- `CompositorHandle` was evaluated and kept: both alternatives made `Compositor`
  generic throughout. It now lives in `meowland-loop`.
- The selection bridge tracks a per-target generation, so host reentrancy
  (detach, replacement, request errors) cannot reinstate stale exports.
- Readback phases are typed (separate acquiring and reading maps). On surface
  destruction, in-flight reads wait for their fence instead of being dropped.
- Per-crate error types; the binary uses `anyhow` with structural cause
  deduplication. Nested Wayland `BindError::Io` and `InitError::Io` are unwrapped at
  the conversion boundary.
- One validated config schema (`compositor::Settings`, serde with validated newtypes),
  re-exported from `meowland-server`; TOML via `toml`, not `config`.
- `ConnectionId` is separate from `PaneId`, and listeners carry their
  `ConnectionKind`.
- `meowland-core` holds only value types; the calloop helpers moved to
  `meowland-async`. The benchmark workload and shim moved to `meowland-bench`; the
  Python harness stays in `benchmarks/`.

### Performance

- shm commits copy only damaged rectangles, with an exact transparent-pixel count.
  DMA-BUF readbacks recycle the previous snapshot's storage.
- Pane updates are rectangles (`FrameBand { x, y, width }`, Kitty `a=f` with `x`/`y`
  offsets and `s`/`v` sizes). Rectangles at least 75% of the frame width go as full
  rows. Shared memory is chosen by changed-rectangle area.
- Run-loop passes are skipped when nothing is due (frame-due counts, earliest
  callback and activation deadlines, deferred popup cleanup).
- Shared-frame consumption polling backs off; consumed frames are copied back in
  bounded chunks between turns.
- Attachments are indexed by window. Surface owners are cached in the surface data
  map.
- Clipboard exports follow a MIME allowlist (text first, then `text/html`,
  `image/png`, `text/uri-list`). OSC 52 panes get one text type, capped at `MAX_TEXT`.
- Framing buffers shrink back to 1 MiB after large messages. `Outgoing` buffers are
  reused, and the session loop yields after a turn budget.

### Types and API

- `FrameSize` is validated during deserialization. The wire uses ID newtypes and
  enums instead of bools. Probe sizes are paired. Dead items are removed.
- Shortcuts are configurable: `[shortcuts] close_window`/`detach_pane` key strings,
  sent to panes in `HelloOk`; an empty string disables a binding.
- Magic numbers are named constants with documented sources.

### Testing and CI

- Unit and property tests for parsers, protocol framing, the async reader resuming
  after cancellation, pane policy (with an independent proptest model), composition
  (independent source-over oracle), shm snapshots, core value types, settings, and
  the selection bridge's reentrancy.
- CI installs the EGL and GBM libraries and runs `benchmarks/smoke.py`, which starts a
  server, maps a workload window, kills it, and checks a clean shutdown.
- GitHub CI and `nix flake check` both stay because they cover different
  environments.
- Benchmarks cover tall, wide, and off-origin rectangles. Each case asserts its
  transport (shared or inline) and edit geometry, and `--compare` still matches
  older saved results.
