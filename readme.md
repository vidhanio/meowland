# meowland

A Wayland compositor that draws windows in a terminal with the kitty graphics
protocol. A server outlives every terminal; each terminal is a *pane* showing
one window. Nothing is tiled: a pane is one window, as big as the terminal it is
in.

## Processes and sockets

```mermaid
flowchart LR
    subgraph cli["meowland run / attach / list / server"]
        run["run"]
        attach["attach"]
        list["list, server stop"]
    end

    subgraph srv["server process"]
        ctl["control socket"]
        loop["event loop"]
        panes["panes: a reader and a presenter thread each"]
        kids["client programs, xwayland-satellite"]
    end

    comp["compositor thread: clients, surfaces, frames"]
    pane["pane process: one terminal"]
    apps["Wayland clients"]
    xc["X11 clients"]

    run -->|"Run(argv)"| ctl
    list -->|"List / Stop"| ctl
    loop --- ctl
    loop --- panes
    loop -->|"Command: attach, detach, resize, input, close, frame back"| comp
    comp -->|"Event: windows, panes, title, pointer, frame"| loop
    attach <-->|"pane socket"| panes
    comp --- apps
    kids --- xc
```

The compositor holds the Wayland clients and the frames they add up to; the
server holds the panes, the control socket, and the programs it started. They
reach each other only in messages (`src/wayland/message.rs`), so composing a
frame never makes an input event wait.

## A frame

A pane has one frame. The compositor draws into it and cannot draw that pane
again until the presenter gives it back. The screen the terminal shows is one
kitty image, and a frame that changes only part of it is sent as patches over
that image: tiles of the screen, each a whole number of cells across and down,
placed in the cell their top-left pixel is in — which is what keeps a patch on
the pixels it holds. A frame that changes too much of the screen, or that takes
too many tiles, goes whole.

```mermaid
sequenceDiagram
    participant W as Wayland client
    participant C as compositor thread
    participant S as server loop
    participant P as presenter thread
    participant T as terminal

    W->>C: commit
    Note over C: copy the client's buffer once, into a Snapshot
    C->>C: on the 60 Hz tick: draw into the pane's frame
    C->>S: Event::Frame (the frame moved)
    S->>P: present(frame)
    Note over P: diff against the frame the terminal has:<br/>nothing, a few patches over it, or the whole screen
    P->>T: one write: sync start, images, sync end
    T->>T: write the bytes to the terminal
    T->>S: Drawn, over the pane socket
    S->>P: drawn()
    P->>S: Free (the frame moved back)
    S->>C: Command::Frame
    Note over C: the same buffer is drawn into again
```

Frames are dropped while a pane's terminal is behind: the frame drawn next is
the scene as it is then, so nothing that changed is lost. The frame itself is
moved between the three threads and back, never copied, and its bytes go to the
socket out of the buffer they were encoded in — a byte string, not a write per
byte. The presenter keeps the pixels it last sent, swapping each frame's buffer
with its own, and that is what a new frame is diffed against.

## Input

```mermaid
sequenceDiagram
    participant K as keys, mouse, paste
    participant T as pane process
    participant S as server loop
    participant C as compositor thread
    participant W as Wayland client

    K->>T: a terminal event
    T->>S: Input, over the pane socket
    S->>C: Command::Input
    C->>C: Alt+Q / Alt+W, else focus the pane's window
    C->>W: wl_keyboard / wl_pointer
```

Terminals do not have to report key releases, so a press is sent as a press and
a release, and the terminal's own repeat is what a held key looks like.

## Starting and stopping

```mermaid
flowchart TD
    a["meowland run foo"] --> b{"server running?"}
    b -->|no| c["spawn a detached server: bind the control, pane and Wayland sockets,<br/>start the compositor thread and xwayland-satellite"]
    b -->|yes| d
    c --> d["control socket: Run(argv)"]
    d --> e["the server spawns the client, with WAYLAND_DISPLAY (and DISPLAY if X11 works)"]
    e --> f["compositor: a toplevel arrives"]
    f --> g["this terminal attaches: Hello(show), over the pane socket"]
    g --> h["frames flow, one per frame callback"]
    h --> s["meowland server stop"]
    s --> t["Command::Close: xdg_toplevel.close to every window"]
    t --> u["clients that honour it exit on their own"]
    u --> v["release the panes: their terminals see EOF"]
    v --> w["SIGHUP, SIGTERM, SIGKILL over the process tree, rescanning before each"]
    w --> x["join the compositor thread, stop xwayland-satellite"]
```
