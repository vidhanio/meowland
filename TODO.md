# TODO

Inventory and backlog from a full read of the workspace (about 13.4k lines of Rust in
10 crates, plus about 1.4k lines of Python benchmarks). See the README for the
architecture diagrams. Items are ordered by impact within each section. File paths are
relative to `crates/` unless stated otherwise.

## Inventory

| Crate | Lines | Role |
|---|---|---|
| `meowland` | 860 | CLI (`usage-rs`), config loading, logging, benchmark workload bin |
| `meowland-server` | 1068 | Owns the `calloop` loop: sockets, socket activation, child processes, shutdown |
| `meowland-compositor` | 4557 | Smithay state: shell, seat, input injection, panes, clipboard bridge, DMA-BUF readback, frame pacing |
| `meowland-render` | 1421 | CPU compositor (RGBA to RGB, viewport, scale), damage, EGL/GLES DMA-BUF backend |
| `meowland-protocol` | 1103 | Postcard wire types, sync and async framing, shared-memory frames, clipboard limits |
| `meowland-terminal` | 1593 | Pane client: probe, session select loop, presentation, terminal modes |
| `meowland-terminal-clipboard` | 973 | OSC 5522 (Kitty) and OSC 52 clipboard state machines |
| `meowland-input` | 681 | VT parser wrapper, SGR pixel mouse, Kitty keyboard to evdev |
| `meowland-kitty` | 331 | Kitty graphics encoder (zlib, base64, chunking, in-place frame edits) |
| `meowland-core` | 628 | Error enum, `FrameSize`, `UiScale`, `RefreshRate`, latency stats, calloop async helpers |

Also in the repo: `benchmarks/*.py` (process-level harness), `share/systemd/user`
(socket and service units), `flake.nix` (crane build, Home Manager module, treefmt),
`.github/workflows/ci.yaml`.

There are **no tests** (`#[test]` appears 0 times). CI and `nix flake check` run
`cargo test` against an empty suite. Clippy is clean with the pedantic and nursery lint
groups.

---

Out of scope by decision: protocol versioning (the project is pre-release), pinning
nightly, and deduplicating the systemd units (Home Manager needs its own copy).

## 1. Correctness bugs

- [ ] **Non-US keyboard layouts send the wrong keys.** `meowland-input/src/keyboard.rs`
  maps Kitty *logical* key codes to evdev codes with a hard-coded US table
  (`char_code`). The server then looks those evdev codes up in the *configured* XKB
  keymap (`keyboard.layout`). With `layout = "de"`, pressing `y` sends evdev 21, which
  the German keymap turns into `z`. Possible fixes:
  - Request `REPORT_ALTERNATE_KEYS` and translate the base-layout key.
  - Build the reverse map from the configured keymap with xkbcommon on the server.
  - Force the server keymap to `us` and forward text separately.
  - At minimum, document or reject non-US layouts in `KeyboardConfig`.
- [ ] **Extra mouse buttons are treated as motion.** In `meowland-input/src/input.rs`
  `mouse()`, SGR codes 128–131 (buttons 8–11, back and forward) hit the
  `code & (32 | 128) != 0` branch and become plain motion.
- [ ] **Horizontal scroll is dropped.** Wheel codes 66 and 67 return `None`, and
  `Input::Pointer` can only express `scroll: i16` vertically. Add a horizontal axis to
  the protocol and handle `Axis::Horizontal` in `meowland-compositor/src/input.rs`.
- [ ] **Configures are sent on every state touch.**
  `meowland-compositor/src/commands.rs` `apply_window_state` always calls
  `send_configure()`. It runs from `focus_window` (twice per focus change),
  `bind_pane_window`, `resize_pane`, `remove_pane`, `title_changed` (through
  `resolve_pending_panes`), and so on. Each redundant configure makes clients
  re-layout and re-render. Use `send_pending_configure()`, which only sends when
  pending state differs from the last configure.
- [ ] **Mixed sync and async writers to the TTY.** `TerminalGuard`
  (`meowland-terminal/src/terminal.rs`) writes mode switches with blocking
  `io::stdout()`. Frames go through a separate nonblocking descriptor
  (`output::open`). A restore can interleave with a half-drained frame. Route mode
  changes through the same `Output` queue, or drain it before `restore()`.
- [ ] **Probe query duplicates its ID constant.** `FRAME_EDIT_QUERY` in
  `meowland-terminal/src/probe.rs` hard-codes `i=30` while `FRAME_EDIT_ID = 30` is a
  separate constant. Build the query from the constant, as `query_shared` already does.
- [x] **Pane-client logs disappear when journald is unavailable.** `init_logging`
  (`meowland/src/main.rs`) silently installs nothing if `tracing_journald::layer()`
  fails. Fall back to a file in `$XDG_STATE_HOME`. stderr cannot be used because the
  terminal is in raw mode or the alternate screen.

## 2. Architecture

- [ ] **Split `meowland-compositor`** (4.5k lines, 4.5× the AGENTS.md guideline).
  Natural seams:
  - `readback.rs` + `readback/fences.rs` (about 600 lines): DMA-BUF acquire, fence
    export, and the GPU submit/complete state machine. Move it next to
    `meowland-render::dmabuf` or into a `meowland-readback` crate.
  - `clipboard.rs` (550 lines): the Wayland ↔ terminal selection bridge. It is an
    independent state machine with its own sources and timers.
  - `catalog.rs` + `attachment.rs`: pure policy that never touches Wayland. As a
    separate crate it could be unit-tested without Smithay.
- [ ] **Break up the `Compositor` god struct.** It has about 45 fields, and every
  submodule reaches into all of them through `impl State`. Group them into
  sub-structs (`Protocols`, `Seat`, `Windows`, `Panes`, `Selection`), so borrows split
  cleanly and the `keyboard.take() … self.keyboard = Some(keyboard)` dance (repeated
  about 12 times) goes away. Drop the `use Compositor as State` alias: two names for
  one type makes grepping harder.
- [ ] **Replace the `CompositorHandle` projection machinery.**
  `meowland-compositor/src/event_loop.rs` adds a custom `Source` trait object,
  `Projected`, and `Rc<dyn Fn>` boxing (about 160 lines) so the compositor can register
  sources on a loop whose data is `Server`. Simpler options: make `Compositor` the loop
  data and keep server state inside it, or make registration generic over
  `D: AsMut<Compositor>`.
- [ ] **The run loop does a full pass on every wakeup.** `actor::run` calls
  `wayland.dispatch`, which runs `frame_callbacks` (all windows), `dispatch_frames`
  (all panes), `expire_activation_tokens`, and `refresh_popup_grab`, after *any* event,
  including a single control request. The duplicated display fd is registered with a
  no-op callback just to force wakeups. Track dirty and due state, and only walk the
  collections when something changed.
- [ ] **`meowland-core` is a grab bag.** Errors, pixel geometry, UI scale, refresh rate,
  latency stats, calloop async helpers, and signals have nothing in common. Move
  `asynchronous` and `signals` into a `meowland-async` crate (or into
  protocol/terminal), and keep `core` for value types.
- [ ] **Replace the crate-wide `Error` enum** (`meowland-core/src/error.rs`). It mixes
  terminal capabilities (`PixelMouseUnsupported`, `KeyboardUnsupported`), CLI response
  handling (`UnexpectedWindowListResponse`), and server startup. Give each library
  crate its own error type. The `meowland` binary should use `anyhow`-style context.
- [x] **Stop using `io::Error` as the validation error.** `UiScale::from_str`,
  `RefreshRate::try_from`, and `Config::into_settings` return
  `io::ErrorKind::InvalidInput`. Use dedicated `thiserror` types such as
  `InvalidScale` and `InvalidRefreshRate`.
- [x] **Remove the duplicate config schema.** `meowland-server::Config`,
  `OutputConfig`, and `KeyboardConfig` mirror `meowland-compositor::Settings` and
  `KeyboardSettings` field by field, and repeat-rate validation exists in both
  `configuration.rs` and `runtime.rs`. Make `Settings` deserialize directly (validated
  newtypes with `TryFrom` and `#[serde(try_from)]`) and validate in one place.
  - `self.output.scale.to_string().parse()` converts a float to a string and back to
    build a `UiScale`. Add `UiScale::try_from(f64)`.
- [x] **Separate connection IDs from pane IDs.** `Server::accepted` assigns
  `PaneId(next_connection)` to *control* connections too. Introduce `ConnectionId` and
  create a `PaneId` only after a successful `Hello`.
- [x] **Listener capacity depends on array order.** `update_listeners` computes
  capacity with `usize::from(index == 1)`, while `accepted` uses
  `ConnectionKind::Pane`. Store the `ConnectionKind` in `Listener`.
- [ ] **Encode readback phases in types.** `readback.rs` `submit` and `complete`
  `unreachable!()` on the wrong `Phase`. Keep acquiring and reading operations in
  separate maps, or pass the phase payload to the callback directly.
- [ ] **Move benchmarks out of the shipping crate.** The `benchmarks` feature,
  `[[bin]] meowland-workload`, `[[bench]] suite`, and the optional
  `wayland-client`/`wayland-protocols`/`rustix` dependencies make
  `crates/meowland/Cargo.toml` mostly about benchmarks. Create a
  `crates/meowland-bench` crate (`publish = false`) for the workload, the shim, and
  `benchmarks/*.py`.

## 3. Performance

- [ ] **shm commits copy the whole buffer.** `Snapshot::copy_shm` copies and swizzles
  the full buffer on every commit, even when the damage is a few rows. At 4K that is
  33 MB per commit. Copy only the buffer-damage rectangles into the retained snapshot.
  The previous snapshot is already passed in for reuse.
- [ ] **DMA-BUF readback allocates and reads the whole buffer every frame.**
  `DmabufBackend::complete` allocates `vec![0; needed]` and runs a full alpha pass
  each time, and `readback()` blits the whole texture. Reuse the previous snapshot
  buffer, and restrict the blit and `copy_framebuffer` to the damage.
- [ ] **Updates are sent as row bands, not rectangles.** `Renderer::update` computes a
  column range but only reports changed *rows*. `FrameBand` and the Kitty `a=f` edit
  then send full-width rows. A blinking cursor costs `width × 16 × 3` bytes before
  zlib. Carry `x` and `width` in `FrameBand` and use Kitty's `x=`/`w=` edit keys.
- [ ] **Polling for shared-frame consumption.** `presentation/shared.rs` calls
  `shm_open` every 1 ms for up to 250 ms to see whether the terminal unlinked the
  object (`SharedFrame::exists`), and each poll allocates the name string
  (`SharedFrame::name()`). Cache the name, and back off exponentially or ack
  optimistically.
- [ ] **A shared-memory frame costs a full read on the next band.** After a shared
  frame, the next inline band forces `SharedPresentation::into_band` to read the whole
  frame back from shm. Read it lazily in the background, or keep the server on shared
  transport for the following update.
- [ ] **A new shm object for every frame.** `SharedObject::create` costs a `shm_open`,
  page faults for every page, and an unlink per frame, because the terminal unlinks
  after reading. Measure against `t=f` with a reusable file, or a small ring of
  objects.
- [ ] **O(windows × panes) scans in hot paths.** `Attachments::visible`, `showing`,
  `owner`, and `first_window` scan every pane. They are called per window in
  `frame_callbacks`, per commit, and on every focus change. Keep a
  `HashMap<WindowId, SmallVec<PaneId>>` index.
- [ ] **`window_for_surface` runs three times per commit.**
  `CompositorHandler::commit` calls it three times, and each call walks up to 64
  parents. Compute it once and cache the owner in the surface `data_map` instead of
  `SurfaceCache::owners`.
- [ ] **Clipboard export reads every MIME type eagerly.**
  `meowland-compositor/src/clipboard.rs` reads every representation (up to 64 types and
  32 MiB) from the app on every selection change, so copying an image in a browser
  pulls every format. Prioritise text and image types and cap the total. The OSC 5522
  write does need the data up front, but not all of it.
- [ ] **Framing buffers never shrink.** `protocol::send`/`recv` use a thread-local
  `SCRATCH`, and `asynchronous::Reader::body` keeps its high-water mark (up to
  64 MiB). Shrink after oversized messages.
- [x] **One `Outgoing` per message.** `pane_writer` allocates a fresh `Outgoing` for
  every message (`transport.rs`). Reuse one.
- [ ] **`yield_now()` on every session iteration.** `TerminalSession::run` yields at the
  end of every loop iteration, on top of the per-chunk yields in `Reader::read` and
  `Outgoing::flush_to`. That is an extra executor round trip per event. Yield only
  after an event budget is spent.

## 4. Type and API design

These follow the `rust-best-practices` skill.

- [ ] **Invalid pointer states are representable.** `Input::Pointer { button:
  Option<u8>, pressed: bool }`: `pressed` means nothing when `button` is `None`. Use
  `button: Option<(Button, ButtonState)>` with a `Button` enum instead of raw
  `0/1/2/_`.
- [ ] **Modifiers are a raw `u8`.** `protocol::modifiers` is a `u8` with loose consts.
  Use a `bitflags` `Modifiers` type. The crate already decodes Kitty's bit layout by
  hand in `input.rs` `csi()` (`(bits & !6) | …`), which needs a comment or a named
  conversion.
- [ ] **Validate dimensions at deserialization.** `Hello { width, height }` and
  `PaneAction::Resize` carry raw `u32`s that the server validates in three places
  (`transport.rs`, `commands.rs` twice). Send `FrameSize` and validate during
  deserialization, as `FrameBand` and `SharedFrame` already do.
- [ ] **Use ID newtypes on the wire.** `WindowInfo::id`, `Show::Id`, and the clipboard
  `request` and `offer` IDs are bare `u64`. Use `WindowId`, `RequestId`, and `OfferId`.
  `PaneId(pub u64)` and `WindowId(pub u64)` expose their fields.
- [ ] **Bool fields in the protocol.** `ClipboardOffer.paste`, `Input::Key.pressed`,
  and `WindowInfo.active` should be enums.
- [ ] **Pair related probe fields.** `ProbeInfo` stores `pixel_width`/`pixel_height`
  and `cell_width`/`cell_height` as independent `Option`s. Use
  `Option<(u32, u32)>`, or reuse `FrameSize`.
- [ ] **Remove dead or over-public items.**
  - `Command::Shutdown` is never constructed.
  - `ControlRequest::Ping` is never sent by the CLI.
  - `terminal_clipboard::Clipboard::cancel_refresh` has no external callers.
  - `render::popup_origin` and `Paths::in_runtime_dir` should be `pub(crate)` or
    private.
- [x] **Duplicate completers.** `meowland/src/cli.rs` `attach_targets` and
  `window_targets` are identical.
- [x] **Logging setup sits in the wrong module.** `cli::start` calls back into
  `crate::init_logging`, and `main.rs` owns a `LogDestination` enum only `cli` uses.
  Move logging into its own module and call it from `main`.
- [x] **The `config` crate is overkill for one TOML file.**
  `meowland/src/configuration.rs` reads the file itself and then routes it through
  `config::Config::builder()`. `toml::from_str` does the same with a much smaller
  dependency tree.
- [ ] **Name the magic numbers.** Examples: 256 deferred inputs (`input_gate.rs`), 16
  deliveries (`clipboard.rs`), 64 events per turn, 4096/4095 chunk sizes
  (`terminal-clipboard/src/kitty.rs`), `SCROLL_FACTOR`, `15.0 / 120.0`, and button
  codes `0x110…0x113` (use `BTN_LEFT` and friends). Give each a named constant with a
  short reason.
- [ ] **Hard-coded Alt+Q / Alt+W shortcuts.** They are fixed in
  `meowland-protocol/src/protocol.rs` and silently steal those keys from applications.
  Make them configurable, and allow disabling them.

## 5. Testing

AGENTS.md says not to add tests unless asked. This section lists where tests would pay
off most, for when that is requested.

- [ ] Pure parsers of untrusted input: `input.rs` `csi()` and `mouse()`, `Parser::push`
  (split escape sequences, signed coordinates), `probe.rs` `ProbeParser`, and
  `terminal-clipboard` `Packet::parse`. These are good targets for property tests and
  `cargo fuzz`.
- [ ] `keyboard::key_code` round-trips, `FrameSize::rgb_band` edge cases, and
  `protocol` framing (oversize, trailing bytes, cancellation-safe `Reader`).
- [ ] `catalog.rs` and `attachment.rs` selection and follow policy, which become
  testable without Wayland once split out (see §2).
- [ ] `render.rs` composition: blending, viewport, transforms, and damage-limited
  recomposition matching full recomposition.

## 6. Tooling, CI, Nix

- [ ] **CI duplicates the flake checks.** The GitHub `check` job and `nix flake check`
  both run clippy, fmt, docs, and tests with different toolchains. Keep one source of
  truth, probably the flake.
- [ ] **CI library setup is incomplete.** The CI `apt-get` installs only
  `libxkbcommon-dev`, while Nix also needs `libglvnd` and `libgbm`. The job works only
  because it never links the EGL paths at runtime.
- [ ] **No headless smoke test.** CI only runs `meowland --help`. A test that starts
  `meowland server` in a temporary `XDG_RUNTIME_DIR`, runs the benchmark workload, and
  checks `meowland list` would catch most integration breakage.
- [ ] **Repo hygiene.**
  - `docs/` is an empty directory.
  - `result` is a stale symlink in the working tree.
  - Recent history contains commits named `commit`, `comit`, and `cmt`. Squash them or
    use descriptive messages.
