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

Every toplevel is a window of its own, filling whatever pane shows it: there is
no tiling and no window list on a screen, which is what keeps movement and focus
simple. A window is named by the ID the server gives it when it is created, and
that is what `attach` takes. A pane that has not been given a window to show
follows the newest one, and takes it when it first has pixels to put on a screen
- the newest one that does, so what was just started is what is being looked at.
Nothing else moves a pane: clients open windows they never draw in, and showing
one of those would leave the pane with nothing on it at all.
`wp_viewporter` is applied while snapshots are drawn; xwayland-satellite needs
that protocol to expose X11 windows as ordinary xdg-shell surfaces. Meowland
reserves an X display, passes its listening sockets to xwayland-satellite, and
sets both `DISPLAY` and `WAYLAND_DISPLAY` for the launched command. The Nix
package and development shell provide xwayland-satellite and Xwayland; without
the satellite the compositor still runs, and its clients are given no `DISPLAY`
at all rather than an X server outside this terminal.

## The server and its panes

Meowland is a server with no terminal of its own (`src/server.rs`). It owns the
compositor, the windows and the clients started in it; a *pane* is one attached
terminal (`src/client.rs`), and it asks to be shown **one window**. The server
draws that window into that terminal's own geometry and sends it there, so
panes are independent of one another: two of them can be showing the same
window, or one each, and nothing any pane does displaces another.

```sh
meowland run foot        # run foot in the server, starting one if there is none
meowland list            # the windows: ID, app, title, and the one with the keyboard
meowland attach 2        # show window 2 in this terminal
meowland attach          # ...or the window that has the keyboard
meowland quit            # stop the server and everything started in it
```

`run` starts a server if there is none and hands the command over, and a server
already running starts the client - so the client gets the *server's*
environment, `PATH` included, rather than that of the shell which typed the
command. It shows the window that client opens rather than the newest one there
is: it notes which windows exist before the command, waits for one that does not
- which is how it knows the app's own window, splash screens and second windows
included - and pins its pane to that one, so nothing else that opens while it is
being looked at can take the pane over. A command that opens no window within
ten seconds leaves the server shown as it is, following whatever appears later,
and one that is gone before then (a windowless command that has finished, a
server that stopped with its command) leaves the terminal alone entirely. A server started for a command is one that exists for it: it stops once
the clients it started are gone, so `meowland run foot` gives the terminal back
when foot exits. One that is already running was started by something still
using it and outlives the command it was handed. Showing a window takes a
terminal and running a command does not, so a `run` with no terminal to draw on
- a script, or output redirected - gives the server its command and exits.

There is one binding, because there is nothing else to decide: `Alt+Q` asks the
window the pane is showing to close (`xdg_toplevel.close`, which a client is
free to answer with a question rather than by exiting), and a pane with nothing
to show lets go of its terminal instead. A pane is a place to look at one
client, so when that client goes - closed this way, or by its own button, or by
exiting - the pane is done and its terminal goes back to whoever was using it.
Typing or clicking in a pane is what gives its window the keyboard, which is the
sense in which one window is "active": `list` marks it.

A window's title is the client's, and it goes to the terminal showing it: that
terminal calls itself what the window calls itself (`tty::title`), so a browser's
pane says which page it is on and an editor's which file is open, with the title
the terminal had pushed onto its stack and popped back on the way out. The title
is filtered and cut before it is sent - control characters in it would be a
client writing escapes to a terminal it does not own - and the same name is what
`list` prints beside the app, which is what tells one window of an app from
another.

What a client draws is its own size, and the size it is told is that of the pane
showing it - the first such pane, when more than one is. Every window is told
`Activated` when it has the keyboard, and one that asked for fullscreen is told
`Fullscreen` too: a page whose video goes fullscreen, or a player started with
`--fullscreen`, stays in its windowed self until it hears that, so the state is
not a formality. A window no pane shows is not configured at all: there is no
terminal to size it for, and a client that is told nothing waits rather than
guessing.

The server is reached over two sockets in `$XDG_RUNTIME_DIR`, commands on one
(`src/control.rs`) and panes on the other (`src/display.rs`, and the vocabulary
both ends speak). Their names are fixed and binding them is what says whether a
server is already there: they are per-user and owner-only, since one of the
commands starts a process. A client that connects to the Wayland socket on its
own (`WAYLAND_DISPLAY=wayland-meowland`) becomes another window too, with no
pane to show it until one attaches.

A pane says hello with what it can do and what it wants to be shown
(`Show::Window` for one window by ID, `Show::Focused` for whichever has the
keyboard, `Show::Newest` to follow what is started next), and is answered by
being drawn on or by being told why not - a version this server does not speak, or a
window ID no window has. What it is sent is frames and escapes in the order they
were made, and a frame is the one message it answers: each pane keeps one frame
on its way at a time, so what a terminal is behind on is one screen rather than
a queue of stale ones. Each pane has a presenter thread of its own, which is
what keeps a slow terminal from being anyone else's business, and the terminal
side writes, reads and asks whether the terminal has hung up in threads of its
own, because a closed terminal fails reads in a way the terminal library spins
on instead of reporting.

A client's own stdout and stderr are not the terminal: the server is drawing on
it through the pane, so text written there lands in the cells the frame is
placed on and a newline among it scrolls the frame out from under itself. They
are given the log instead, which is opened for appending so that the server and
its clients write to one file in the order they wrote. Client output that reads
as "why did no window appear" is found there. A server with no pane attached
keeps running and keeps its windows, and draws nothing. The one window a pane
was given is the whole of what it is for: that window going is the pane going,
while a pane that follows the newest window carries on to the next one.

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
