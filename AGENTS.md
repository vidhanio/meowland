# Repository Guidelines

## Product

`meowland` is a Wayland compositor that runs inside the terminal. Clients connect
to a Wayland socket as usual, their windows are composited into a frame buffer the
compositor owns, and that buffer is drawn with the
[kitty graphics protocol](https://sw.kovidgoyal.net/kitty/graphics-protocol/).

`wl_shm` is the path every client can take. `zwp_linux_dmabuf_v1` exists so
clients that render on the GPU can hand their buffers over: those pixels are
brought back through a renderer on the same device (`src/gpu.rs`), because a
driver is entitled to keep a buffer somewhere the CPU cannot map - video memory
is exactly that - and such a buffer cannot be read as memory at all. What is
advertised is what that renderer takes, and nothing else, because a client that
takes the offer and is then refused has no window at all.

Both paths hand the compositor a `Snapshot` of pixels in main memory, which is
what it composites, diffs and sends to the terminal: composition stays on the
CPU, one implementation of it, and the terminal never learns where the pixels
came from. Composing happens on the thread that reads input and sending does not
(`src/presenter.rs`), because compressing a frame and waiting for the terminal to
take it is milliseconds of work that a keystroke would otherwise queue behind.
A frame the presenter is too busy for is dropped rather than queued, and the
tiles it carried stay due until a later frame carries them. `Meowland` is the application state and owns the protocol globals,
toplevels, input routing and presentation. Pass that one state around rather than
building a second copy of any part of it.

Every toplevel is a window of its own: it fills the terminal, one window is on
screen at a time, and `Alt+Tab` cycles between them. The screen shows the active
window and nothing else - no tiling, no window list on screen - which is what
keeps movement and focus simple. A window is named by the ID the server gives it
when it is created, and that is what `attach` takes. A window takes the screen
when it first has pixels to put on it - the newest one that does, so what was
just started is what is being looked at. Nothing else moves it: clients open
windows they never draw in, and taking the screen for one of those would leave
nothing on it at all.
`wp_viewporter` is applied while snapshots are drawn; xwayland-satellite needs
that protocol to expose X11 windows as ordinary xdg-shell surfaces. Meowland
reserves an X display, passes its listening sockets to xwayland-satellite, and
sets both `DISPLAY` and `WAYLAND_DISPLAY` for the launched command. The Nix
package and development shell provide xwayland-satellite and Xwayland; without
the satellite the compositor still runs, and its clients are given no `DISPLAY`
at all rather than an X server outside this terminal.

## The server

Meowland is a server with no terminal of its own (`src/server.rs`). It owns the
compositor, the windows and the clients started in it, and it draws on whichever
terminal is attached to it at the time - a terminal being a separate process
(`src/client.rs`), so the one it is being looked at in can be closed, or another
one can take over, without the server or its windows going anywhere:

```sh
meowland run foot        # run foot in the server, starting and showing one if there is none
meowland list            # the windows, their labels, and the active one
meowland attach 2        # show window 2 here
meowland quit            # stop the server and everything started in it
```

`run` starts a server if there is none and hands the command over, and a server
already running starts the client - so the client gets the *server's*
environment, `PATH` included, rather than that of the shell which typed the
command. A server started for a command is one that exists for it: it stops once
the clients it started are gone, so `meowland run foot` gives the terminal back
when foot exits. One that is already running was started by something still
using it and outlives the command it was handed.

Both `run` and `attach` show the server in the terminal they were typed in, and
they differ in what they do about a server that is already shown: `run` leaves
it alone, so a command typed elsewhere appears in the terminal that has the
server, while `attach` takes it over. `Alt+Q` stops showing the server without
stopping it; `meowland quit` stops the server. Showing a server takes a terminal
and running a command does not, so a `run` with no terminal to draw on - a
script, or output redirected - gives the server its command and exits.

There are no decorations, so the bindings are the whole of the window
management: `Alt+Tab` cycles the windows, `Alt+W` asks the window on screen to
close (`xdg_toplevel.close`, which a client is free to answer with a question
rather than by exiting), and `Alt+Q` lets go of the terminal. What a client
draws is its own size: every window is *told* the size of the terminal and
`Activated`, and one that asked for fullscreen is told `Fullscreen` too - a page
whose video goes fullscreen, or a player started with `--fullscreen`, stays in
its windowed self until it hears that, so the state is not a formality.

The server is reached over two sockets in `$XDG_RUNTIME_DIR`, commands on one
(`src/control.rs`) and terminals on the other (`src/display.rs`, and the
vocabulary both ends speak). Their names are fixed and binding them is what says
whether a server is already there: they are per-user and owner-only, since one
of the commands starts a process. A client that connects to the Wayland socket
on its own (`WAYLAND_DISPLAY=wayland-meowland`) becomes another window too.

A terminal says hello with what it can do and what it wants, and is answered by
being drawn on or by being told why not - a version this server does not speak,
a window ID that no window has, or another terminal already showing it. What it
is sent is frames and escapes in the order they were made, and a frame is the
one message it answers: the server keeps one frame on its way at a time, so what
a terminal is behind on is one screen rather than a queue of stale ones. The
terminal side writes and reads in threads of its own, and asks whether the
terminal has hung up, because a closed terminal fails reads in a way the
terminal library spins on instead of reporting.

A client's own stdout and stderr are not the terminal: the server is drawing on
it through the terminal side, so text written there lands in the cells the frame
is placed on and a newline among it scrolls the frame out from under itself.
They are given the log instead, which is opened for appending so that the server
and its clients write to one file in the order they wrote. Client output that
reads as "why did no window appear" is found there. A server with no terminal
attached keeps running and keeps its windows, and draws nothing.

`meowland completions <shell>` prints the completion script `usage` generates for
this CLI, which calls back into `meowland __complete_word__`; completing `attach`
asks the running server which windows it has, so what is offered is what exists.
The Nix package installs the bash, fish and zsh scripts with
`installShellFiles`, which is why the binary has to run during `postInstall`.

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
buffer as a slice bounded by the mapping. Bringing up EGL and GLES is the third,
because smithay's constructors for both are unsafe and there is no safe way in.

Each module declares its own error type with `thiserror`, so a caller can tell
its failures apart. `anyhow` belongs to the command layer - `src/lib.rs` and the
startup `src/server.rs` does before its event loop - because that is where a
failure is reported rather than handled. `src/main.rs` only says where to start.

The commands that draw need a terminal on stdin and stdout that speaks the kitty
graphics protocol (kitty, Ghostty, WezTerm, or a pane that passes graphics
through); redirecting their output makes them exit immediately. `meowland
server` needs no terminal at all, which is what lets a server outlive the one it
was started from. Logs go to `$XDG_RUNTIME_DIR/meowland.log`, and a client's own
output goes there too - see [the server](#the-server). Wayland clients connect
with `WAYLAND_DISPLAY=wayland-meowland`.

Terminals that read tiles out of shared memory get them that way, which keeps
their pixels off the pty entirely; the probe the terminal side runs when it
attaches decides, by sending one tile that way and seeing whether the terminal
says it read it. Everything else goes direct, base64'd, as before.

Settings are flags that fall back to environment variables, and the flag wins:
`--gpu-buffers` (`MEOWLAND_GPU_BUFFERS`, default `auto`), `--render-node`
(`MEOWLAND_RENDER_NODE`), `--log` (`MEOWLAND_LOG`), `--log-level`
(`MEOWLAND_LOG_LEVEL`). The resolving is the CLI's job - a module is handed what
was decided, not an environment to look up.

`--gpu-buffers off` stops clients being offered GPU buffers at all; `auto` offers
the first render node a renderer can be built on, and `--render-node` picks that
node instead of taking the first. The offer is only made where a renderer can
bring buffers back, which is what makes it safe to make unasked - a compositor
that advertises GPU buffers and then refuses the ones a client produces leaves
that client with no window at all.

A renderer needs `libEGL.so.1` at the loader's search path, which `dlopen` does
not take from `buildInputs`: the package wraps the binary with an
`LD_LIBRARY_PATH`, and the dev shell sets one. Run outside both and, on a machine
with a render node but no loadable EGL, startup panics inside the EGL bindings -
the bindings' own behaviour, and the reason `off` exists.

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
