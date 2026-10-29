# Process benchmarks

Run from the repository root, in the Nix development shell (`direnv reload`,
then `direnv exec . …` when the calling shell does not load direnv automatically).
Python 3.11+, Linux `/proc`, pidfds, and POSIX shared memory are required. The
runner uses only Python's standard library. No benchmark service is installed.

## Commands

Quick iteration; the shim rebuilds the release `meowland` binary in the same
target directory before starting the harness:

```sh
cargo bench -p meowland-bench --bench suite
```

This runs eight cases with 3 warmups, 12 operations, and one fresh process tree per
case. For a focused, less noisy measurement:

```sh
cargo bench -p meowland-bench --bench suite -- \
  --filter sparse --samples 120 --warmup 8 --runs 3
```

Full run: 8 warmups, 120 operations, three fresh process trees per case, and the
additional high-resolution, transport, backpressure, attachment, and structural
cases:

```sh
cargo bench -p meowland-bench --bench suite -- --full
cargo bench -p meowland-bench --bench suite -- --full --list
```

`--filter` selects a case-name substring before starting processes. Explicit
`--samples`, `--warmup`, and `--runs` override the quick/full defaults. Cargo's
bench profile is optimized. The `meowland-bench` package builds the workload
helper; it is not part of the normal executable or installed Nix package.

### Comparing commits

Run the same harness and settings on each revision. Save JSON outside the
checkout if switching branches, or use separate worktrees with their own Cargo
target directories. There is no executable-path selection or two-binary mode:
Cargo supplies the workload path through `CARGO_BIN_EXE_meowland-workload`. The
suite shim builds the release `meowland` binary in the same target directory and
passes both paths to the harness.

On the baseline revision:

```sh
cargo bench -p meowland-bench --bench suite -- \
  --full --output /tmp/meowland-before.json
```

On the candidate revision:

```sh
cargo bench -p meowland-bench --bench suite -- \
  --full --output /tmp/meowland-after.json --compare /tmp/meowland-before.json
```

For revisions predating this harness, apply the same benchmark-only files and
manifest changes to a detached worktree without changing its production code.
Do not share a target directory between worktrees. Results record the revision,
dirty flag, tracked-diff hash, executable hashes, and harness-source hash.

The comparison prints the median primary metric across independent process
runs, repetition counts, and the candidate's range. Frame cases use per-run p50;
input uses input-to-response p50; streams use delivered throughput; static uses
output bytes; Vulkan uses changed-output throughput. Lower latency/bytes is
better, higher throughput is better. Environment/harness differences warn;
different endpoint tiers are rejected. This is not a significance test. Twelve
samples do not establish a meaningful p99. Use full runs, compare tails in the
JSON, alternate revision order, and repeat on an otherwise idle machine before
calling a small difference a regression.

### Actual Kitty rendering and readback

On an **unobscured X11/XWayland desktop**, with a display large enough for the
workload plus a small cell-rounding margin:

```sh
nix shell nixpkgs#kitty nixpkgs#xdotool nixpkgs#imagemagick -c \
  cargo bench -p meowland-bench --bench suite -- \
  --terminal kitty --filter input --samples 30 --output /tmp/meowland-kitty.json
```

For a reproducible software-rendered terminal check without using the desktop,
start a private Xvfb display (choose a free display number):

```sh
nix shell nixpkgs#kitty nixpkgs#xdotool nixpkgs#imagemagick nixpkgs#xorg-server -c \
  bash -c '
    Xvfb :199 -screen 0 2048x1200x24 -nolisten tcp >/tmp/meowland-xvfb.log 2>&1 &
    display_pid=$!
    trap "kill $display_pid; wait $display_pid 2>/dev/null" EXIT
    sleep 1
    DISPLAY=:199 LIBGL_ALWAYS_SOFTWARE=1 \
      cargo bench -p meowland-bench --bench suite -- \
      --terminal kitty --samples 12 --output /tmp/meowland-kitty-software.json
  '
```

The runner starts a fresh Kitty with no user configuration, launches
`meowland run` inside it, and reads the Kitty window through X11/ImageMagick. It
requires an exact, checksummed frame marker and checks image probes, including
sparse content. Color channels allow an error of one in readback because OpenGL
color conversion can round RGB values. Markers remain exact. Kitty's grid is
aligned top-left in a window requested 32 pixels larger than the application;
the window geometry is recorded and the application rectangle is verified.
This avoids clipping at fractional cell boundaries. Startup waits for window-manager
geometry to settle before taking readbacks. A constrained window, or resizing
after that startup phase, fails rather than silently changing the resolution.
Saved comparisons skip cases whose recorded readback geometry differs.

This tier measures **commit to verified window readback**, including capture,
process-launch, polling, and validation overhead. Input starts before an
`xdotool` invocation that sends a key to Kitty, so subprocess/X11 injection
cost is included. The application independently reports receipt of that input;
the matching response must appear in the window. This exercises actual Kitty
rendering, but is not a GPU fence timestamp, scanout time, hardware input time,
or input-to-photon measurement. With Xvfb/software OpenGL it is specifically
not a hardware-GPU benchmark. Environment metadata records software/driver
selection overrides; it does not infer a GPU renderer from the existence of a
render node. Hardware-driver identification can be collected separately with
the platform's graphics tools.

Readback is deliberately serial. Streaming/static cases and forced-inline
negotiation are skipped in this tier with explicit reasons. Missing tools or
`DISPLAY` also produce explicit skips. An available but malfunctioning terminal,
invalid pixels, or a readback timeout is a failure. Wayland-only screenshot
capture is not implemented. Do not compare Kitty readback latency with headless
consumption latency.

### Optional hardware Vulkan workload

```sh
nix shell nixpkgs#vulkan-tools -c \
  cargo bench -p meowland-bench --bench suite -- \
  --gpu --filter vulkan --output /tmp/meowland-vulkan.json
```

This discovers an integrated/discrete GPU with `vulkaninfo --summary`, launches
`vkcube --wsi wayland` through `meowland run`, discovers its window through the
isolated server's normal `list` command, and attaches the pane. Current vkcube
has no app ID/activation handoff, so it cannot use the fixture's combined
launch-and-attach path. The selected source GPU's hardware type must also be
confirmed in vkcube's own diagnostic output; software fallback is never counted
as GPU work. Enumeration order can differ with presentation environment, so
both discovery properties and vkcube's observed selection are retained. Missing
Vulkan tools/hardware skip the case. A discovered workload that fails to render
fails the run, rather than being replaced by SHM painting.

After 250 ms warmup it measures at least one second of changed, nonuniform RGB
updates consumed by the headless endpoint. This exercises a real Vulkan
Wayland client, compositor import/readback, frame transport, and pane output.
It has no source-frame IDs, so it reports **changed terminal updates/s**, not
source FPS, drop counts, commit latency, or GPU completion. Content sampling
can merge updates that happen to have identical sampled pixels. The optional
client's diagnostics are used only to reject software-device fallback; no
production compositor log parsing or mandatory runtime timing instrumentation
is involved. This is useful GPU/import smoke and throughput coverage, not a
controlled DMA-BUF/fence microbenchmark. It needs no media download or video
stack.

## Matrix and measurement boundaries

| Case | Purpose |
| --- | --- |
| `full-1080p` | Every RGB pixel changes; SHM negotiated, serial commit-to-consumption |
| `sparse-1080p` | Marker plus alternating 20×20 patch; accurately declared damage, retained composition/edit path |
| `tall-1080p` | 16×600 strip starting away from the left edge; nonzero-`x` inline rectangle, narrow enough to stay off SHM |
| `wide-1080p` | 1600×60 strip; inline full-row rectangle after the 75% widening rule |
| `input` | Armed key identity, independent application receipt, matching sparse rendered response at 640×360 |
| `static` | Identical buffer content recommitted at 120 Hz; requires zero measured image uploads |
| `inline` | 640×360 full frames, terminal rejects SHM; fast transport/encoding check |
| `stream` | 1080p SHM, offers at 120 Hz without waiting for each frame; bounded final drain |
| `full-4k` | Full-resolution bandwidth, allocation/copy/readback pressure |
| `inline-1080p` | Larger inline compression/base64/PTY path |
| `slow-inline` | 1080p at 120 Hz, 2 ms between PTY reads, reads at most 16 KiB; backpressure/coalescing |
| `attach` | Launch unattached through `run`, then use the real standalone `attach` command |
| `resize` | Alternate 640×360 and 320×180 surface buffers; verify cleared background outside the shrunken surface |
| `vulkan` | Optional hardware source workload, changed-output throughput only |

The first eight are quick defaults. `--full` adds the next five. `--gpu` explicitly
adds Vulkan to either selection. The slow case starts throttling **only after**
initial output, warmup, and settling. Its drain deadline derives from the
uncompressed output size, a conservative 4 KiB effective read, and the 2 ms
read interval; normal operations keep their five-second deadline. Server socket
startup has five seconds, initial frame/focus fifteen seconds, and individual
capture/control subprocesses have independent bounds. This separates startup
failure from intentionally slow output without inflating every timeout. Slow
4K is not another default: it exercises the same bottleneck at much greater
cost; 4K bandwidth already has its own case.

The fixture uses real Wayland SHM buffers, xdg-shell, keyboard events, and three
release-tracked backing files. Bounded patterns paint frame 0 outside their
declared damage in every backing buffer, so the workload obeys the buffer-damage
contract and a compositor's full reread stays as valid as an incremental one. Pixel IDs carry a checksum. The consumer checks
the marker plus distributed content-sensitive RGB probes, including both sides
of the sparse patch, the corners and edges of each bounded damage rectangle, and
the resized surface boundary. Pixels inside a bounded rectangle must carry the
current frame and pixels outside it must still carry the initial frame, so an
unexpected recomposition fails the case. This is sampled image validation, not
an exhaustive image equality proof. Full-content painting
and SHM writes occur **before** the commit timestamp. Commit timing starts just
before enqueueing `wl_surface.commit`, including subsequent flush/scheduling,
not when the server receives it. All fixture/runner timestamps use Linux
`CLOCK_MONOTONIC`. Input timing starts before PTY injection (or the X11 tool
invocation), not at a physical device.

The default endpoint reconstructs whole RGB images and in-place Kitty frame
edits, and records consumption after validation at the synchronized-output end.
Neither an escape write, acknowledgement, nor SHM unlink is treated as display
presentation. Python decode/copy/validation cost is included: especially for
inline transport, results characterize the combined path and consumer, not an
isolated compositor stage. Criterion is not used because these paced,
stateful external-process experiments need explicit operation IDs and drains,
not an adaptive tight-loop microbenchmark. No speculative hot-path
microbenchmarks were retained.

Each offered ID, actual commit ID, and consumed ID is accounted separately.
The fixture retains the newest pending request when buffers are unavailable;
`not_committed` counts offers superseded before commit.
`committed_not_observed` counts committed IDs absent from the drained endpoint;
it does not claim which production stage coalesced them. Only exact matching
IDs contribute latency samples. Warmup IDs are excluded. Static commits are
`expected_unchanged`, not dropped frames, and have no fabricated latency.
Throughput records actual offering-window rate, late offers (at least one period
late), delivered rate including drain, and drain duration. Streams always run
through at least their complete nominal offering window. Serial rates include
fixture preparation and round trips and are not unpaced compositor capacity.

JSON includes raw matched per-ID durations in nanoseconds, p50/p95/p99,
counts, PTY bytes, decoded RGB upload bytes, SHM/inline upload counts, workload
configuration, timestamps, executable/harness hashes, revision, CPU, kernel,
governors, toolchain, and endpoint metadata. It is atomically updated after each
run. The default destination is `target/benchmarks/TIMESTAMP.json`; `--output`
never overwrites an existing result. Adjacent `.artifacts/` directories retain
per-process logs, failure tracebacks, and, for timed-out readback, the last PPM.
A failed run exits nonzero; interrupted runs preserve partial results.

## Isolation and remaining limits

Every repetition gets private mode-0700 runtime and config/cache directories,
activation/service variables are removed, and D-Bus is pointed at an unavailable
private address to prevent desktop service autolaunch. The user's Meowland
sockets, service, and configuration are not used. Children are tracked through
`/proc` and stopped with pidfds, including applications that the server launches
in separate process groups. Shutdown first requests a fixture stop and sends
SIGTERM; after two seconds remaining children are killed and direct children
reaped. Known SHM names and names found in owned process FDs are cleaned up;
normal server shutdown releases producer-owned unpublished/pending objects.
No global SHM sweep can delete another session's resources. As with other
external-process harnesses, killing the runner itself with SIGKILL, or a producer
that cannot shut down before forced termination, cannot guarantee reclamation
of an unknown, already-closed named SHM object. SIGINT/SIGTERM take the cleanup
path. No leftover benchmark processes or Meowland SHM objects remained in the
validation runs, including deliberate interruption.

This matrix does not claim physical presentation, controlled GPU-fence timing,
exhaustive GPU formats, fractional scale, terminal-grid resizing, popup trees,
pointer input, input under simultaneous animation, CPU/RSS attribution, or
video decoding coverage. Surface shrink/expand was chosen as the compact
structural invalidation regression case. Add another case only when it isolates
a distinct suspected bottleneck; do not multiply every resolution, transport,
and input type. Runtime stage metrics remain available for investigation, but
benchmarks do not depend on their logs.
