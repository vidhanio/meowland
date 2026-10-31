# TODO

Open work comes first. Completed items are condensed under [Done](#done), each with a
note on any decision a future change should respect. See the README for the
architecture diagrams. Paths are relative to `crates/` unless stated otherwise.

Out of scope by decision: protocol versioning (the project is pre-release), pinning
the nightly toolchain, and deduplicating the systemd units (the Home Manager module
needs its own copy).

## Inventory

About 21.4k lines of Rust in 16 crates (counts include tests), plus the Python
benchmark harness and the `fuzz/` crate.

| Crate | Lines | Role |
|---|---|---|
| `meowland` | 578 | CLI (`clap` derive, `clap_complete` dynamic completion), TOML config loading, logging, `anyhow` error reporting |
| `meowland-server` | 1130 | Owns the `calloop` loop: sockets, socket activation, child processes, shutdown |
| `meowland-compositor` | 4340 | Smithay state (`Protocols`, `Input`, `Windows`), shell, seat, input injection, keymap, frame pacing |
| `meowland-loop` | 216 | `CompositorHandle` projection: register calloop sources against a projected state |
| `meowland-panes` | 1543 | Window catalog and pane attachments, generic over a `Presentation` trait (no Smithay) |
| `meowland-selection` | 840 | Wayland ↔ terminal clipboard bridge behind a `Host` trait |
| `meowland-readback` | 665 | DMA-BUF acquire fences, GPU submit and completion behind a `Host` trait |
| `meowland-render` | 3323 | CPU composition, damage, shm snapshots, EGL/GLES DMA-BUF backend |
| `meowland-protocol` | 2029 | Postcard wire types, sync and async framing, shared frames, clipboard limits |
| `meowland-terminal` | 2167 | Pane client: probe, session loop, presentation, terminal modes |
| `meowland-terminal-clipboard` | 1213 | OSC 5522 (Kitty) and OSC 52 clipboard state machines |
| `meowland-input` | 1615 | VT parsing, SGR pixel mouse, Kitty key events, oversized-control recovery |
| `meowland-kitty` | 355 | Kitty graphics encoder (zlib, base64, chunking, rectangle edits) |
| `meowland-async` | 298 | calloop-local async I/O, timers, scheduling, and signals |
| `meowland-core` | 463 | `FrameSize`, `UiScale`, `RefreshRate`, latency percentiles |
| `meowland-bench` | 615 | Benchmark workload binary and the `cargo bench` shim |

Also in the repo: `benchmarks/` (Python harness, `smoke.py`, `GUIDE.md`), `fuzz/`
(cargo-fuzz targets, outside the workspace), `share/systemd/user` (socket and service
units), `flake.nix` (crane build, Home Manager module, treefmt),
`.github/workflows/ci.yaml` (fmt, clippy, tests, docs, headless smoke test).

## Open

Items from the code reviews carry two independent severity scores out of 10, written
**[R n · M n]**: R is the reviewer agent that found the issue, M is the consolidating
agent, scored without seeing R. Items are sorted by combined score within each
section. The [state machine](#state-machines) section groups the items that an
explicit lifecycle model would rule out, and [Architecture](#architecture) proposes
larger changes that remove whole bug classes. Findings from comparing against other
projects are under [Niri comparison](#niri-comparison) and
[wlclipsync comparison](#wlclipsync-comparison).

### State machines

Each machine names its states and the TODO items it would prevent. **Prevents**
means both scorers agreed the bug is an illegal or forgotten state or transition.
**Helps** means one of them called it partial: the machine gives the right structure,
but a deadline, budget, or validation is still needed.

1. **Pane connection lifecycle** (`meowland-server`):
   `Handshaking → Attached → Draining { deadline } → Closed`. A connection that
   fails during `Handshaking` cannot reach `Attached`, so `Focus` has nothing to act
   on, and `Draining` always carries a deadline.
   - Prevents: *Skip `Focus` when sending `HelloOk` fails*.
   - Helps: *Put a deadline on the release drain*; *Bound total memory held by
     partial frames* (a reader state `Partial { since }` gives the deadline a home).
2. **Pane session lifecycle** (`meowland-terminal`):
   `Probing → Starting → Running → Closing { deadline } → Restored`. Every exit path,
   including errors, goes through `Closing`, and each state chooses which event
   sources stay live, so signals and server control messages are read even while
   output is blocked.
   - Helps: *Make startup output draining interruptible*; *Use bounded restoration
     on error paths*; *Keep reading from the server while output is stalled*;
     *Restore the terminal title* (entering `Running` saves it, `Restored` writes it
     back).
3. **Kitty paste request** (`meowland-terminal-clipboard`):
   `Idle → Refreshing { origin: Explicit | Background, deferred input } →
   OfferPublished → Released`. A deferred Ctrl-V belongs to one specific explicit
   refresh. Focus loss cancels only `Background` refreshes. Input is released only
   after the offer is published.
   - Prevents: *Hold Ctrl-V until its own paste refresh completes*; *Keep explicit
     pastes across focus loss*; *Queue the paste offer before `Unblock`*.
   - Helps: *Deliver terminal-initiated primary pastes* (the selection becomes part
     of the request state).
4. **Kitty clipboard write** (`meowland-terminal-clipboard`):
   `Writing { item, offset }` advanced once per session turn.
   - Helps: *Encode Kitty clipboard writes in bounded steps*.
5. **Clipboard export and ownership** (`meowland-selection`): an export moves
   `Reading { completed } → Complete | Truncated { completed }`, so completed
   representations survive a later failure. A terminal-sourced seat selection moves
   `Live → Revoked` when its pane detaches.
   - Prevents: *Revoke terminal-owned selections when their pane detaches*.
   - Helps: *Keep finished representations when a later one fails*.
6. **Surface snapshot freshness** (`meowland-readback`):
   `Synchronized → MissedCommit → FullRefresh → Synchronized`. A failed or
   over-budget readback cannot be followed by a damage-only update.
   - Prevents: *Force full damage after a failed snapshot*.
   - Helps: *Give each client its own DMA-BUF readback budget* (reservations tied to
     snapshot states, owned per client); *Test readback lifetimes*.
7. **Seat focus** (`meowland-compositor`): keyboard focus and selection focus change
   in one transition. Restoring held keys is a separate transition from live key
   input, so it never sends key presses to a popup that has the grab.
   - Helps: *Clear selection focus when keyboard focus is removed*; *Don't send
     synthetic key presses to popup grabs*.
8. **Window binding** (`meowland-panes`): `Bound::Drawable` versus
   `Bound::Unmapped`, so input, paste, and focus restore can only target drawable
   windows.
   - Helps: *Ignore paste into an unmapped pane window*; *Restore focus to a drawable
     window*.
9. **Activation token lifecycle** (`meowland-compositor`):
   `Issued { deadline } → Used | Expired`, owned in one place that also inserts the
   token into Smithay's store after the callback returns.
   - Helps: *Expire client-created activation tokens*.
10. **VT string and recovery states** (`meowland-input`): `Osc → Terminated |
    Cancelled` and `Paste → Overflowing { bytes, deadline } → Failed`.
    - Prevents: *Drop cancelled OSC replies*.
    - Helps: *Bound recovery from an oversized bracketed paste*.
11. **Key lifecycle** (`meowland-input`): `HeldMapped` versus `HeldAsText`, with a
    bounded set of held keys.
    - Helps: *Forward repeats for keys sent as text*; *Validate and limit held key
      identities*.
12. **XDG toplevel configure lifecycle** (`meowland-compositor`):
    `AwaitingInitialCommit → Configured → Mapped → Unmapped → AwaitingInitialCommit`,
    with request handlers returning a `ConfigureAction` (see
    [Architecture](#architecture) A5).
    - Prevents: *Restart the configure sequence after unmapping*.
    - Helps: *Reply to explicit state requests even when state is unchanged*;
      *Separate protocol mapping from snapshot availability*.

Niri-comparison items that the existing machines also cover: *Reconcile keyboard
focus when reattaching a window* and *Restrict popup grabs to the focused window*
(machine 7, seat focus); *Do not clear the terminal clipboard when an export has no
usable data* and *Synchronize an existing selection on pane attachment* (machine 5);
*Resume callbacks when a retained pane's window remaps* (machine 8). *Handle SIGHUP
as a shutdown request* is a missing transition in the server's own shutdown lifecycle.

Ownership tracking also helps with *Keep signal masks correct across overlapping
subscriptions* and *Clean up shared objects after a producer crash*. Neither is a
lifecycle bug at heart: the first needs shared mask ownership, the second needs
ownership records that survive a crash.

Not state-machine problems: arithmetic and geometry (damage clipping, viewports,
popup geometry, value120, SGR pixels), resource bounds (decoded sizes, output
budget, MIME lists), environment handling, layout mapping, and docs.

### Architecture

Larger-scope changes proposed by the reviewers to remove whole bug classes rather than
individual bugs, ranked by consolidated value against cost. **R** is the reviewer's
value/cost rating (several reviewers proposed overlapping ideas; their ratings are
listed together), **M** is the consolidator's. These are proposals to evaluate, not
agreed plans.

1. **[R 8 · M 8] One committed scene model per transaction.** Build a `SurfaceView`
   tree once per transaction: effective window geometry, root origin, buffer
   transform, scale, validated viewport, mapped state, and clipping in wide integers.
   Composition, damage, hit testing, popup placement, and source bounds all read the
   same tree instead of re-deriving geometry. Eliminates the geometry-drift class:
   *Clip surface damage before transforming it*, *Account for viewports when bounding
   source buffers*, *Use the effective window geometry for popup placement*, *Place
   the root at the window-geometry origin*, *Skip descendants of unmapped surfaces*,
   *Validate committed viewport bounds*. Cost: a substantial cutover across
   `meowland-render` and `meowland-compositor`; Smithay's `RenderElement`/
   `OutputDamageTracker` path is GPU-oriented and not a drop-in for the shm CPU
   fallback, so reuse only Smithay's surface-view helpers where buffer ownership fits.
2. **[R 9, 7 · M 8] Resource admission as owned reservations.** A per-client budget
   under a global one; framing readers, bounded deserializers, clipboard deliveries,
   and readback submissions must acquire an RAII reservation *before* allocating or
   submitting, and the reservation travels with the buffer or operation until every
   completion or cancellation path releases it. Unbounded allocation becomes
   impossible to write without a token. Eliminates: *Bound decoded collection sizes*,
   *Bound total memory held by partial frames*, *Give each client its own DMA-BUF
   readback budget*, *Prevent one client from occupying every clipboard delivery
   slot*. Cost: invasive API changes plus a fairness policy; submitted GPU work must
   stay retained until complete, so deadlines cannot simply free its buffer.
3. **[R 8, 8 · M 8] Real-client conformance harness.** A reusable test crate modelled on
   niri's `src/tests/fixture.rs`: in-process Wayland clients over socket pairs, a
   scripted terminal peer on the pane protocol, controlled commits, input, pipe
   progress, permission replies, and disconnects, recording protocol traces and
   asserting invariants. Optionally run the same XDG scenarios against headless niri
   as a differential reference. Covers most open Testing items and every lifecycle
   finding from the niri comparison. Detects rather than prevents; niri pins a
   different Smithay revision and legitimate policy differences must not count as
   failures.
4. **[R 9 · M 8] Clipboard effects as distinct semantic types.** Replace the overloaded
   empty `ClipboardData` with `SelectionUpdate::Clear | Publish(NonEmptyClipboardData)`
   and a separate export-failure result; a zero-byte payload is an item, not a clear.
   Only an explicit selection-clear event can construct `Clear`. Eliminates *Do not
   clear the terminal clipboard when an export has no usable data* and structures
   *Keep finished representations when a later one fails*. Cost: a wire change across
   `meowland-protocol`, `meowland-selection`, and `meowland-terminal-clipboard` (fine
   pre-release); partial exports still need a policy.
5. **[R 9 · M 7] Protocol obligations returned, not remembered.** Route Smithay XDG
   callbacks through a `ShellPolicy` whose request methods return an exhaustive
   `ConfigureAction` (defer until initial commit, send even if unchanged, send if
   changed, dismiss); only the adapter sends configures and it tracks obligations
   across map and unmap. Eliminates *Restart the configure sequence after unmapping*
   and *Reply to explicit state requests even when state is unchanged*. Risk: Rust
   cannot force Smithay callbacks to go through the adapter, so make direct configure
   calls private to it.
6. **[R 8 · M 7] One-shot obligation objects for clipboard requests.** A `PastePermit`
   owns the deferred input and can only be released through
   `publish_offer_then_release` or an explicit failure; a `ReadReply` owns each request
   ID and is consumed by success, denial, or cancellation. This is the typed form of
   [state machine 3](#state-machines). Eliminates *Hold Ctrl-V until its own paste
   refresh completes*, *Queue the paste offer before `Unblock`*, *Keep explicit pastes
   across focus loss*. Risk: `Drop` cannot deliver after a disconnect, so shutdown must
   resolve outstanding obligations explicitly.
7. **[R 7, 7 · M 7] Pure lifecycle cores with effects at the edges.** Extract reducers for
   the pane handshake, clipboard, readback, and frame presentation that consume
   timestamped events and return typed effects; calloop, Smithay, and GPU adapters
   execute effects and feed completions back. Tests replay traces and generate
   interleavings (proptest or stateright) against the real transition logic. Targets
   ordering and forgotten-transition bugs: *Skip `Focus` when sending `HelloOk`
   fails*, *Queue the paste offer before `Unblock`*, *Force full damage after a failed
   snapshot*, *Test readback lifetimes*, *Test frame acknowledgement*. Start with the
   pane handshake, not a compositor-wide rewrite; it makes failures testable but does
   not prove correctness by itself.
8. **[R 7, 8 · M 7] Seat authority with scoped interaction grants.** One coordinator
   derives a drawable focus target and generation, then applies keyboard, selection,
   and grab effects together. Sensitive entry points (popup grab, paste, activation)
   take short-lived `MappedWindow`/`InteractionGrant` values minted after checking
   window, seat, originating surface tree, serial, and mapping generation; unmap,
   focus change, and destruction invalidate them. Eliminates *Clear selection focus
   when keyboard focus is removed*, *Don't send synthetic key presses to popup grabs*,
   *Ignore paste into an unmapped pane window*, *Restore focus to a drawable window*,
   *Restrict popup grabs to the focused window*, *Reconcile keyboard focus when
   reattaching a window*. Grants must still be revalidated at use time.
9. **[R 6, 8 · M 7] Snapshot generations with derived damage.** Each successful surface
   update produces an immutable `SnapshotGeneration` with complete pixels and its
   mapping basis; pane damage is derived by comparing tiles against the last
   published generation (hash, then confirm bytes), and skipped generations force a
   full refresh. Eliminates *Force full damage after a failed snapshot* and *Rebuild
   shm snapshots when buffer mapping changes*. Cost: full shm copies raise memory
   bandwidth; benchmark before replacing damage-limited copies.
10. **[R 8 · M 7] Credit-based backpressure in the pane protocol.** Replace the boolean
    enqueue result with byte and frame credits plus acknowledgements; coalesce frame
    work when out of credit and keep control and shutdown traffic serviceable on a
    bounded path. Eliminates *Stop disconnecting panes for valid concurrent output*,
    *Put a deadline on the release drain*, *Keep reading from the server while output
    is stalled*. Both endpoints and the benchmark assertions migrate together; a single
    blocked byte stream still cannot be preempted, so deadlines remain.
11. **[R 7 · M 6] Separate text, key, and paste intent end to end.** Distinct protocol
    events for committed text, key transitions, and paste; the terminal classifies
    once, the compositor routes text through text-input or an explicit fallback rather
    than reverse-mapping it to a layout key, and paste is an operation instead of a
    synthetic Ctrl+V. Eliminates *Use associated text even when the key maps to the
    layout*, *Forward repeats for keys sent as text*, *Paste on layouts without a
    Latin V*. Kitty gives logical identities, not scan codes, so physical shortcuts and
    XKB group switching remain separate problems.
12. **[R 7 · M 4] Declared protocol manifest.** Derive globals and XDG capabilities from
    one table with explicit `Supported` or `Rejected` per optional operation instead of
    inherited trait defaults. Eliminates *Advertise only implemented window
    capabilities* and *Install the drag-and-drop grab*. A generic framework for a small
    protocol set may cost more than it saves; contract tests from A3 catch the same
    bugs.
13. **[R 5 · M 4] Durable compositor-owned clipboard snapshot.** Optionally capture
    bounded representations into immutable storage and publish that as the seat
    selection, so reads no longer depend on the pane or source staying alive. Helps
    *Revoke terminal-owned selections when their pane detaches* and *Keep finished
    representations when a later one fails*. A behavior change: eager capture can
    trigger terminal permission prompts and freeze dynamic data.

### Security

- [ ] **[R 7 · M 5] Give each client its own DMA-BUF readback budget.**
  `meowland-readback/src/lib.rs:86`: the global limits (32 operations, 256 MiB) stay
  reserved until a surface's transaction is applied. One client can fill them with
  synchronized subsurface commits while holding back the parent commit, which blocks
  every other client's DMA-BUF updates. Add per-client limits and share the global
  budget fairly.
- [ ] **[R 6 · M 6] Clip surface damage before transforming it.**
  `meowland-compositor/src/buffer.rs:66`: Smithay's integer `transform_rect_in`
  subtracts a client-supplied damage coordinate from the buffer size before clipping.
  On a transformed shm surface this overflows (a panic with overflow checks on, wrong
  damage without them). Clip in a wider type first.
- [ ] **[R 6 · M 6] Clear selection focus when keyboard focus is removed.**
  `meowland-compositor/src/input.rs:406`: the pinned Smithay `set_focus(None)` does not
  call `focus_changed`. The previous client therefore stays the clipboard and primary
  selection focus, and keeps receiving offers while unfocused.
- [ ] **[R 6 · M 5] Bound decoded collection sizes.**
  `meowland-protocol/src/protocol.rs:413`: the 64 MiB frame limit bounds encoded bytes,
  not decoded allocation. A pane can send millions of empty strings in
  `ClipboardOffer.mimes`, and postcard builds the whole `Vec<String>` before
  `mime_types` filters it. Deserialize into bounded collections and reject excess
  entries before allocating them.
- [ ] **[R 6 · M 4] Bound total memory held by partial frames.**
  `meowland-protocol/src/asynchronous.rs:63`: a pane can send all but the last byte of
  a 64 MiB packet and then stall. With 255 pane connections allowed, that can pin about
  16 GiB. Add a memory budget shared by all connections and a deadline for finishing
  each packet.
- [ ] **[R 5 · M 3] Serialize stale-socket recovery.** `meowland-server/src/sockets.rs:124`:
  two servers can both verify the same stale inode. One then replaces the socket, and
  the other's `remove_file` unlinks the live socket, so that server keeps running but
  nobody can reach it. Hold a lock across bind, recovery, and cleanup.
- [ ] **[R 4 · M 3] Bound recovery from an oversized bracketed paste.**
  `meowland-input/src/input.rs:403`: once a paste passes `MAX_TEXT`, the parser drops
  all input until it sees an exact end marker. There is no byte cap and no deadline, so
  an unterminated paste blocks keyboard, mouse, focus, and clipboard input forever. Use
  the same bounded, fail-closed recovery as oversized control sequences.
- [ ] **[R 4 · M 3] Validate and limit held key identities.**
  `meowland-input/src/keyboard.rs:34`: Kitty key codes from the terminal are inserted
  without checking the range or the compositor's 256-key snapshot limit. One invalid
  identity makes the compositor reject every later snapshot, and the set can grow
  without bound.

### Correctness

Compositor:

- [ ] **[R 5 · M 6] Emit 120 `value120` units per wheel detent.**
  `meowland-compositor/src/input.rs:50`: `V120_PER_DETENT` is 60, so a 120-unit detent
  from the terminal arrives as half a detent (confirmed). Set it to 120.
- [ ] **[R 6 · M 5] Make XKB layout switching work.** `meowland-compositor/src/input.rs:464`:
  `input_forward` does not update XKB key state, and modifier snapshots only set the
  modifier masks. Options such as `grp:alt_shift_toggle` therefore never switch
  between the layouts that `KeyboardSettings` accepts. Keep the reverse map in sync
  with the group.
- [ ] **[R 6 · M 4] Use associated text even when the key maps to the layout.**
  `meowland-compositor/src/input.rs:170`: associated text is only checked when reverse
  lookup fails. A mapped key carrying composed text is sent as the physical key, and
  the composed text is lost.
- [ ] **[R 5 · M 4] Paste on layouts without a Latin V.**
  `meowland-compositor/src/input.rs:486`: `paste_action` does nothing when `KEY_V` is
  not in the active layout (for example a Russian-only layout).
- [ ] **[R 5 · M 3] Ignore paste into an unmapped pane window.**
  `meowland-compositor/src/input.rs:477`: unlike `pane_input`, `paste_action` does not
  check that the window is drawable, so it can focus a hidden surface and send it
  Ctrl+V.
- [ ] **[R 4 · M 4] Don't send synthetic key presses to popup grabs.**
  `meowland-compositor/src/input.rs:314`: during a popup keyboard grab, Smithay ignores
  the focus reset, so restoring the held-key snapshot sends real presses to the popup.
- [ ] **[R 4 · M 3] Restore focus to a drawable window.**
  `meowland-compositor/src/panes.rs:58`: when the focused pane detaches,
  `first_window()` can return an unmapped window that is still bound. `focus_window`
  then gives up instead of trying the next attachment.
- [ ] **[R 4 · M 2] Expire client-created activation tokens.**
  `meowland-compositor/src/activation.rs:129`: `token_created` never updates
  `activation_deadline`. Smithay inserts the token after the callback returns, so an
  unused token can stay forever. Put launches, the token store, and the earliest
  deadline behind one activation owner (`lib.rs:103`) and add a regression test.

Server and CLI:

- [ ] **[R 6 · M 5] Skip `Focus` when sending `HelloOk` fails.**
  `meowland-server/src/lib.rs:120`: if the pane is already gone, `send_to_pane` removes
  the connection, but the handler still sends `Command::Focus`. That leaves an
  attachment with no transport that nothing will ever remove.
- [ ] **[R 6 · M 4] Stop disconnecting panes for valid concurrent output.**
  `meowland-server/src/transport.rs:154`: a 5120×2880 inline frame (~44 MB) and a
  32 MiB clipboard export together exceed the 64 MiB budget, and the pane is detached.
  Apply backpressure instead of failing the connection.
- [ ] **[R 4 · M 4] Put a deadline on the release drain.**
  `meowland-server/src/transport.rs:198`: a pane that stops reading can block
  `flush_to` forever after `release`. Its connection and reservation leak, and enough
  of them use up the pane connection limit.
- [ ] **[R 3 · M 2] Remove `WAYLAND_SOCKET` from launched children.**
  `meowland-server/src/process.rs:48`: the child inherits the caller's environment but
  not its file descriptors. A stale `WAYLAND_SOCKET` takes precedence over the new
  `WAYLAND_DISPLAY`.
- [ ] **[R 3 · M 2] Keep completion values exact.** `meowland/src/cli.rs:224`: completions
  are sanitized and cut to 256 characters, but selectors match the original string
  exactly. Keep the display text separate from the inserted value, and never truncate
  printed window IDs.

Clipboard and selection:

- [ ] **[R 6 · M 6] Hold Ctrl-V until its own paste refresh completes.**
  `meowland-terminal-clipboard/src/kitty.rs:180`: if any request is active (including
  the background MIME refresh started on attach), the input gate forwards Ctrl-V before
  the new offer arrives, so the app pastes the old selection.
- [ ] **[R 5 · M 5] Queue the paste offer before `Unblock`.**
  `meowland-terminal-clipboard/src/kitty.rs:446`: `cancel_lists()` queues `Unblock`
  ahead of the paste offer, so the shortcut is flushed against the old selection and
  then the offer is treated as a second paste.
- [ ] **[R 4 · M 5] Keep finished representations when a later one fails.**
  `meowland-selection/src/lib.rs:434`: when a later representation is too large or
  times out, the whole Kitty export is dropped, including text that was already read.
- [ ] **[R 5 · M 4] Keep explicit pastes across focus loss.**
  `meowland-terminal-clipboard/src/clipboard.rs:107`: Kitty focus handling returns
  `false` and `cancel_refresh()` cancels paste requests. A terminal permission dialog
  that takes focus therefore discards the paste. Cancel only background refreshes.
- [ ] **[R 5 · M 4] Revoke terminal-owned selections when their pane detaches.**
  `meowland-selection/src/lib.rs:204`: the seat selection backed by
  `Source::Terminal { pane, .. }` stays installed, and later pastes get no data.
- [ ] **[R 5 · M 3] Deliver terminal-initiated primary pastes.**
  `meowland-terminal/src/session/events.rs:100` and `meowland-selection/src/lib.rs:688`:
  a `loc=primary` notification is forwarded with paste intent, but paste only runs for
  the clipboard selection.
- [ ] **[R 3 · M 4] Keep a bounded prefix of large Kitty MIME lists.**
  `meowland-terminal-clipboard/src/kitty.rs:31`: a list longer than `MAX_TYPE_LIST` is
  rejected outright, which disables paste for legitimate large offers.
- [ ] **[R 3 · M 3] Export to OSC 52 only types it accepts.**
  `meowland-selection/src/lib.rs:127`: `export_types` can pick `TEXT` or `STRING`,
  which `ClipboardData::utf8_text` rejects. Filter those out or convert them to UTF-8.
- [ ] **[R 3 · M 2] Drop cancelled OSC replies.** `meowland-input/src/input.rs:443`:
  `vtparse` dispatches an OSC when CAN or SUB cancels it, so a truncated OSC 52 reply
  can replace the clipboard. Dispatch only after BEL or ST.

Terminal pane:

- [ ] **[R 6 · M 5] Make startup output draining interruptible.**
  `meowland-terminal/src/session.rs:373`: both startup `flush_output()` calls wait with
  no deadline or signal handling while in raw mode. If the terminal stalls, the pane
  ignores SIGINT, SIGTERM, and SIGHUP and stays in raw mode.
- [ ] **[R 6 · M 5] Use bounded restoration on error paths.**
  `meowland-terminal/src/terminal.rs:185`: session errors drop `TerminalGuard`, whose
  destructor writes to stdout with blocking writes, instead of calling the bounded
  `close()`.
- [ ] **[R 5 · M 4] Keep reading from the server while output is stalled.**
  `meowland-terminal/src/session.rs:177`: the server reader is disabled while output is
  pending, so a release or disconnect is never seen and shutdown never starts. Keep
  accepting control messages without accepting another frame.
- [ ] **[R 3 · M 2] Restore the terminal title.** `meowland-terminal/src/terminal.rs:83`:
  OSC 2 titles are never reverted. Save the title on entry and restore it on every exit
  path.

Input:

- [ ] **[R 5 · M 2] Keep all of the Kitty associated text.**
  `meowland-input/src/input.rs:434`: local `vtparse` 0.7 keeps at most 32 CSI
  parameters and does not report the overflow as truncation (confirmed by the
  reviewer), so text longer than about 15 code points is cut short. The existing test
  calls `csi` directly and does not hit this limit.
- [ ] **[R 4 · M 3] Forward repeats for keys sent as text.**
  `meowland-input/src/keyboard.rs:25`: repeats are always dropped. A key that falls back
  to a one-time text commit has no held key on the Wayland side, so it never repeats.
- [ ] **[R 3 · M 3] Make the SGR pixel origin terminal-specific.**
  `meowland-input/src/input.rs:619`: xterm reports mode 1016 coordinates from 1
  ([button.c](https://github.com/ThomasDickey/xterm-snapshots/blob/master/button.c)),
  Kitty from 0 ([mouse.c](https://github.com/kovidgoyal/kitty/blob/master/kitty/mouse.c)).
  Subtracting 1 everywhere would break Kitty, so choose the origin from the probed
  terminal.

Render and readback:

- [ ] **[R 6 · M 7] Force full damage after a failed snapshot.**
  `meowland-readback/src/lib.rs:344`: when the budget runs out or a readback fails, that
  commit's damage is still consumed. The next successful readback only composes the
  newer damage, so the missed region can stay stale indefinitely.
- [ ] **[R 6 · M 5] Account for viewports when bounding source buffers.**
  `meowland-render/src/geometry.rs:120`: `source_bound` ignores `wp_viewport`, so a
  valid large buffer that is cropped or downscaled gets rejected.
- [ ] **[R 5 · M 6] Use the effective window geometry for popup placement.**
  `meowland-render/src/render.rs:248`: if xdg geometry is unset, `(0, 0)` is used
  instead of the surface-tree bounds; if it is set, it is not clamped to them. Popups
  and their hit regions end up misplaced.

Async and protocol:

- [ ] **[R 3 · M 3] Keep signal masks correct across overlapping subscriptions.**
  `meowland-async/src/signals.rs:32`: dropping calloop's `Signals` always unblocks its
  signals, even ones another subscription still needs or that were blocked before
  registration.
- [ ] **[R 4 · M 2] Clean up shared objects after a producer crash.**
  `meowland-protocol/src/shared.rs:151`: a producer killed before it publishes leaves
  the object in `/dev/shm`. Reclaim abandoned objects without touching those owned by
  live producers.

Tooling:

- [ ] **[R 3 · M 2] Isolate the smoke test from `MEOWLAND_CONFIG`.**
  `benchmarks/smoke.py:125`: the inherited variable overrides the temporary
  `XDG_CONFIG_HOME`.
- [ ] **[R 2 · M 1] Isolate benchmark pane logs.** `benchmarks/session.py:59`: set
  `XDG_STATE_HOME` under the temporary root so the run does not write to the user's
  real `pane.log`.

### Code quality

- [ ] **[R 4 · M 4] Encode Kitty clipboard writes in bounded steps.**
  `meowland-terminal-clipboard/src/kitty.rs:399`: `advance()` encodes an entire 32 MiB
  transfer (over 8,000 packets) in one turn. Track the current item and offset instead.
- [ ] **[R 3 · M 2] Keep DMA-BUF import failure causes.** `meowland-render/src/dmabuf.rs:290`:
  `validate` reduces dimension, format, and EGL errors to `bool`.
- [ ] **[R 2 · M 2] Add `// SAFETY:` comments instead of suppressing the lint.**
  `meowland-readback/src/fences.rs:110` (ioctl and `OwnedFd::from_raw_fd`) and
  `meowland-server/src/process.rs:43` (`pre_exec`, reviewer scored it R 1) suppress
  `undocumented_unsafe_blocks`.
- [ ] **[R 2 · M 2] Make the Kitty output-buffer contract consistent.**
  `meowland-kitty/src/presenter.rs:77`: `present_into`, `repaint_into`, and
  `present_shared` clear the buffer, while `PresentOutcome::Encoded` says commands are
  appended.
- [ ] **[R 2 · M 1] Stop copying compression samples.** `meowland-kitty/src/encoding.rs:98`:
  feed the four 16 KiB samples straight to the encoder instead of staging them.
- [ ] **[R 2 · M 1] Avoid copying borrowed associated text.**
  `meowland-compositor/src/input.rs:193`: `key_text` always allocates a `String`.
- [ ] **[R 1 · M 1] Remove the unused clock indirection.**
  `meowland-input/src/input.rs:186`: `Clock::source` is always `Instant::now`.

### Structure

- [ ] **[R 3 · M 4] Make readback testable without EGL.** `meowland-readback/src/lib.rs:314`:
  `Host` exposes a concrete `DmabufBackend`, and the state machine owns real GPU
  readbacks. Move submission and completion behind the host boundary.
- [ ] **[R 2 · M 3] Let the session own the output queue.**
  `meowland-terminal/src/presentation.rs:142`: the presentation owns the queue for
  clipboard and terminal-mode output and exposes its `Vec`, so callers can invalidate
  the write offset. Expose append-only operations.
- [ ] **[R 2 · M 3] Define the subscribed signal list once.** `meowland-server/src/actor.rs:31`
  and `process.rs:34-39` repeat it. If they drift, children inherit a blocked signal.
- [ ] **[R 2 · M 2] Split `meowland-input/src/input.rs` (1.5k lines).** Keep `Reader`
  together with streaming recovery. Move CSI and mouse decoding, paste buffering, and
  their tests into private modules.
- [ ] **[R 2 · M 2] Split scene traversal from composition in `meowland-render/src/render.rs`.**
  Move popup and subsurface traversal and hit testing into a scene module, and leave
  the blit routines with composition.
- [ ] **[R 2 · M 2] Name the Kitty functional-key identities once.**
  `meowland-input/src/input.rs:540` and the compositor keymap repeat the same raw
  numbers. Define shared constants next to `KEY_INSERT` in `meowland-protocol`.
- [ ] **[R 2 · M 2] Keep each source's lifecycle opt-in.** `meowland-loop/src/lib.rs:77`:
  every projected source sets `NEEDS_EXTRA_LIFECYCLE_EVENTS = true`, so calloop visits
  plain `Generic` and `Timer` sources around every poll.
- [ ] **[R 1 · M 2] Remove the `meowland-kitty` module cycle.** `encoding` imports
  `Placement` and `transmit` from `presenter` (`presenter.rs:203`). Move the wire-header
  helpers into `encoding`.

### Testing

- [ ] **[R 5 · M 5] Test compositor input through the real seat and XKB keymap:**
  value120, composed text, non-Latin paste, layout switching, and keys held during a
  popup grab (`meowland-compositor/src/keymap.rs:236`; `input.rs` has no tests).
- [ ] **[R 4 · M 5] Test readback lifetimes:** acquiring and reading maps, blockers,
  reservation release, surface destruction, and failed registration
  (`meowland-readback/src/lib.rs:330`).
- [ ] **[R 3 · M 5] Test the Kitty clipboard state machine:** serialization,
  cancellation, timeout, focus, and notification ordering
  (`meowland-terminal-clipboard/src/kitty.rs:622`).
- [ ] **[R 3 · M 5] Cover pane presentation in CI.** `.github/workflows/ci.yaml:43`: the
  smoke test never attaches a pane. Add a short headless benchmark case.
- [ ] **[R 4 · M 4] Test frame acknowledgement:** damage arriving while a frame is in
  flight, and a resync forcing a full frame (`meowland-compositor/src/frame.rs:139`).
- [ ] **[R 4 · M 4] Add independent oracles for popup geometry and buffer transforms,**
  including fractional viewport crops. `meowland-render/tests/incremental.rs:466`
  compares the renderer against itself.
- [ ] **[R 4 · M 4] Test terminal restoration under backpressure** with a PTY or a writer
  that can be stalled (`meowland-terminal/src/session.rs:464`).
- [ ] **[R 4 · M 4] Test partial writes and acknowledgement ordering** in `Output` and
  `PanePresentation` (`meowland-terminal/src/presentation.rs:350`).
- [ ] **[R 3 · M 4] Test pane handshake and teardown:** a pane that closes right after
  `Hello`, and a release with stalled output (`meowland-server/src/lib.rs:117`).
- [ ] **[R 3 · M 4] Test completed and failed clipboard transfers** through real pipes:
  MIME filtering, size limits, and timeouts (`meowland-selection/src/lib.rs:803`).
- [ ] **[R 3 · M 3] Test `Keyboard`:** held-key snapshots, intercepted shortcuts, focus
  entry, and reset (`meowland-input/src/keyboard.rs:23`).
- [ ] **[R 3 · M 3] Require zero inline uploads in shared benchmark cases.**
  `benchmarks/run.py:226` only checks that at least one upload was shared.
- [ ] **[R 3 · M 3] Test Kitty encoding:** raw versus zlib choice, 4096-byte chunk
  boundaries, and partial edits rebuilt independently
  (`meowland-kitty/src/encoding.rs:45`).
- [ ] **[R 3 · M 3] Drain queued events before returning an error in the
  split-invariance fuzzer** (`fuzz/fuzz_targets/input_parser.rs:40`; the input reviewer
  scored it R 2). Otherwise it reports failures caused by the harness, not the parser.
- [ ] **[R 2 · M 2] Expect the recovery-limit error in the mouse fuzzer.**
  `fuzz/fuzz_targets/input_mouse.rs:35`: the `expect` turns a designed rejection into a
  crash.
- [ ] **[R 2 · M 2] Test `meowland-loop` projection** (dispatch, lifecycle hooks,
  removal) and `meowland-async` timer cancellation and signal masks.
- [ ] **[R 2 · M 2] Fix `focus_ignores_windows_that_are_not_drawable`.**
  `meowland-panes/tests/catalog.rs:112`: start with a focused window so the test can
  tell "ignored" apart from "cleared".
- [ ] **[R 2 · M 2] Make `server_errors.rs` deterministic.**
  `meowland/tests/server_errors.rs:81` spawns threads until the server gets a
  five-digit PID.

### Docs

- [ ] **[R 2 · M 2] Document that `Presentation::due` must stay stable between mutations**
  (`meowland-panes/src/attachment.rs:35`).
- [ ] **[R 2 · M 2] Make the undrawable-focus docs, test name, and implementation agree**
  (`meowland-panes/src/catalog.rs:125`).
- [ ] **[R 2 · M 2] Document that the single-pixel constructor takes premultiplied RGBA**
  (`meowland-render/src/snapshot.rs:38`).
- [ ] **[R 1 · M 1] Document the renderer's shm-only fallback**
  (`meowland-compositor/src/runtime.rs:41`).
- [ ] **[R 1 · M 1] Describe changed rows as a bounding range**
  (`meowland-render/src/render.rs:60`).
- [ ] **[R 1 · M 1] README frame diagram:** inline compression is optional, and shared
  frames use `t=s` (`README.md:159`; needs your OK under AGENTS.md).

### Niri comparison

Found by comparing against [niri](https://github.com/YaLTeR/niri/tree/8e7b574) at
commit `8e7b574`; niri paths are relative to its repository. **SM** marks whether an
explicit state machine would prevent the bug (yes, partial, or no).

Correctness:

- [ ] **[R 5 · M 7] Place the root at the window-geometry origin.** SM: no.
  `meowland-render/src/render.rs:231` renders the root surface at `(0, 0)` while the
  pane is sized to the xdg window geometry, so client-side decoration and shadow
  margins (GTK, libadwaita) shift content and clip the bottom and right edges. Niri
  subtracts the geometry origin when rendering and compensates input
  (`src/window/mapped.rs:637,661`).
- [ ] **[R 7 · M 6] Restart the configure sequence after unmapping.** SM: yes.
  `meowland-compositor/src/buffer.rs:145` sends the initial configure only at toplevel
  creation; after a null-buffer unmap, `send_pending_configure` sees no change and
  sends nothing, so a remapping client can wait forever. Niri returns the toplevel to
  an initial-configure state (`src/handlers/compositor.rs:251-257`).
- [ ] **[R 6 · M 6] Do not clear the terminal clipboard when an export has no usable
  data.** SM: partial. `meowland-selection/src/lib.rs:360`: a source offering only
  non-allowlisted types, or whose reads all fail, produces an empty `ClipboardData`,
  which the terminal treats as a clear.
- [ ] **[R 6 · M 5] Skip descendants of unmapped surfaces.** SM: partial.
  `meowland-render/src/render.rs:296` always descends; a buffer-less intermediate
  subsurface leaves its children visible and hit-testable. Niri stops at the unmapped
  ancestor (`src/render_helpers/surface.rs:103`).
- [ ] **[R 5 · M 5] Install the drag-and-drop grab.** SM: partial.
  `meowland-compositor/src/seat.rs:250` uses Smithay's default, which cancels the
  source, so drags fail even within one window. Niri installs `DnDGrab`
  (`src/handlers/mod.rs:319`).
- [ ] **[R 5 · M 5] Reconcile keyboard focus when reattaching a window.** SM: yes.
  `meowland-compositor/src/seat.rs:49`: detaching the keyboard-owning pane clears
  Smithay focus while the catalog still names the window, so reattaching returns early
  without a keyboard enter. Niri derives focus centrally (`src/niri.rs:1314`).
- [ ] **[R 5 · M 5] Separate protocol mapping from snapshot availability.** SM: partial.
  `meowland-compositor/src/window.rs:44` treats "has a snapshot" as "mapped", so a
  window whose first readback fails is never announced. Track mapping from committed
  state and represent missing pixels explicitly.
- [ ] **[R 5 · M 5] Allow visible portions of oversized surfaces.** SM: no.
  `meowland-compositor/src/buffer.rs:166` rejects any buffer larger than twice the
  pane, so a window whose minimum size exceeds a small pane never appears. Keep a
  global limit but crop instead.
- [ ] **[R 6 · M 4] Rebuild shm snapshots when the buffer mapping changes.** SM: partial.
  `meowland-render/src/snapshot.rs:123` reuses snapshots on matching size, format, and
  stride but ignores transform, scale, and viewport, so a damage-only patch after a
  transform change leaves stale pixels.
- [ ] **[R 6 · M 4] Move application spawning off the compositor loop.** SM: no.
  `meowland-server/src/control.rs:29` calls `Command::spawn` while dispatching, so a
  slow executable or working directory stalls everything. Niri spawns on a thread
  (`src/utils/spawning.rs:65`).
- [ ] **[R 5 · M 4] Avoid following an unknown app ID.** SM: partial.
  `meowland-compositor/src/panes.rs:119` turns an empty app ID into
  `Selector::AppId("")`, so the pane can follow any unrelated window without an app ID.
  Keep the target unresolved until an identity exists, as niri uses `Option`.
- [ ] **[R 5 · M 4] Synchronize an existing selection on pane attachment.** SM: partial.
  `meowland-selection/src/lib.rs:266`: a selection made while no pane was eligible is
  never exported, and a later terminal background offer can replace it with older
  data. Define attach-time reconciliation before accepting background refreshes.
- [ ] **[R 5 · M 4] Reply to explicit state requests even when state is unchanged.**
  SM: partial. `meowland-compositor/src/shell.rs:158`: refused maximize and repeated
  fullscreen requests get no configure. Niri forces one
  (`src/handlers/xdg_shell.rs:469`).
- [ ] **[R 5 · M 3] Restrict popup grabs to the focused window.** SM: partial.
  `meowland-compositor/src/input.rs:368` accepts any surface of the same client; check
  the popup's root against the focused window as niri does
  (`src/handlers/xdg_shell.rs:404`).
- [ ] **[R 5 · M 3] Prevent one client from occupying every clipboard delivery slot.**
  SM: partial. `meowland-selection/src/lib.rs:499`: 16 stalled reads from one client
  make everyone else's reads return EOF for up to 60 s. Add per-requester limits.
- [ ] **[R 5 · M 3] Handle descriptor exhaustion without stopping the compositor.**
  SM: partial. `meowland-server/src/actor.rs:194` shuts down on `EMFILE`/`ENFILE`. Niri
  raises the soft `RLIMIT_NOFILE` (`src/utils/spawning.rs:25`); also treat exhaustion as
  a temporary admission failure.
- [ ] **[R 5 · M 3] Handle SIGHUP as a shutdown request.** SM: yes.
  `meowland-server/src/actor.rs:31,64` subscribes to SIGHUP and then ignores it. Niri
  handles it with SIGINT and SIGTERM (`src/utils/signals.rs:45`).
- [ ] **[R 4 · M 3] Reconfigure reactive popups when their constraints change.** SM: no.
  `meowland-compositor/src/panes.rs:72` places popups only on creation or reposition,
  so resizing a pane leaves reactive popups constrained to the old size
  (`src/handlers/xdg_shell.rs:1413`).
- [ ] **[R 4 · M 3] Filter for supported types before applying the MIME count limit.**
  SM: no. `meowland-selection/src/lib.rs:127` keeps the first 64 types before checking
  the allowlist, so `text/plain` after 64 unrelated types is lost.
- [ ] **[R 3 · M 3] Advertise only implemented window capabilities.** SM: no.
  `meowland-compositor/src/lib.rs:319` uses `XdgShellState::new`, which advertises
  maximize, minimize, fullscreen, and window menu. Use `new_with_capabilities` like
  niri (`src/niri.rs:2530`).
- [ ] **[R 4 · M 2] Reply after a control client half-closes its write side.** SM:
  partial. `meowland-server/src/transport.rs:58` races the EOF against the reply.
  Niri supports `shutdown(SHUT_WR)` clients (`niri-ipc/src/lib.rs:25`).
- [ ] **[R 3 · M 2] Validate committed viewport bounds.** SM: no.
  `meowland-render/src/geometry.rs:101` never calls Smithay's `ensure_viewport_valid`,
  so out-of-buffer sources sample clamped edges instead of raising the protocol error.
- [ ] **[R 3 · M 2] Resume callbacks when a retained pane's window remaps.** SM: partial.
  `meowland-compositor/src/window.rs:55` keeps the hidden one-second callback deadline
  after remap, delaying the next callback by up to a second.
- [ ] **[R 3 · M 2] Reject overlapping shortcut bindings.** SM: no.
  `meowland-compositor/src/settings.rs:88`: the same key for close and detach makes
  detach unreachable. Niri reports conflicting binds (`niri-config/src/binds.rs:924`).

Simplification:

- [ ] **[R 2 · M 1] Share local paste bytes across MIME aliases and deliveries.**
  `meowland-selection/src/lib.rs:507`, `meowland-protocol/src/clipboard.rs:59`: two
  identical vectors per paste, copied again per read. Store one `Arc<[u8]>`.

Testing (all feed [Architecture](#architecture) A3):

- [ ] **[R 5 · M 5] Test the XDG lifecycle through a real client fixture:** initial
  configure, unmap and remap, unchanged-state requests, reactive popup resize, grabs
  across two toplevels (niri `src/tests/window_opening.rs:39`).
- [ ] **[R 4 · M 4] Test attachment and activation through a compositor fixture:**
  detach and reattach keyboard enter, following a missing or late app ID.
- [ ] **[R 4 · M 4] Test selection lifecycle through real Wayland clients:** offer
  dispatch, focus changes, source destruction, data-control clients, unsupported offers.
- [ ] **[R 3 · M 2] Test control-socket half-close and SIGHUP shutdown.**

### wlclipsync comparison

[wlclipsync](https://codeberg.org/gms/wlclipsync) (commit `6e51b0e`, one 664-line C
file) syncs selections between two Wayland compositors as an ext-data-control client.
It forwards each offer's MIME list to a new source on the other side without copying
payload, passes the requester's fd straight to `offer.receive` on paste, and breaks
echo loops with a marker MIME type (`strange/loop`). The selection reviewer checked
each lesson against meowland and the [OSC 5522 spec](https://sw.kovidgoyal.net/kitty/clipboard/);
R is that reviewer's score, M the consolidator's.

- [ ] **[R 8 · M 7] Prototype a direct local bridge to the host compositor.** SM:
  partial. When the pane runs in a terminal on a local Wayland session, it inherits the
  host `WAYLAND_DISPLAY` and could bridge selections wlclipsync-style between the host
  and meowland, which already exposes ext- and wlr-data-control
  (`meowland-compositor/src/lib.rs:228-235`). That path is lossless (every MIME type),
  lazy, passes fds through with no bridge-side copies, needs no size caps, and avoids
  terminal permission prompts; OSC 5522 and OSC 52 stay as the remote (SSH) fallback.
  The pane should own it, because the server may run in a different session. It needs
  arbitration between multiple panes, must not echo through the OSC path at the same
  time, and must tear down cleanly. Zero-code experiment today, if the host supports
  ext-data-control: `wlclipsync -d <meowland socket> -b <host socket>`.
- [ ] **[R 6 · M 5] Restrict data-control to trusted clients.** SM: no. Both
  data-control globals use the filter `|_| true` (`meowland-compositor/src/lib.rs:228-235`),
  so any client in meowland can read every clipboard and primary selection without
  focus. Restrict the globals to an allowlist (clipboard managers, the bridge above), or
  document the trust model.
- [ ] **[R 5 · M 5] Tag Kitty exports with a per-export marker MIME type.** SM: partial.
  OSC 5522 accepts arbitrary MIME types on write and lists them back, so writing a
  unique type such as `application/x-meowland-export-<generation>` lets a later
  background refresh or paste notification recognise that the terminal still holds
  meowland's own export, instead of replacing the seat selection with a terminal-backed
  copy of the same data. This is only a best-effort hint: the marker can be stripped,
  copied, or spoofed, and OSC 52 has no channel for it. It does not replace
  attach-time reconciliation (*Synchronize an existing selection on pane attachment*)
  or detach revocation (*Revoke terminal-owned selections when their pane detaches*).
- [ ] **[R 5 · M 3] Send identical representations once with Kitty `type=walias`.**
  SM: no. The text MIME aliases are currently written as separate copies; OSC 5522
  can alias a type to one already sent, when the bytes are known to be identical.

Not adopted:

- **Lazy export to the terminal.** OSC 5522 makes the application send bytes during
  `type=write`/`wdata`; the terminal cannot pull them on paste, so Wayland-to-terminal
  exports must stay eager and bounded (R 1). Terminal-to-Wayland reads are already
  on demand.
- **fd passthrough for terminal paths.** Smithay already pipes Wayland-to-Wayland
  transfers directly; terminal replies arrive as framed bytes, so only the local bridge
  above can pass fds. Chunked streaming is the alternative for terminal transports.
- **wlclipsync's error handling.** It calls `exit(1)` on failures, has no bounds, does
  not propagate a selection clear, and despite its comment drops MIME types that arrive
  after the selection event (KWin sends them late). A meowland bridge needs tests for
  late offers and clears, and bounded failure handling.

### Performance

- [ ] **Restrict DMA-BUF readback to the damage.** The GPU blit and
  `copy_framebuffer` still read the whole buffer. Smithay's renderer has no
  per-rectangle mapping export, so this needs a separate target texture and copy per
  damaged region.

### Owner

- [ ] **Repo hygiene.** These are local files, not code changes:
  - `docs/` is an empty directory.
  - `result` is a stale symlink in the working tree.

## Done

### Correctness

- Reusable shared-frame slots (`t=f` two-slot ring) were dropped: safe reuse needed
  never-reused reply identities across pane processes (Kitty `a=q` and `a=d` cannot
  act as barriers), and the gain was ~0.45 ms of a ~4.6 ms full-1080p frame. Panes
  keep one-shot `t=s` objects, then inline RGB.
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
- The niri comparison (2026-10-31) found no case for merging crates or replacing
  structure: the calloop helpers and postcard framing are needed for cancellation-safe
  partial frames, binary payloads, and bounded writes; `meowland-panes`,
  `CompositorHandle`, and the selection bridge's `Host` trait and generations stay.
  Niri's GPU `RenderElement` pipeline is not a safe replacement, because meowland needs
  the shm CPU fallback and early buffer release.
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
- `meowland-render/tests/incremental.rs` drives `Renderer::update` with real
  `wl_surface` commits (subsurfaces, buffer scale and transform, viewports,
  desynchronized commits, hidden damage) at three pane scales, checking that the kept
  pane and the reported rectangles both match full recomposition. Popups are not
  covered; they share the per-surface path.
- `fuzz/` has cargo-fuzz targets for the input parser (split invariance), `csi`,
  `mouse`, the probe parser, and clipboard packets, run through a `fuzzing` feature in
  `meowland-input` and `meowland-terminal`. Run with
  `nix shell nixpkgs#cargo-fuzz -c cargo fuzz run <target>`. They are not in CI.
- `meowland/tests/server_errors.rs` starts `meowland server` with a runtime directory
  that fits the control socket but not the Wayland socket, and asserts that the cause
  prints exactly once.
