"""Process-level benchmarks launched by Cargo; compare JSON results saved on separate commits."""

import argparse
import hashlib
import json
import math
import os
import platform
import re
import shutil
import signal
import statistics
import subprocess
import sys
import time
import traceback
from datetime import UTC, datetime
from pathlib import Path

from session import Session, terminal_requirement


def cases(full):
    def case(
        name,
        kind="latency",
        pattern="full",
        width=1920,
        height=1080,
        transport="shared",
    ):
        return {
            "name": name,
            "kind": kind,
            "pattern": pattern,
            "width": width,
            "height": height,
            "transport": transport,
        }

    result = [
        case("full-1080p"),
        case("sparse-1080p", pattern="sparse"),
        case("input", kind="input", pattern="sparse", width=640, height=360),
        case("static", kind="static", pattern="sparse", width=640, height=360),
        case("inline", width=640, height=360, transport="inline"),
        case("stream", kind="stream"),
    ]
    if full:
        result += [
            case("full-4k", width=3840, height=2160),
            case("inline-1080p", transport="inline"),
            case("slow-inline", kind="slow", transport="inline"),
            case("attach", pattern="sparse", width=640, height=360),
            case("resize", pattern="resize", width=640, height=360),
        ]
    return result


def percentile(values, percent):
    if not values:
        return None
    return sorted(values)[max(0, math.ceil(len(values) * percent / 100) - 1)]


def stats(values):
    return {
        f"p{p}_ms": None if not values else percentile(values, p) / 1e6
        for p in (50, 95, 99)
    }


def frame(session, identity):
    session.command("commit", identity)
    session.until(
        lambda: identity in session.commits and identity in session.frames,
        f"frame {identity}",
    )


def interaction(session, identity):
    session.command("arm", identity)
    session.until(lambda: identity in session.armed, f"arming input {identity}")
    started = session.input()
    session.until(
        lambda: (
            identity in session.inputs
            and identity in session.frames
            and identity in session.commits
        ),
        f"input response {identity}",
    )
    session.release()
    return started


def execute(session, args):
    case = session.case
    session.start()
    if case["kind"] == "gpu":
        session.settle(0.25)
        before, initial = session.counters(), len(session.frames)
        started = time.monotonic_ns()
        session.settle(max(1, args.samples / 60))
        elapsed = (time.monotonic_ns() - started) / 1e9
        updates = len(session.frames) - initial
        if updates == 0:
            raise RuntimeError("Vulkan workload stopped changing")
        log = (session.directory / "server.log").read_text(errors="replace")
        selection = re.search(
            r"Selected GPU[^\n]+type: (?:IntegratedGpu|DiscreteGpu)[^\n]*", log
        )
        if selection is None:
            raise RuntimeError(
                "vkcube did not confirm an integrated/discrete GPU; refusing unverified software fallback"
            )
        return {
            "status": "ok",
            "elapsed_s": elapsed,
            "observed_updates": updates,
            "updates_per_s": updates / elapsed,
            "observed_gpu": selection.group(),
            "counters": {
                key: value - before[key] for key, value in session.counters().items()
            },
            "latency": stats([]),
            "samples": [],
        }
    for identity in range(1, args.warmup + 1):
        if case["kind"] == "input":
            interaction(session, identity)
        else:
            frame(session, identity)
    session.settle(0.05)
    before = session.counters()
    session.throttle = case["kind"] == "slow"
    first = args.warmup + 1
    last = args.warmup + args.samples
    offered = {}
    late = 0
    input_starts = {}
    started = time.monotonic_ns()
    if case["kind"] in ("stream", "slow", "static"):
        for identity in range(first, last + 1):
            scheduled = started + (identity - first) * 1_000_000_000 // 120
            while time.monotonic_ns() < scheduled:
                session.pump(min((scheduled - time.monotonic_ns()) / 1e9, 0.005))
            at = time.monotonic_ns()
            late += at - scheduled >= 1_000_000_000 // 120
            offered[identity] = at
            if case["kind"] == "static":
                session.command("repeat", identity, str(args.warmup))
            else:
                session.command("commit", identity)
            session.pump(0)
        if case["kind"] == "static":
            session.until(lambda: last in session.commits, "final unchanged commit")
            session.settle(0.3)
        else:
            drain = 5
            if case["kind"] == "slow":
                drain += case["width"] * case["height"] * 4 / 4096 * 0.002 * 3
            session.until(
                lambda: last in session.frames and last in session.commits,
                "last offered frame",
                seconds=drain,
            )
    else:
        for identity in range(first, last + 1):
            if case["kind"] == "input":
                input_starts[identity] = interaction(session, identity)
            else:
                offered[identity] = time.monotonic_ns()
                frame(session, identity)
    if case["kind"] in ("stream", "slow"):
        remaining = (
            started + args.samples * 1_000_000_000 // 120 - time.monotonic_ns()
        ) / 1e9
        if remaining > 0:
            session.settle(remaining)
    ended = time.monotonic_ns()
    measured_ids = set(range(first, last + 1))
    committed_ids = measured_ids & session.commits.keys()
    delivered_ids = measured_ids & session.frames.keys()
    if not delivered_ids <= committed_ids:
        raise RuntimeError("received an image without its matching workload commit")
    raw = []
    for identity in sorted(delivered_ids):
        commit, end = session.commits[identity], session.frames[identity]
        if end < commit:
            raise RuntimeError("frame observed before commit")
        sample = {"id": identity, "commit_to_endpoint_ns": end - commit}
        if identity in offered:
            sample["offer_to_endpoint_ns"] = end - offered[identity]
        if identity in input_starts:
            delivery = session.inputs[identity] - input_starts[identity]
            response = end - input_starts[identity]
            if delivery < 0 or response < delivery:
                raise RuntimeError("invalid input/frame correlation")
            sample.update(
                input_to_application_ns=delivery, input_to_endpoint_ns=response
            )
        raw.append(sample)
    after = session.counters()
    counters = {key: value - before[key] for key, value in after.items()}
    if args.terminal == "headless":
        if case["transport"] == "inline" and counters["shared_uploads"]:
            raise RuntimeError("inline case unexpectedly used SHM")
        if (
            case["pattern"] == "full"
            and case["transport"] == "shared"
            and not counters["shared_uploads"]
        ):
            raise RuntimeError("shared case never used SHM")
        if (
            case["kind"] == "static"
            and counters["shared_uploads"] + counters["inline_uploads"] != 0
        ):
            raise RuntimeError("unchanged commits unexpectedly emitted image uploads")
    if case["kind"] != "static" and not raw:
        raise RuntimeError("no correlated frames")
    elapsed = (ended - started) / 1e9
    offer_window = elapsed
    if case["kind"] in ("stream", "slow", "static"):
        offer_window = (offered[last] - started) / 1e9 + 1 / 120
    return {
        "status": "ok",
        "elapsed_s": elapsed,
        "offer_window_s": offer_window,
        "drain_s": max(0, elapsed - offer_window),
        "offered": args.samples,
        "committed": len(committed_ids),
        "delivered": len(delivered_ids),
        "not_committed": args.samples - len(committed_ids),
        "expected_unchanged": len(committed_ids) if case["kind"] == "static" else 0,
        "committed_not_observed": len(committed_ids - delivered_ids)
        if case["kind"] != "static"
        else 0,
        "late_offers": late,
        "offered_per_s": args.samples / offer_window,
        "delivered_per_s": len(delivered_ids) / elapsed,
        "latency": stats([sample["commit_to_endpoint_ns"] for sample in raw]),
        "input_delivery": stats(
            [
                sample["input_to_application_ns"]
                for sample in raw
                if "input_to_application_ns" in sample
            ]
        ),
        "input_response": stats(
            [
                sample["input_to_endpoint_ns"]
                for sample in raw
                if "input_to_endpoint_ns" in sample
            ]
        ),
        "counters": counters if args.terminal == "headless" else None,
        "samples": raw,
        "window_size": session.window_size,
    }


def command_output(command):
    try:
        result = subprocess.run(
            command, check=False, capture_output=True, text=True, timeout=5
        )
        return result.stdout.strip() if result.returncode == 0 else None
    except (OSError, subprocess.TimeoutExpired):
        return None


def gpu_requirement(case):
    missing = [tool for tool in ("vkcube", "vulkaninfo") if not shutil.which(tool)]
    if missing:
        return "missing optional Vulkan tools: " + ", ".join(missing)
    summary = command_output(["vulkaninfo", "--summary"])
    if not summary:
        return "Vulkan device discovery failed"
    for number, properties in re.findall(
        r"GPU(\d+):\s*\n(.*?)(?=\nGPU\d+:|\Z)", summary, re.DOTALL
    ):
        if re.search(
            r"deviceType\s*=\s*PHYSICAL_DEVICE_TYPE_(DISCRETE|INTEGRATED)_GPU",
            properties,
        ):
            case.update(gpu_number=int(number), vulkan_properties=properties.strip())
            return None
    return "no integrated/discrete Vulkan GPU discovered; software fallback is not a GPU benchmark"


def metadata(args):
    root = Path(__file__).resolve().parent.parent
    binary = Path(os.environ["MEOWLAND_BENCH_BINARY"])
    sources = sorted((root / "benchmarks").glob("*.py")) + sorted(
        (root / "crates/meowland/benchmarks").rglob("*.rs")
    )
    harness_hash = hashlib.sha256(
        b"".join(source.read_bytes() for source in sources)
    ).hexdigest()
    diff = command_output(["git", "-C", str(root), "diff", "HEAD", "--"])
    return {
        "created_utc": datetime.now(UTC).isoformat(),
        "commit": command_output(["git", "-C", str(root), "rev-parse", "HEAD"]),
        "harness_sha256": harness_hash,
        "tracked_diff_sha256": hashlib.sha256((diff or "").encode()).hexdigest(),
        "dirty": bool(
            command_output(["git", "-C", str(root), "status", "--porcelain"])
        ),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "workload_sha256": hashlib.sha256(
            Path(os.environ["MEOWLAND_BENCH_WORKLOAD"]).read_bytes()
        ).hexdigest(),
        "platform": platform.platform(),
        "machine": platform.machine(),
        "python": platform.python_version(),
        "cpu": next(
            (
                line.split(":", 1)[1].strip()
                for line in Path("/proc/cpuinfo").read_text().splitlines()
                if line.startswith("model name")
            ),
            None,
        ),
        "governors": {
            p.parent.name: p.read_text().strip()
            for p in Path("/sys/devices/system/cpu/cpufreq").glob(
                "policy*/scaling_governor"
            )
        },
        "rustc": command_output(["rustc", "-Vv"]),
        "terminal": args.terminal,
        "display": os.environ.get("DISPLAY") if args.terminal == "kitty" else None,
        "gl_environment": {
            key: os.environ.get(key)
            for key in (
                "LIBGL_ALWAYS_SOFTWARE",
                "DRI_PRIME",
                "MESA_LOADER_DRIVER_OVERRIDE",
            )
        },
        "input_start": "xdotool invocation before X11 key injection"
        if args.terminal == "kitty"
        else "before PTY input write",
        "kitty_version": command_output(["kitty", "--version"])
        if args.terminal == "kitty"
        else None,
        "render_nodes": [p.name for p in Path("/dev/dri").glob("renderD*")],
        "parameters": {
            "samples": args.samples,
            "warmup": args.warmup,
            "runs": args.runs,
            "full": args.full,
        },
        "endpoint": "verified Kitty X11 window readback (includes capture/polling overhead)"
        if args.terminal == "kitty"
        else "verified RGB reconstruction at synchronized-output end; no terminal GPU rendering",
        "pacing_hz": 120,
        "slow_read_delay_ms": 2,
        "slow_read_max_bytes": 16384,
    }


def save(path, data):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(data, indent=2) + "\n")
    temporary.replace(path)


def summarize(row):
    if row["status"] != "ok":
        print(
            f"{row['case']['name']:<18} {row['status']}: {row.get('reason', row.get('error', ''))}",
            flush=True,
        )
        return
    if row["case"]["kind"] == "gpu":
        print(
            f"vulkan             r{row['run']} {row['observed_updates']} changed terminal updates "
            f"{row['updates_per_s']:.1f}/s (no source-frame latency or drop inference)",
            flush=True,
        )
        return
    value = row["latency"]
    text = (
        "no changed frames"
        if value["p50_ms"] is None
        else " / ".join(f"{value[f'p{p}_ms']:.3f}" for p in (50, 95, 99)) + " ms"
    )
    print(
        f"{row['case']['name']:<18} r{row['run']} p50/95/99 {text:<31} "
        f"{row['delivered']:>3}/{row['offered']} observed {row['delivered_per_s']:>6.1f}/s "
        f"uncommitted={row['not_committed']} unobserved={row['committed_not_observed']}",
        flush=True,
    )
    if row["counters"] is not None:
        counters = row["counters"]
        print(
            f"  PTY={counters['output_bytes'] / 1e6:.3f} MB "
            f"SHM/inline uploads={counters['shared_uploads']}/{counters['inline_uploads']} "
            f"offered={row['offered_per_s']:.1f}/s drain={row['drain_s']:.3f}s",
            flush=True,
        )
    if row["case"]["kind"] == "input":
        print(
            f"  input p50: application={row['input_delivery']['p50_ms']:.3f} ms "
            f"endpoint={row['input_response']['p50_ms']:.3f} ms",
            flush=True,
        )


def comparison_values(data, case, field, statistic):
    values = []
    for row in data["results"]:
        if row["case"] == case and row["status"] == "ok":
            value = row[field][statistic] if statistic else row[field]
            if value is not None:
                values.append(value)
    return values


def compare(current, path):
    previous = json.loads(path.read_text())
    if previous.get("schema") != current["schema"]:
        raise RuntimeError("comparison schema mismatch")
    if previous["environment"]["terminal"] != current["environment"]["terminal"]:
        raise RuntimeError("cannot compare different measurement endpoints")
    for key in (
        "parameters",
        "harness_sha256",
        "cpu",
        "platform",
        "gl_environment",
        "kitty_version",
    ):
        if previous["environment"].get(key) != current["environment"].get(key):
            print(f"WARNING: comparison environment field differs: {key}", flush=True)
    print(
        f"\nSaved-result comparison with {path} (median primary metric across process runs; not a significance test):",
        flush=True,
    )
    for case in {
        row["case"]["name"]: row["case"] for row in current["results"]
    }.values():
        geometries = [
            {
                tuple(row.get("window_size") or ())
                for row in result["results"]
                if row["case"] == case and row["status"] == "ok"
            }
            for result in (previous, current)
        ]
        if geometries[0] and geometries[1] and geometries[0] != geometries[1]:
            print(
                f"WARNING: skipping {case['name']}: window readback geometry differs",
                flush=True,
            )
            continue
        field, statistic, unit = {
            "input": ("input_response", "p50_ms", "ms p50 response"),
            "stream": ("delivered_per_s", None, "frames/s"),
            "slow": ("delivered_per_s", None, "frames/s"),
            "gpu": ("updates_per_s", None, "updates/s"),
            "static": ("counters", "output_bytes", "PTY bytes"),
        }.get(case["kind"], ("latency", "p50_ms", "ms p50"))
        old = comparison_values(previous, case, field, statistic)
        new = comparison_values(current, case, field, statistic)
        if old and new:
            a, b = statistics.median(old), statistics.median(new)
            change = f"{100 * (b / a - 1):+.1f}%" if a else f"delta {b - a:+.3f}"
            print(
                f"{case['name']:<18} {a:.3f} -> {b:.3f} {unit} {change} "
                f"(runs {len(old)}/{len(new)}; current range {min(new):.3f}..{max(new):.3f})",
                flush=True,
            )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--full",
        action="store_true",
        help="120 samples, 8 warmups, 3 repetitions; add 4K, inline and slow-terminal cases",
    )
    parser.add_argument("--samples", type=int)
    parser.add_argument("--warmup", type=int)
    parser.add_argument("--runs", type=int)
    parser.add_argument("--filter", default="", help="case-name substring")
    parser.add_argument("--terminal", choices=("headless", "kitty"), default="headless")
    parser.add_argument(
        "--output",
        type=Path,
        help="JSON result path; default target/benchmarks/TIMESTAMP.json",
    )
    parser.add_argument(
        "--compare",
        type=Path,
        help="summarize changes against a saved result from another commit",
    )
    parser.add_argument(
        "--gpu",
        action="store_true",
        help="add optional hardware Vulkan workload; requires vkcube and vulkaninfo",
    )
    parser.add_argument("--list", action="store_true")
    parser.add_argument("--bench", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    args.samples = (
        args.samples if args.samples is not None else (120 if args.full else 12)
    )
    args.warmup = args.warmup if args.warmup is not None else (8 if args.full else 3)
    args.runs = args.runs if args.runs is not None else (3 if args.full else 1)
    if not (
        1 <= args.samples <= 10000
        and 1 <= args.warmup <= 1000
        and 1 <= args.runs <= 100
    ):
        parser.error("require samples 1..10000, warmup 1..1000, runs 1..100")
    matrix = cases(args.full)
    if args.gpu:
        matrix.append(
            {
                "name": "vulkan",
                "kind": "gpu",
                "pattern": "gpu",
                "width": 640,
                "height": 360,
                "transport": "shared",
            }
        )
    selected = [case for case in matrix if args.filter in case["name"]]
    if not selected:
        parser.error("no matching cases")
    if args.list:
        for case in selected:
            print(json.dumps(case))
        return 0
    if (
        "MEOWLAND_BENCH_BINARY" not in os.environ
        or "MEOWLAND_BENCH_WORKLOAD" not in os.environ
    ):
        parser.error(
            "launch through cargo bench -p meowland --features benchmarks --bench suite -- [options]"
        )
    path = args.output or Path("target/benchmarks") / (
        datetime.now(UTC).strftime("%Y%m%dT%H%M%S%fZ") + ".json"
    )
    path = path.resolve()
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        parser.error(f"refusing to overwrite {path}")
    artifacts = path.with_suffix(".artifacts")
    artifacts.mkdir()
    data = {"schema": 1, "environment": metadata(args), "results": []}

    def interrupt(_number, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupt)
    print(data["environment"]["endpoint"], flush=True)
    print(f"Results: {path}\nLogs: {artifacts}", flush=True)
    failure = False
    completed = False
    try:
        for case in selected:
            reason = gpu_requirement(case) if case["kind"] == "gpu" else None
            if args.terminal == "kitty":
                reason = terminal_requirement()
                if reason is None and (
                    case["kind"] not in ("latency", "input")
                    or case["transport"] != "shared"
                ):
                    reason = "window-readback tier supports serial frame/input latency only; capture cannot observe every streamed frame or force Kitty transport"
            if reason:
                row = {"case": case, "status": "skipped", "reason": reason}
                data["results"].append(row)
                summarize(row)
                save(path, data)
                continue
            for repetition in range(1, args.runs + 1):
                directory = artifacts / f"{case['name']}-{repetition}"
                directory.mkdir()
                session = Session(case, args, directory)
                try:
                    row = execute(session, args)
                    row.update(case=case, run=repetition)
                except (
                    OSError,
                    ValueError,
                    RuntimeError,
                    subprocess.SubprocessError,
                ) as error:
                    row = {
                        "case": case,
                        "run": repetition,
                        "status": "failed",
                        "error": str(error),
                        "logs": str(directory),
                    }
                    (directory / "failure.txt").write_text(traceback.format_exc())
                    failure = True
                finally:
                    try:
                        session.close()
                    except (
                        OSError,
                        ValueError,
                        RuntimeError,
                        subprocess.SubprocessError,
                    ) as error:
                        row = {
                            "case": case,
                            "run": repetition,
                            "status": "failed",
                            "error": f"cleanup: {error}",
                            "logs": str(directory),
                        }
                        (directory / "cleanup-failure.txt").write_text(
                            traceback.format_exc()
                        )
                        failure = True
                data["results"].append(row)
                summarize(row)
                save(path, data)
                if failure:
                    return 1
        if args.compare:
            compare(data, args.compare)
        completed = True
    finally:
        data["status"] = (
            "failed" if failure else ("complete" if completed else "interrupted")
        )
        save(path, data)
    return int(failure)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"benchmark failed: {error}", file=sys.stderr)
        traceback.print_exc()
        sys.exit(1)
