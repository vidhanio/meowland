# INTENT.md — meowland

A handoff document. It says what meowland is, what it must do, what was learned
building it, and which constraints are the outside world's rather than ours. It
deliberately does **not** prescribe an implementation: the current one is only
one way to hold these invariants, and this file exists so a different one can be
built without rediscovering the findings. Where a decision is ours rather than
the world's, it is marked as such, with the reason it was made.

---

## 1. What meowland is

A Wayland compositor that draws its windows **inside a terminal**, using the
kitty graphics protocol.

- One terminal window shows one compositor window ("a pane"). No tiling, no
  window management: a pane is exactly one window, at the size of the terminal
  it is in.
- The compositor (a server process) **outlives every terminal**. Terminals come
  and go; the Wayland clients and their windows keep living.
- Any number of terminals can attach, each showing a window, each independent.
- The user runs ordinary Wayland clients on it (`foot`, browsers, `mpv`), and
  ordinary X11 clients through xwayland-satellite.

The target experience is: `meowland run foot` behaves like opening a window,
except the window is your terminal.

## 2. The shape of the experience

Command line (all of it):

```sh
meowland run foot                  # start a server if needed, run the client, show its window here
meowland run                       # show the newest window here, start nothing
meowland attach 2                  # show window 2 here
meowland attach                    # show the focused window here
meowland list                      # "id  label  title  active" per window, one per line
meowland server start              # start a server with no terminal attached
meowland server stop               # stop the server and everything it started
meowland completions zsh           # shell completions, including live window IDs
```

Keys, inside a pane:

- `Alt+Q` — ask the shown window to close; on an empty pane, detach.
- `Alt+W` — detach the pane.
- Everything else goes to the client.

A pane detaches on its own when its window closes; the terminal is given back
with a message saying why (if there is a why).

`run` does the whole of the common path: start a server if none is listening,
have the server start the client, watch the window list for the window that
client creates (up to ten seconds), and attach this terminal to it. If no new
window appears it attaches to the newest one instead. Run without a terminal on
stdin/stdout, it still starts the client and returns.

## 3. Stack

Rust (edition 2024). The important dependency choices:

| Piece | What it is for |
|---|---|
| `smithay` 0.7 (`desktop`, `renderer_gl`, `wayland_frontend`) | Wayland server: `wayland-server` protocol dispatch, seats/input, xdg-shell, shm, dmabuf, viewporter, output, data-device, cursor-shape; EGL/GLES bring-up and dmabuf import for read-back |
| `calloop` | event loops (server process, compositor thread, pane client), channels, timers, signals |
| `wayland-server`/`wayland-backend` | reached through smithay's reexports; used directly for `Display`, `ListeningSocketSource`, resources |
| `bincode` 2 + `serde` + `serde_bytes` | wire format for the two internal protocols (pane socket, control socket) |
| `crossterm` | the pane's terminal: raw mode, resize/input events, window size |
| `evdev` | Linux key codes (`KEY_*`) as the internal key vocabulary |
| `rustix` | raw syscalls: `shm_open`/`unlink`, sockets, signals, `/proc`-adjacent process work, fd flags, `poll` |
| `flate2` (zlib), `base64` | kitty graphics payload compression and encoding |
| `nutype` | newtypes for IDs and protocol versions |
| `tracing` + `tracing-subscriber` | file logging; per-second rate reports |
| `usage-rs` | CLI parsing, help, shell completions |
| `thiserror` | one library error type; `anyhow` appears only in `main.rs` |
| `libxkbcommon` (linked), `xwayland-satellite` (runtime) | keymap compilation; X11 clients |

Build/verification environment: Nix flake (crane + rust-overlay + treefmt-nix)
is the authoritative gate — `cargo test`, `cargo clippy --all-targets -- -D
warnings` on nightly, `cargo fmt` (nightly rustfmt, unstable options in
`rustfmt.toml`), plus nixfmt/statix/deadnix/taplo. CI runs the same checks and a
`meowland --help` smoke test. Runtime inputs wrapped into the package:
`xwayland`, `xwayland-satellite`, `libglvnd`, `libxkbcommon`.

## 4. Architecture intent

Three things, deliberately separated:

1. **Server process** — owns everything that outlives a terminal: the control
   socket, the pane sockets, the client processes it spawned, xwayland-satellite,
   the Wayland socket, the compositor thread.
2. **Compositor** — a thread inside the server process. Owns the Wayland
   clients, protocol state, surfaces, focus, and composition. It runs its own
   event loop.
3. **Pane client** — a small process holding one terminal. It takes the terminal
   over, negotiates capabilities, writes frames to the terminal, sends input back.

The server and the compositor **never touch each other's state**; they exchange
messages. The intent is that composing a frame can never make an input event
wait, and that a terminal can never stall the compositor: a slow terminal
delays only its own pane, and its frames are dropped rather than queued.

```
pane process  ──pane socket──┐
pane process  ──pane socket──┤
                            server process ──channel──▶ compositor thread ──▶ Wayland clients
meowland run/list/attach ──control socket──┘                     │
                                                                 └──▶ xwayland-satellite ──▶ X11 clients
```

The message boundary is the load-bearing decision. A reimplementation may cut
it differently, but the invariant is: **the compositor's frame loop must never
block on a terminal, and a terminal's slowness must never be the compositor's
problem.**

### Messages across that boundary

Server → compositor: attach pane (with `Show` and capabilities), detach pane,
capabilities changed, input, "ask every window to close", and a recycled frame.

Compositor → server: a window exists/renamed (id, app-id, title), focus changed,
window closed, pane has nothing left to show (with an optional reason), set the
terminal's title, set the pointer shape, and a frame for a pane.

The frame itself is **moved**, never copied, along the whole path: compositor →
server → presenter → terminal → ack → back to compositor.

## 5. Processes, sockets, lifecycle

- Two internal sockets, both in `$XDG_RUNTIME_DIR`: the **control** socket and
  the **pane** socket. Owner-only files; a second server is refused by the
  socket lock rather than half-binding.
- The Wayland socket is named `wayland-meowland`, with an auto-generated name as
  a fallback.
- `run`, `list` and `stop` connect to the control socket; if no server is
  listening, `run` starts one **detached** (its own session, no controlling
  terminal) and waits for it to listen (5 s budget), passing the resolved
  settings through as flags.
- `meowland server start` detects "am I a session leader?" and only then becomes
  the server loop; otherwise it spawns itself detached. This lets one code path
  serve both the foreground and the detached case.
- Clients are started by the server, not by the `run` process, with
  `WAYLAND_DISPLAY` set. `DISPLAY` is only passed on when xwayland-satellite is
  actually up — an inherited `DISPLAY` would send clients elsewhere. Toolkit
  hints are set (`GDK_BACKEND=wayland`, `QT_QPA_PLATFORM=wayland`,
  `SDL_VIDEODRIVER=wayland`, `MOZ_ENABLE_WAYLAND=1`,
  `ELECTRON_OZONE_PLATFORM_HINT=auto`, `XDG_SESSION_TYPE=wayland`).
- Client stdout/stderr go to the log file (`$XDG_RUNTIME_DIR/meowland.log`,
  overridable), never to a pane: a pane's terminal belongs to meowland.
- The server blocks the signals it watches, so every child must be spawned with
  **its signal mask cleared between fork and exec**, or a client can never be
  killed by `SIGHUP`/`SIGTERM` from anywhere.

### Stopping

`server stop` (or a signal) unwinds in this order, and the order matters:

1. Ask every window to close (`xdg_toplevel.close`) and give the clients a
   moment (≈250 ms). A client that honours it exits with its own children, on
   its own terms.
2. Release the panes: each terminal sees EOF and the terminal is restored.
3. For whatever is left, and for xwayland-satellite: signal the **descendant
   process tree** `SIGHUP`, `SIGTERM`, `SIGKILL`, rescanned before each signal
   with its own grace (≈250 ms). Unix does not kill children when their parent
   exits, so per-child kills are not enough — helpers and daemonized
   grandchildren must be found by walking `/proc` parents.
4. Join the compositor thread; stop xwayland-satellite; clean up the X lock and
   sockets.

SIGCHLD reaps children as they exit (including xwayland-satellite).

### X11

xwayland-satellite is started with a **reserved display number** and `-listenfd`
handles for both the `/tmp/.X11-unix/X<n>` socket and the abstract socket of the
same name. The display number is protected by the standard `.X<n>-lock` file,
with stale locks (owner pid gone) taken over. 0–32 are tried in order. If
xwayland-satellite cannot start, that is a warning, not a failure: X11 clients
then have no display and `DISPLAY` is not passed on.

## 6. Frames: the core loop

Intent, and the hard rules:

- A pane has **one** frame buffer. The compositor draws into it and hands it
  off; until it comes back, that pane cannot draw. The frame that comes back is
  the storage for the next one. This is what makes frames **dropped, not
  queued**: if the terminal is behind, the client is simply told later, and the
  next frame shows the scene as it is then. Nothing that changed is lost.
- A pane's frame is **one kitty image** of the whole screen, placed at the
  terminal's top-left, at its own pixel size. A frame that changes only part of
  the screen is sent as **patches over that image**.
- The presenter keeps the pixels the terminal was last sent; the next frame is
  diffed against them. Remembering costs a buffer swap, not a copy.
- The compositor sends **frame callbacks when it draws**, not on a timer. A
  client that animates draws one frame per callback, so this is what paces it.
  A terminal at 30 fps gives its clients 30 fps.
- Composition is capped (the current design uses ~60 Hz) so a client cannot
  spin the compositor; a late frame does not push the next one out.

### Patches vs whole frames

Rules that made patches correct, and that any reimplementation must keep:

- A patch is a **whole number of character cells** in both directions, and its
  top-left pixel is the top-left pixel of the cell it is placed in. Then placing
  it needs the cursor alone: no cell rectangle (`c=`/`r=`) is ever sent, so
  nothing is scaled and nothing can land off its pixels. This is also why a
  whole frame is sent without `c=`/`r=`: a terminal whose cell size does not
  divide the pixel area would scale it away from the screen.
- Patches require the terminal's **own reported cell size**. A cell size derived
  from pixel ÷ cells can be a pixel out, and a patch placed a pixel out is a
  screen showing the wrong pixels. No answer ⇒ whole frames for that pane.
- Patch count and patch coverage are both bounded: a frame that changes too much
  of the screen, or would take too many tiles, or would leave the terminal
  holding too many images, goes whole instead. (Current numbers, chosen after
  measuring: ≤32 tiles per frame, ≤64 live tiles, and whole when the changed
  area exceeds the screen area — i.e. when the total pixels sent would be more
  than a screen.) The terminal keeps every placed image until it is deleted, so
  the design deletes superseded patches and resets to a whole frame before live
  images accumulate.
- A frame that the terminal already shows costs nothing and is not written;
  the frame still goes back to the compositor at once.

### Whole-frame path

- Whole frames can go through a **POSIX shared memory object**: the escape
  carries only the object name; the terminal reads the pixels itself. This is
  worth it only if the terminal can read it, which is discovered by probe (§7).
- Patches always go the pty's way, compressed when it pays — a patch is a
  fraction of a screen, and opening/unlinking an object costs more than it saves.
- Each pane's transfers use their own object **namespace** (per process, per
  pane), and an encoder reuses one slot: if the object still exists the terminal
  has not read it yet, and the frame goes over the pty instead, which keeps the
  two in order. Left-behind objects (the terminal unlinks what it reads) are
  cleaned by name prefix at encoder drop.
- Compression is zlib, and it is sent only if it actually pays (currently: at
  least a 1/4 saving, else raw pixels). Compositor output is mostly flat colour
  and usually shrinks an order of magnitude; already-compressed content does not
  shrink and the compressor's time is wasted on it.

### The kitty protocol facts this depends on

- Payloads are base64, chunked at 4096 bytes per escape with `m=1` on every
  chunk but the last. Chunks must be whole base64 quanta.
- `a=T` transmits and places in one command; `f=24` RGB; `o=z` zlib; `t=s`
  payload is a shared-memory object name; `s`/`v` are pixel size; `i` is the
  image and `p` its placement; `z` is z-index (positive draws above text);
  `C=1` leaves the cursor still (a placement must never scroll the terminal);
  `q=2` suppresses replies.
- Screen image id 1; patch images are other ids; ids are reused so the terminal
  replaces rather than accumulates. Deletes: `a=d,d=I,i=<id>` frees one image's
  pixels (the capital `I` is what frees), `a=d,d=A` frees all. `CSI 2J` also
  destroys images on screen — which is why wiping is: delete all images, then
  `CSI 2J`, then home.
- A frame is wrapped in the synchronized-update private mode
  (`ESC [ ? 2026 h` … `l`) and written as one buffer in one write, so a frame
  cannot tear.
- The terminal draws the mouse pointer itself; the client's wanted cursor shape
  is translated to a kitty **pointer-shape name** (`OSC 22 ; name ST`), and
  reset with an empty name. A cursor sent as an image cannot be described, so
  the terminal's default stands in for it.
- The terminal's window title is `OSC 2 ; text BEL`. A client supplies that
  text, so control characters are stripped and the length is capped. Same
  reasoning for `meowland list`: a client must not be able to print escapes into
  the terminal reading the list.

## 7. Terminal negotiation (the pane's handshake)

A pane takes the terminal over (raw mode, alternate screen, no autowrap, cursor
hidden, mouse reporting with SGR, bracketed paste, kitty keyboard protocol when
available) and only then asks what the terminal can do. It must be ready for
answers to be missing, partial, or zero.

Findings, all of them learned the hard way:

- Ask, don't guess by terminal name. `XTVERSION` is unreliable (Ghostty reports
  `libghostty`; anything can rename itself). The one name-based fallback used
  is for `SGR-Pixels` (`kitty`/`ghostty`/`WezTerm` prefixes) because that query
  is not universally answered.
- The things worth asking:
  - text area in pixels (`CSI 16 t` → `CSI 4 ; h ; w t`),
  - one cell in pixels (`CSI 14 t` → `CSI 6 ; h ; w t`),
  - terminal identity (`CSI > q` → `DCS > | name(version) ST`),
  - graphics support (`a=q` query under a known image id, answer `OK` in an APC
    reply),
  - kitty keyboard support (`CSI ? u`),
  - `SGR-Pixels` mouse (`CSI ? 1016 $ p`, DECRQM),
  - **shared-memory graphics support**: the terminal does not announce it, so
    the pane sends a one-pixel image through an object and sees whether it was
    read (`OK`). A terminal on another machine (SSH) answers with an error
    because the object is not in its `/dev/shm` — that is the answer, and it
    must not be mistaken for "no graphics".
  - Primary device attributes (`CSI c`) requested **last**, because it is the
    one reply a terminal always gives, and an early one would end the handshake
    before the other answers arrive.
- Zero-valued size answers are treated as no answer (some terminals answer 0,
  which taken literally is an empty screen).
- A silent terminal still answers device attributes. If it never answered the
  graphics query, it cannot show a window: refuse the pane with a clear reason.
- Defaults when answers are missing: 80×24 cells; cell 10×20 px; pixel size
  derived from cells × cell size, or from the pty's window size when the kernel
  gives one.
- A cell size that was probed beats one derived from pixels ÷ cells.
- Probe answers arrive on stdin; nothing else may read stdin until the probe is
  done, and the whole probe gets a short timeout (≈1 s) — a machine-local
  terminal answers in milliseconds, one across SSH in one round trip.
- Terminal key releases are unreliable, and terminal libraries can **spin**
  instead of reporting EOF after a hangup. So: hangup is detected by polling the
  terminal's file descriptors for HUP/ERR, and threads that may be stuck inside
  a terminal read are abandoned rather than joined when the terminal is gone.

## 8. Input: intent and findings

- A key that arrives is a **press and a release**: the terminal's own auto-repeat
  is what a held key looks like. A client that gets no release repeats forever.
  Modifier keys are the exception: they are state, synced from the modifier
  flags the terminal reports, because they are state for everything typed while
  held.
- The terminal reduces a key to a code and a "needs shift" bit; the pane side
  has the key codes and the keymap. Characters arriving as text (paste) are
  replayed as keystrokes by matching each character to the stroke that types it
  on the advertised layout.
- **The offset trap**: Linux input codes (`KEY_*`, evdev) and the codes XKB/the
  Wayland keyboard protocol uses differ by exactly 8 for an `evdev`-rules
  keymap. Confusing them does not fail loudly — it types a different key. This
  conversion must exist in exactly one place.
- Shift that a symbol needs is synthesized: hold the shift key, send the
  stroke, release it, so typing is independent of the terminal's own layout.
- The seat is built on an `evdev`/`pc105`/`us` keymap.
- A compositor binding takes precedence over the client, but only for its keys:
  `Alt+Q`, `Alt+W`, and only without Ctrl/Super.
- Pointer coordinates arrive as terminal cells. If the terminal reported
  `SGR-Pixels`, they *are* pixels; otherwise the cell size is used, aiming at
  the middle of the cell. Wheel events act where the pointer is; scroll amounts
  are the client's units (currently ±15 per notch).
- A pane's focus: typing or clicking takes the keyboard for the pane's window.
  The pane that most recently interacted with a window is what decides that
  window's configured size (falling back to the first pane attached), because a
  window's size must follow the pane the user is actually using.
- Popups get no keyboard grab: the terminal's own shortcuts stay usable (the
  compositor answers the grab request with "popup done").

## 9. The Wayland side: scope and window model

Protocols implemented (the ones clients in the wild need): `wl_compositor`,
`wl_shm`, `wl_subcompositor` (via smithay's compositor state),
`zwp_linux_dmabuf_v1`, `xdg_shell` (toplevels + popups),
`zxdg_output_manager_v1`, `wl_output`, `wl_seat` (keyboard + pointer, no
touch), `wl_data_device`, `wp_cursor_shape_manager_v1`, `wp_viewporter`.

Deliberately absent: tiling/stacking policy, xdg-decoration, layer-shell,
session lock, idle inhibit, fractional scaling, touch, real clipboard ownership.

Window model:

- Windows get **server-assigned monotonic IDs** (Wayland object IDs are scoped
  to one client and may be reused, so they are not usable as handles).
- A window is *shown* only once it has pixels: a first commit with no buffer is
  the client asking to be configured. A window that never draws must not
  capture a following pane.
- Panes attach with a *selection*: a specific window ID, the newest window, or
  the focused window. A window ID and the focused window resolve at attach time
  and *pin* the pane to that window; when its window closes such a pane is
  released, showing the terminal why. "Newest" is the selection that keeps
  following: such a pane takes each window that first has pixels while it is
  attached, and when its window closes it waits for the next one rather than
  being released. (Focused with no focused window yet behaves like newest.)
- A pane that asked for a window that is gone must be released with a reason;
  the check that the window exists happens before the message is sent, but the
  window can close in between, so the compositor must check again and not leave
  a pane holding nothing forever.
- Windows are configured `Maximized` at the pane's pixel size (from the
  deciding pane), plus `Fullscreen` when the client asked for it, plus
  `Activated` for the focused one. Fullscreen here means "be the window on this
  screen": a fullscreen request brings the window to the front.
- Focus lives with the compositor; the keyboard is set on the active window's
  surface; frame callbacks are sent to that surface and its popups.
- `wl_output` is one output, described as the **first attached pane's** size
  (scale 1, normal transform, unknown physical size). Until a pane attaches
  there is no mode, and windows wait for their first configure. A terminal has
  no refresh rate; the reported refresh exists only because the protocol wants
  one, and frame callbacks are what actually pace clients. Surfaces are told
  they enter/leave the output as panes show or stop showing their window.
- Popup positions must be reconciled between "relative to the window geometry
  the client set" and "relative to the surface origin the pane draws from" — one
  shared helper, used by both drawing and hit-testing, so the two can never
  disagree about where a popup is.

## 10. Buffers and composition

- Both `wl_shm` and `zwp_linux_dmabuf_v1` end in the same CPU-side owned
  **snapshot**: a buffer is copied the moment the client commits it, and the
  surface is composited from that copy. This is what makes a client that redraws
  into a reused buffer unable to tear a frame. Damage is consumed/cleared when
  the snapshot is taken; otherwise it would accumulate across commits.
- If a buffer cannot be copied, the surface has nothing to draw, and the buffer
  is still released. Never hold a client's buffer.
- Copy size is bounded (currently: up to 2× the pane's size, so a resize can be
  in flight). No pane attached ⇒ no bound.
- Read layouts: shm and dmabuf are accepted only as ARGB8888/XRGB8888. Anything
  else is refused, not mis-rendered.
- dmabuf has two read paths:
  - **linear + single-plane**: map the plane directly (inside a CPU-access
    synchronization bracket for the driver's sake);
  - **anything else**: draw the buffer into the renderer's own staging texture
    and read it back as `GL_RGBA`. The client's buffer is not necessarily
    something the driver will draw into; a texture the renderer made is.
    Staging textures are kept between reads so a client at a steady size does not
    allocate per frame. The readback waits for the frame to finish.
  - `GL_BGRA` is an extension, `GL_RGBA` is not; readback always asks for RGBA
    and the compositor knows both layouts.
- **Offer clients only what you can actually read.** The dmabuf global is
  advertised with the renderer's colour formats and its device, and the renderer
  is chosen by *device path* so the offered device is the reading device. A
  "can I read this" check runs on a corner of the buffer **before** accepting an
  import: a no is cheap (the client falls back to shm), a wrong yes is not (the
  client has already stopped using shm).
- Output is one RGB (3 bytes/pixel) frame per pane: premultiplied-alpha blending
  (the protocol's requirement) over an opaque destination, nearest-neighbour
  sampling when scaling (with an exact integer mapping, not floats), clipping at
  both the image and frame edges, and a fast row-copy path for opaque images at
  their own size.
- `wp_viewporter` is applied when drawing: source rectangle and destination
  size. Drawing and input hit-testing must use the same size function, or a
  window's clicks land somewhere other than its pixels. A source rectangle
  comes in surface coordinates and is scaled to buffer pixels.
- Input hit-testing walks surface trees topmost-first and respects
  `wl_surface.set_input_region` (whole surface when unset).
- When a window's own surface is opaque, uncropped, and at least the pane's
  size, it leaves no backdrop visible, so the backdrop need not be laid down
  under it. (Measured saving: a screen's worth of pixels per frame.)

## 11. Robustness, safety, secrets of the trade

- **Protocol version discipline**: internal messages are bincode — a build that
  does not know a message cannot skip it. So the pane protocol has an explicit
  version, the server refuses a mismatch with a human-readable reason, and the
  version must be bumped whenever a message changes. Message size is capped
  (64 MiB) so a peer cannot make the other side allocate without limit.
- A terminal's socket may be slow or never speak: the hello has a timeout
  (≈1 s), and control requests have one too, so a dumb connection cannot hold
  the event loop.
- Client-supplied text (window titles, app IDs) is untrusted output: strip
  control characters everywhere it is shown (terminal title, `list`).
- Frames and escapes are written from the buffer they were built in (byte
  strings, not per-byte writes, not re-encoded): the cost of the whole design
  rests on "move, don't copy".
- The library has one error type; `anyhow` is only the binary's concern.
  `unsafe` is denied crate-wide; the few exceptions (shared memory, mapped
  dmabuf, fork/exec hooks) are local, with the safety contract stated: mapped
  memory is only reachable as a raw pointer, the slice is bounded by the
  mapping's length, and it is copied out at once.
- The process tree is discovered by one pass over `/proc` (parent field read
  from the last `)` in `stat`, because process names may contain parentheses),
  then walked.
- Per-second rate reports (composed frames/s, bytes and patches per frame,
  encode ms, write ms, skipped frames) exist so a slow terminal is *attributed*
  rather than guessed at. Keep some equivalent if you want to debug performance.

## 12. Performance intent

- No copies where a move or a swap works: frames move between threads; the
  presenter swaps its remembered pixels with the incoming frame; whole frames
  may go through shared memory so their pixels never touch the pty; the
  compressor reuses its allocation.
- Avoid per-frame allocation: reused encode buffers, reused staging textures,
  reused base64 buffers, tile buffers.
- Encoding work is proportional to what changed: a row that did not change costs
  one comparison; tiles are examined only in changed rows; detecting one tile
  past the patch limit is enough to give up and send whole.
- The frame loop is capped and frames are dropped under back-pressure rather
  than queued.

## 13. Non-goals (deliberate)

- No tiling, stacking, or window management beyond "one pane, one window".
- No multiple windows per pane; no window switcher inside a pane.
- CPU composition only. There is no GPU presentation path; the GPU is used
  solely to read back buffers it cannot be mapped.
- No touch input; no tablets (only enough of the tablet protocol for
  cursor-shape to work).
- No DnD semantics; clipboard is whatever clients negotiate between themselves.
  Terminal paste is replayed as keystrokes.
- No client cursor images: the terminal draws the pointer.
- No fractional scaling; scale is 1.
- Server restart/session restore is out of scope.

## 14. Verification intent

What "it works" meant here, and should mean again:

- Unit tests live beside the logic they cover, and are worth keeping only where
  they defend a contract: the kitty encoder round-trips pixels byte-for-byte
  through an independent decoder; the probe parser is tested against byte-for-
  byte captures of a real kitty handshake (including the SSH variant where the
  shared-memory probe fails but graphics work); the presenter is tested against
  a simulated terminal that replays the escapes and compares the resulting
  screen pixels for a long sequence of whole/patch frames; the protocol round-
  trips and rejects truncated/unknown messages; the process tree walk is tested
  against a shell with its own children; drawing has a slow reference
  implementation to compare every scaling/clipping/format case against.
- A compositor like this cannot be proven by tests alone. The real check is
  running it in a pane: run clients, resize the terminal, detach and reattach,
  stop the server, and watch targets/frames in the log.
- The frame path's costs are measured, not asserted: `cargo bench` runs the
  criterion suites in `benches/`. `kitty.rs` covers the encoder's branches (a
  whole frame of flat and of incompressible pixels at 1080p and 4K, one-cell
  and block patches, the patch budget spent exactly, a revert to the base
  image, an unchanged frame, and the shared-memory handover including the
  fallback when the object has not been read), and `protocol.rs` covers the
  wire codec (frames both ways, the per-event input messages, the window
  list). Criterion is a dev-dependency with plotting off: the numbers are
  text. The end-to-end rate check — client commit, compositor copy, pane
  socket, ack — stays with the harness it needs, as the ignored test in
  `tests/wayland.rs`.
- The three-part split must be observable in practice: killing a pane must not
  disturb other panes or the compositor; a stalled terminal must only cost its
  own pane frames.

## 15. Checklist for a reimplementation

If you rebuild this: hold these, choose your own means.

1. Server outlives terminals; panes attach/detach freely.
2. Compositor isolated from terminal slowness (separate thread or process,
   message-only boundary).
3. One frame per pane, moved not copied, dropped not queued, ack-paced.
4. Whole-frame image + cell-aligned patches only when the terminal reported its
   cell size; otherwise whole frames.
5. Never send `c=`/`r=`; never scroll the terminal (cursor stays put); wrap a
   frame in a synchronized update, one write.
6. Probe, don't guess: sizes, graphics, keyboard, pixel mouse, shared memory.
   Handle missing/zero/partial answers and SSH.
7. Press+release for every non-modifier key; modifiers are state; one place
   knows the evdev↔XKB offset.
8. Copy client buffers at commit; never hold them; bound the copy.
9. Advertise only GPU buffers you have proven you can read; keep a CPU fallback.
10. Version the pane protocol; refuse mismatches politely.
11. Sanitize every client-supplied string before a terminal sees it.
12. On stop: close windows first, then release panes, then escalate signals
    over the rescanned process tree.
13. Keep per-second counters for frames, bytes, patches, encode/write time.
14. `Alt+Q` closes the shown window (or detaches an empty pane); `Alt+W` detaches.
15. Treat terminal key releases as unreliable; terminal hangups as pollable;
    terminal libraries as able to spin forever.
