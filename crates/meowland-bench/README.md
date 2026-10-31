# meowland-bench

End-to-end benchmarks and the headless smoke test. Every case runs the real
process tree: `meowland server`, a pane process (`meowland launch` or
`meowland focus`) on a pseudo-terminal, and the `meowland-workload` Wayland
SHM fixture. The other end of the terminal is a headless Kitty-graphics
endpoint in the harness that answers the pane's capability probe, rebuilds
every image and frame edit, and validates it before counting the frame.

## Running

```sh
cargo bench -p meowland-bench                # whole matrix, a few minutes
cargo bench -p meowland-bench -- sparse      # cases whose ID contains "sparse"
cargo bench -p meowland-bench -- --test      # run every case once, no statistics
cargo test -p meowland-bench                 # smoke test and unit tests
```

The suite first runs `cargo build -p meowland` with the bench profile into the
same target directory, so the measured compositor is always the checked-out
one. The smoke test does the same with the test profile.

## Comparing revisions

Criterion keeps baselines under `target/criterion`:

```sh
git switch main
cargo bench -p meowland-bench -- --save-baseline main
git switch my-branch
cargo bench -p meowland-bench -- --baseline main
```

Criterion then reports each case's change with a confidence interval and
p-value. Compare on an otherwise idle machine and rerun before trusting a
small change.

## Cases

All panes use 10×20-pixel cells. One session per case is started on first use
and reused for every warm-up and measurement batch.

| Case           | Workload                                           | Required uploads       | One iteration                   |
| -------------- | -------------------------------------------------- | ---------------------- | ------------------------------- |
| `full-1080p`   | 1920×1080, every pixel changes                     | SHM                    | commit → consumed               |
| `sparse-1080p` | 1920×1080, marker plus an alternating 20×20 patch  | inline (SHM offered)   | commit → consumed               |
| `input`        | 640×360 sparse, responds to `a`                    | inline (SHM offered)   | key write → response consumed   |
| `static`       | 640×360 sparse, displayed content recommitted      | none                   | commit → buffer release         |
| `inline`       | 640×360, every pixel changes                       | inline (SHM refused)   | commit → consumed               |
| `full-4k`      | 3840×2160, every pixel changes                     | SHM                    | commit → consumed               |
| `resize`       | 640×360 alternating with 320×180                   | SHM                    | commit → consumed               |
| `stream`       | 1080p full frames offered at 120 Hz                | SHM                    | one consumed frame (throughput) |
| `slow-inline`  | as `stream`; reads ≤ 16 KiB every 2 ms once warm   | inline (SHM refused)   | one consumed frame (throughput) |

"SHM" requires at least one shared-memory upload per batch, "inline" requires
inline uploads only, and `static` must upload nothing and consume no new frame.
Any violation, invalid pixels, or a step that takes over five seconds panics
and fails the run.

`slow-inline` consumes roughly one frame every four seconds, so its ten samples
dominate the run and Criterion warns that it overruns its target time.

## Measurement boundaries

- Samples are differences of `CLOCK_MONOTONIC` timestamps taken by the
  processes involved, not wall time around the loop.
- A commit is stamped by the workload after painting and writing its SHM
  buffer, just before `wl_surface.commit`; painting is excluded.
- A frame is consumed when synchronized output ends and the endpoint has read
  the SHM object or decoded the inline data, applied edits, identified the
  checksummed marker, and checked content probes (both sides of the sparse
  patch, corners, a diagonal, the area cleared by a shrink). Decoding and
  validation are therefore included; terminal rendering, GPU work, and
  presentation are not.
- Input starts just before the Kitty keyboard press is written to the
  terminal, not at a physical device.
- `static` ends when the workload receives `wl_buffer.release`, which the
  server flushes after the dispatch turn that handled the commit.
- Stream iterations run from the batch's first offer to its N-th consumed
  frame, so coalesced offers lengthen the time per frame. Draining the last
  offer happens after the sample. `slow-inline` starts throttling only after
  warm-up.
- Startup and three warm-up operations per session are never sampled.

## Isolation

Each session gets private mode-0700 runtime, config, cache, and state
directories; `MEOWLAND_*`, socket-activation, notify, Wayland, X11, and
activation-token variables are removed; and D-Bus points at a socket that does
not exist. The user's server, configuration, and desktop services are never
touched. Children run in their own sessions and get `SIGTERM` if the harness
thread that started them dies. Dropping a session, including on panic, stops
the workload, sends `SIGTERM` to every process it owns (including clients the
server launched), kills survivors after two seconds, reaps its children, and
unlinks `meowland-*` shared-memory objects those processes still held.
Killing the harness with `SIGKILL` can still strand an SHM object nobody holds.
