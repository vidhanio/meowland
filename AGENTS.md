# Repository Guidelines

## Product

`meowland` is a Wayland compositor that runs inside a terminal. Clients connect
to a Wayland socket as usual. The compositor draws their windows into a frame
buffer it owns, and sends that buffer to the terminal with the
[kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/).

`wl_shm` is the path that every client can use. `zwp_linux_dmabuf_v1` exists for
clients that render on the GPU: those pixels are read back through a renderer on
the same device (`src/gpu.rs`). A driver may keep a buffer where the CPU cannot
map it, such as video memory, and such a buffer cannot be read directly. The
compositor advertises only the formats that the renderer can read back. A client
that takes an offer and is then refused has no window at all.

Both paths give the compositor a `Snapshot` of pixels in main memory. The
compositor composites, diffs and sends that copy. Composition stays on the CPU
and has one implementation. The terminal never learns where the pixels came
from. Composing runs on the thread that reads input. Sending does not
(`src/presenter.rs`), because compressing a frame and waiting for the terminal
costs milliseconds, and a keystroke would queue behind that work. If the
presenter is busy, the frame is dropped and its tiles stay due for a later
frame. `Meowland` is the application state: it holds the protocol globals, the
toplevels, input routing and presentation. Pass that one state around, and do
not build a second copy of any part of it.

Each toplevel is one window and fills the pane that shows it. There is no tiling
and no window list. The server gives each window an ID when it creates it, and
`attach` takes that ID. A pane that has no window follows the newest one, and
takes it when that window first has pixels to draw. A client that opens a window
it never draws in therefore cannot leave a pane empty. `wp_viewporter` is
applied while snapshots are drawn; xwayland-satellite needs that protocol to
expose X11 windows as ordinary xdg-shell surfaces. Meowland reserves an X
display, passes its listening sockets to xwayland-satellite, and sets `DISPLAY`
and `WAYLAND_DISPLAY` for the launched command. The Nix package and the
development shell provide xwayland-satellite and Xwayland. Without the satellite
the compositor still runs, and its clients get no `DISPLAY` at all, rather than
an X server outside this terminal.

## The server and its panes

Meowland is a server with no terminal of its own (`src/server.rs`). It owns the
compositor, the windows and the clients started in it. A *pane* is one attached
terminal (`src/client.rs`), and it asks to be shown one window. The server draws
that window into that terminal's geometry and sends it there. Panes are
independent: two panes can show the same window, or one each, and no pane
displaces another.

```sh
meowland run foot        # run foot in the server, starting one if there is none
meowland list            # the windows: ID, app, title, and the one with the keyboard
meowland attach 2        # show window 2 in this terminal
meowland attach          # ...or the window that has the keyboard
meowland server start    # start a server on its own, and stay in it
meowland server stop     # stop the server and everything started in it
```

`run` starts a server if there is none, and hands the command to it. A server
that is already running starts the client instead, so the client gets the
environment of the server, `PATH` included. `run` shows the window that the
client opens, not the newest window there is. It notes the windows that exist
before the command, waits for a new one, and pins its pane to that window. This
is how it finds the window of an application that starts with a splash screen,
or opens a second window later. A command that opens no window within ten
seconds leaves the server shown as it is. A command that is gone before then
leaves the terminal alone.

Stopping a server hands every pane back first, and then stops every process
under the server: `SIGHUP`, `SIGTERM`, then `SIGKILL`, each with a short grace
period (`src/process.rs`). The tree is read again before each signal, so a helper
that a client started while the last one was being delivered is included. A
process that daemonizes leaves the tree and survives, as it would with any other
supervisor.

A server has no command of its own. `run` starts one if there is none, then
hands it the command over the control socket, so the first `run` and every later
one take the same path. The server starts the clients it is handed and keeps
them: it stops only when `meowland server stop` or a signal stops it.
`meowland run foot` gives the terminal back when the window of foot goes away,
and the server stays for the next command. Showing a window needs a terminal.
Running a command does not. A `run` with no terminal to draw on therefore gives
the server its command and exits.

There is one binding. `Alt+Q` asks the window that the pane shows to close. The
request is `xdg_toplevel.close`, which a client may answer with a question
instead of exiting. A pane with nothing to show releases its terminal. A pane
shows one client, so when that client goes away, by any means, the pane is done
and the terminal is released. Typing or clicking in a pane gives its window the
keyboard, and `list` marks that window active.

The title of a window belongs to the client and goes to the terminal that shows
it. The terminal sets its own title from it (`tty::title`), so a browser pane
names its page and an editor pane its file. The terminal puts its previous title
back on exit. The title is filtered and cut before it is sent. Control
characters in a title would let a client write escapes to a terminal it does not
own. `list` prints the same name beside the app, which is how two windows of one
app are told apart.

A client draws at its own size, and the size it is told is that of the pane that
shows it. When several panes show it, that is the first of them. A window with
the keyboard is told `Activated`. A window that asked for fullscreen is also
told `Fullscreen`. A page whose video goes fullscreen, or a player started with
`--fullscreen`, stays windowed until it hears that state. A window that no pane
shows is not configured. There is no terminal to size it for, and a client that
is told nothing waits.

The server is reached over two sockets in `$XDG_RUNTIME_DIR`. Commands arrive on
one (`src/control.rs`) and panes on the other (`src/display.rs`, which holds the
vocabulary of both ends). The names are fixed, and binding a name is how a
server is found. The sockets are per-user and owner-only, because one of the
commands starts a process. A client can also connect to the Wayland socket
directly (`WAYLAND_DISPLAY=wayland-meowland`); it becomes another window, with
no pane to show it until one attaches.

A pane says hello with what it can do and what it asks to be shown. The choices
are `Show::Window` for one window by ID, `Show::Focused` for the window with the
keyboard, and `Show::Newest` to follow what starts next. The server answers by
drawing on the pane, or by refusing it. It refuses a version it does not speak,
and a window ID that no window has. Frames and escapes are sent in the order
they were made, and a frame is the only message that the terminal answers. Each
pane keeps one frame on its way at a time. A slow terminal therefore falls
behind by one screen, and not by a queue of stale screens. Each pane has its own
presenter thread, so one slow terminal holds up nothing else. The terminal side
reads, writes and checks for a hangup in threads of its own. A closed terminal
fails reads in a way that the terminal library spins on instead of reporting.

A client's stdout and stderr are not the terminal. The server draws on that
terminal, so text written to it lands in the cells of the frame, and a newline
scrolls the frame away. Both descriptors are given the log instead. The log is
opened for appending, so the server and its clients write to one file in the
order they wrote. The log is where to look when a client starts and no window
appears. A server with no pane attached keeps running, keeps its windows, and
draws nothing.

Terminals that read tiles out of shared memory get them that way, which keeps
their pixels off the pty. The probe that the terminal side runs at attach
decides this. It sends one tile that way, and waits for the terminal to say it
read it. Each pane's presenter names its own objects, so two panes never
collide. Everything else goes through the pty, base64 encoded.

`meowland completions <shell>` prints the completion script that `usage`
generates for this CLI, which calls back into `meowland __complete_word__`.
Completing `attach` asks the running server which windows it has, so the shell
offers only windows that exist. The Nix package installs the bash, fish and zsh
scripts with `installShellFiles`, which is why the binary must run during
`postInstall`.

## Development

```sh
cargo +nightly fmt
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

`rustfmt.toml` uses unstable options, so formatting needs nightly rustfmt.
Clippy runs the nursery and pedantic groups; fix lints instead of allowing them.
`unsafe_code` is denied crate-wide. Three documented exceptions read a client
buffer through a raw pointer: shared memory as a slice, a mapped GPU buffer as a
slice bounded by the mapping, and bringing up EGL and GLES, whose smithay
constructors are unsafe. The fourth clears the signal mask of a child between
`fork` and `exec` (`src/process.rs`), where it is the only way to do it and the
only place where it is safe to try.

The library uses one `thiserror` type in `src/error.rs`. An underlying error
has one variant, with `From` conversions where appropriate. `anyhow` appears
in `src/main.rs` only, which reports failures as text.

The commands that draw need a terminal on stdin and stdout that speaks the kitty
graphics protocol. kitty, Ghostty and WezTerm do, and so does a pane that passes
graphics through. Redirecting their output makes them exit immediately.
`meowland server start` needs no terminal, which is what lets a server outlive the
terminal it was started from. Logs go to `$XDG_RUNTIME_DIR/meowland.log`, and a
client's own output goes there too. Wayland clients connect with
`WAYLAND_DISPLAY=wayland-meowland`.

Settings are flags that fall back to environment variables, and the flag wins:
`--gpu-buffers` (`MEOWLAND_GPU_BUFFERS`, default `auto`), `--render-node`
(`MEOWLAND_RENDER_NODE`), `--log` (`MEOWLAND_LOG`), `--log-level`
(`MEOWLAND_LOG_LEVEL`). The CLI resolves them, and a module is handed the
decisions, not an environment to look up.

`--gpu-buffers off` stops clients being offered GPU buffers. `auto` offers the
first render node that a renderer can be built on, and `--render-node` picks a
node instead of taking the first. The offer is only made where a renderer can
read buffers back. A compositor that advertises GPU buffers and then refuses
those of a client leaves that client with no window.

A renderer needs `libEGL.so.1` on the loader search path, and `dlopen` does not
take it from `buildInputs`. The package wraps the binary with an
`LD_LIBRARY_PATH`, and the development shell sets one. Outside both, on a
machine that has a render node but no loadable EGL, startup panics inside the
EGL bindings. That is the behaviour of the bindings, and the reason `off`
exists.

Terminals and multiplexers do not reliably report key releases. Meowland
therefore treats a press as a whole keystroke: press, release, and let the
terminal's own auto-repeat produce the repeats. Keep that rule. A client that is
left holding a key the terminal never released repeats it forever.

Tests live next to the logic they cover. The startup and state plumbing has no
tests on purpose; check it by running the compositor in a pane.

## Commits

Prefix the subject with the area you touched, in lowercase (`keys:`,
`compositor:`), and put backticks around code identifiers, paths and commands.
Commit a coherent change once `cargo +nightly fmt`,
`cargo clippy --all-targets -- -D warnings` and `cargo test` pass.
