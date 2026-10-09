# Readiness experiment

The implementation retains single-owner compositor/server state. It does not
introduce a general async runtime or change mouse parsing.

## Changes

- The compositor uses calloop to wait on its command channel, Wayland backend
  descriptor, listening socket, and the next frame/callback deadline. Incoming
  clients are accepted until the listener would block. An empty display no
  longer wakes every 16 ms just to discover that nothing changed.
- Pending clipboard transfers and implicit DMA-BUF fences still receive a
  16 ms progress check. Fully idle displays use a 60-second housekeeping cap;
  commands and Wayland activity wake them immediately.
- Pane output uses an independently opened nonblocking terminal descriptor.
  The inherited stdout flags remain unchanged. Encoding reuses the output
  allocation, and each writable turn drains at most 64 KiB. Partial writes and
  `WouldBlock` retain the exact byte offset.
- Titles, cursor commands, OSC 52, and graphics use the same ordered output
  buffer. Server reads and clipboard response draining pause while output is
  pending, preventing a slow terminal from accumulating further updates in
  the pane. Keyboard/mouse processing continues, with a 64-event turn budget.
- Frame accounting and acknowledgement happen only after the entire pending
  update has been written. Empty/dropped frames do not wait for writable
  readiness. Terminal acceptance of bytes is still the synchronization point,
  not proof that the terminal has displayed them.
- Server accept threads poll listener readiness instead of sleeping 10 ms.
  Each owns a cancellation socket; shutdown closes its peer to wake the poll
  immediately, then cancels/joins connection workers as before. A one-second
  housekeeping timeout reaps finished workers during idle periods.

## Measurements

Compared with commit `b31d661`, using the same Nix development shell and machine.
The baseline was built in a temporary worktree with the same benchmark tests.
These are local samples, not portable performance guarantees.

| Check | Original | Readiness implementation |
| --- | ---: | ---: |
| Idle Wayland sync median (200 roundtrips, release) | 16.059 ms | 4.1–5.0 µs |
| Idle Wayland sync p95 | 16.070 ms | 4.8–7.4 µs |
| Full 1080p pane frame throughput (5 seconds, release) | 59.8 fps | 60.0 fps |
| Control ping median, 1/8/64 idle panes (release) | about 10.07 ms | about 19–27 µs |
| Attach 8 idle panes sequentially (release) | 80.55 ms | 0.39–0.40 ms |
| Attach 64 idle panes sequentially (release) | 653.43 ms | 26–30 ms |
| Keyboard input while inline terminal output is stalled | timed out after 750 ms | 28–89 µs observed |

The keyboard check runs a real pane on a pty, declines shared-memory graphics,
then sends incompressible 512×512 RGB data without reading terminal output. It
requires keyboard press/release delivery before resuming output, rejects an
early frame acknowledgement, resumes output, and checks the final decoded
pixels and acknowledgement. It fails on the original synchronous writer.
The fake terminal was also corrected to decode continuation graphics chunks;
previous small-frame tests did not exercise them.

The throughput test uses pane sockets, not terminal encoding or GPU readback.
Its 60 Hz cap makes the tiny throughput difference uninteresting. The main
benefit is removing fixed latency and keeping input responsive under output
backpressure, not increasing maximum frame rate.

## Transport decision and remaining limits

No Tokio transport was implemented or benchmarked. The transport fanout test
found roughly 7/21/133 server threads at 1/8/64 panes, respectively, and roughly
6.6–8.1 MiB resident memory with GPU and Xwayland disabled. This is an idle
connection test, not a concurrent active-frame load test; stack address-space
reservations are not represented by resident memory.

The measurable control/attach latency came from the accept sleep and was fixed
without changing established-connection ownership. Two threads per pane remain
reasonable for small pane counts. Async transport may still be worthwhile for
large counts, but should be compared using concurrent active and stalled
connections, CPU/RSS/virtual-memory measurements, and shutdown latency.

Other limits remain explicit:

- Frame encoding, composition, GPU snapshot/readback, and terminal setup and
  restoration are synchronous. Input cannot run during a long encode/readback.
- Pane-to-server writes are still blocking, so a server that stops reading can
  delay pane input handling. This experiment addresses terminal backpressure,
  not arbitrary backpressure in both directions.
- Single-in-flight framing bounds queued frames, and protocol payloads and
  clipboard worker queues have bounds. Existing server metadata/command mpsc
  queues are not bounded queues, however. Any async transport follow-up should
  define bounded/coalesced metadata queues and explicit overflow semantics,
  rather than copy those unbounded queues into async tasks.
- Clipboard pipes and DMA-BUF fences are periodically checked, not yet
  registered as independent readiness sources.

## Reproduce

```sh
cargo test --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo test --release --locked --test wayland -- --ignored --nocapture --test-threads=1
cargo test --release --locked --test readiness -- --nocapture
```

The ignored tests are observational benchmarks without narrow latency
assertions. The regular stalled-terminal regression is included in `cargo test`.
