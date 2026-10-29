A Wayland compositor that runs apps in terminal panes.

## Architecture

Meowland runs as two kinds of process:

- **Server** (`meowland server`): a headless Wayland compositor. It runs on one thread
  with a single `calloop` loop.
- **Pane clients** (`meowland run` / `meowland attach`): one per terminal. Each pane
  client sends terminal input to the server and draws the frames it gets back using the
  Kitty graphics protocol.

### Crate dependencies

```mermaid
flowchart TD
    bin["meowland<br/>CLI, config loading, logging"]
    server["meowland-server<br/>event loop owner, sockets, processes"]
    compositor["meowland-compositor<br/>Smithay state, panes, input, clipboard bridge"]
    render["meowland-render<br/>CPU composition, damage, DMA-BUF readback"]
    terminal["meowland-terminal<br/>pane client session, probing, presentation"]
    input["meowland-input<br/>VT and Kitty keyboard decoding"]
    tclip["meowland-terminal-clipboard<br/>OSC 5522 and OSC 52"]
    kitty["meowland-kitty<br/>Kitty graphics encoding"]
    protocol["meowland-protocol<br/>wire messages, framing, shared frames"]
    core["meowland-core<br/>errors, FrameSize, async helpers, scale"]

    bin --> server
    bin --> terminal
    bin --> protocol
    server --> compositor
    server --> protocol
    compositor --> render
    compositor --> protocol
    terminal --> input
    terminal --> tclip
    terminal --> kitty
    terminal --> protocol
    input --> protocol
    kitty --> protocol
    tclip --> protocol
    protocol --> core
    render --> core
    server --> core
    compositor --> core
    terminal --> core
    input --> core
    kitty --> core
```

### Runtime flow

```mermaid
flowchart LR
    subgraph Term["Terminal emulator: Kitty or Ghostty"]
        tty["stdin / stdout"]
    end

    subgraph Pane["Pane client: meowland attach / run"]
        probe["probe.rs<br/>capability handshake"]
        session["session.rs<br/>TerminalSession select loop"]
        decode["meowland-input<br/>Reader + Keyboard"]
        gate["input_gate.rs<br/>defer input during clipboard refresh"]
        present["presentation.rs<br/>PanePresentation"]
        presenter["meowland-kitty<br/>Presenter + Encoder"]
        clip["meowland-terminal-clipboard"]
    end

    subgraph Server["Server process: meowland server"]
        sockets["sockets.rs<br/>control.sock + pane.sock"]
        transport["transport.rs<br/>per-connection futures"]
        actor["actor.rs run loop<br/>Server owns Compositor"]
        control["control.rs<br/>Run / List / Kill"]
        proc["process.rs<br/>spawn app with WAYLAND_DISPLAY"]
        subgraph Comp["meowland-compositor"]
            cmds["commands.rs<br/>handle_command"]
            inp["input.rs<br/>pane_input to wl_seat"]
            buf["buffer.rs commit<br/>shm copy / readback"]
            rb["readback.rs<br/>DMA-BUF fences + GPU"]
            frame["frame.rs<br/>PaneState + dispatch_frames"]
            cb["clipboard.rs<br/>selection bridge"]
        end
        renderer["meowland-render<br/>Renderer::update"]
    end

    app["Wayland client app"]

    tty -->|raw bytes| decode --> session
    session --> gate -->|PaneAction::Input| transport
    session -->|Hello, Resize, Ack| transport
    clip <-->|OSC 5522 / 52| tty
    session <--> clip
    transport -->|Incoming mpsc| actor
    actor --> control --> proc --> app
    actor -->|Command| cmds
    cmds --> inp -->|wl_keyboard / wl_pointer| app
    cmds --> cb <-->|data_device| app
    app -->|wl_surface.commit| buf
    buf --> rb
    buf -->|Snapshot + damage| frame
    frame --> renderer -->|changed rows| frame
    frame -->|Event: Frame band or SharedFrame| actor
    actor -->|PaneTransport byte budget| transport
    transport -->|ServerToPane| session
    session --> present --> presenter -->|Kitty APC escape codes| tty
    probe <--> tty
```

### Frame lifecycle

```mermaid
sequenceDiagram
    participant App as Wayland app
    participant C as Compositor
    participant R as Renderer
    participant S as Server transport
    participant P as Pane client
    participant T as Terminal

    App->>C: attach + damage + commit
    alt shm buffer
        C->>C: Snapshot::copy_shm, BGRA to RGBA
    else DMA-BUF
        C->>C: pre-commit blocker, export sync_file fences
        C->>C: GPU blit + PBO readback, native fence fd
    end
    C->>C: PaneState::damage marks pane Dirty
    Note over C: run loop calls Runtime::dispatch,<br/>then frame_callbacks and dispatch_frames
    C->>R: update scene, compose damaged strips
    R-->>C: changed row range
    alt shared transport and at least 1/4 of rows changed
        C->>S: SharedFrame named POSIX shm object
    else inline
        C->>S: FrameBand of changed RGB rows
    end
    S->>P: length-prefixed postcard packet
    P->>T: Kitty a=T whole image or a=f edit, zlib + base64
    P->>S: PaneAction::Ack Retained or Resync
    S->>C: pane_ack, InFlight to Clean or Dirty
    C->>App: wl_callback.done, paced by refresh_rate
```

Only one frame per pane is in flight at a time. The next frame is composed after the
pane acknowledges the current one, which gives end-to-end backpressure.
