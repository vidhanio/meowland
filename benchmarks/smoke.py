#!/usr/bin/env python3
"""Start a headless server, map the benchmark workload, close it, and stop the server."""

import argparse
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
WORKLOAD_APP_ID = "meowland-workload"
POLL_INTERVAL = 0.05


class SmokeError(RuntimeError):
    """Raised when a smoke-test step fails."""


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--meowland",
        type=Path,
        default=Path("target/debug/meowland"),
        help="path to the meowland binary",
    )
    parser.add_argument(
        "--workload",
        type=Path,
        default=Path("target/debug/meowland-workload"),
        help="path to the benchmark workload binary",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=10.0,
        help="seconds to wait for each step",
    )
    return parser.parse_args()


def resolve(path: Path) -> Path:
    return path if path.is_absolute() else (REPO_ROOT / path).resolve()


def command(
    meowland: Path,
    env: dict[str, str],
    *arguments: str,
    timeout: float,
) -> subprocess.CompletedProcess[str]:
    command_line = [str(meowland), *(str(argument) for argument in arguments)]
    try:
        return subprocess.run(
            command_line,
            cwd=REPO_ROOT,
            env=env,
            stdin=subprocess.DEVNULL,
            check=True,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
    except subprocess.CalledProcessError as error:
        detail = (error.stderr or error.stdout or "").strip()
        raise SmokeError(
            f"`{' '.join(command_line)}` failed with status {error.returncode}: {detail}"
        ) from error
    except subprocess.TimeoutExpired as error:
        raise SmokeError(f"`{' '.join(command_line)}` timed out") from error


def windows(meowland: Path, env: dict[str, str], timeout: float) -> dict[int, str]:
    listed = command(meowland, env, "list", timeout=timeout)
    found: dict[int, str] = {}
    for line in listed.stdout.splitlines():
        fields = line.split("\t")
        if not fields:
            continue
        try:
            identifier = int(fields[0])
        except ValueError:
            continue
        found[identifier] = fields[1] if len(fields) > 1 else ""
    return found


def wait_for(description: str, timeout: float, probe) -> object:
    deadline = time.monotonic() + timeout
    while True:
        result = probe()
        if result is not None:
            return result
        if time.monotonic() >= deadline:
            raise SmokeError(f"timed out waiting for the {description}")
        time.sleep(POLL_INTERVAL)


def main() -> int:
    arguments_ = arguments()
    meowland = resolve(arguments_.meowland)
    workload = resolve(arguments_.workload)
    root: Path | None = None
    server: subprocess.Popen | None = None
    listener: socket.socket | None = None
    connection: socket.socket | None = None
    log_path: Path | None = None
    try:
        for path in (meowland, workload):
            if not os.access(path, os.X_OK):
                raise SmokeError(f"{path} is not executable")

        root = Path(tempfile.mkdtemp(prefix="meowland-smoke-"))
        runtime = root / "runtime"
        runtime.mkdir(mode=0o700)
        for name in ("config", "state", "cache"):
            (root / name).mkdir()

        env = os.environ.copy()
        env.update(
            {
                "XDG_RUNTIME_DIR": str(runtime),
                "XDG_CONFIG_HOME": str(root / "config"),
                "XDG_STATE_HOME": str(root / "state"),
                "XDG_CACHE_HOME": str(root / "cache"),
                "RUST_LOG": "warn",
            }
        )
        for name in (
            "WAYLAND_DISPLAY",
            "DISPLAY",
            "LISTEN_FDS",
            "LISTEN_PID",
            "LISTEN_FDNAMES",
            "NOTIFY_SOCKET",
        ):
            env.pop(name, None)

        workload_socket = root / "workload.sock"
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(workload_socket))
        listener.listen(1)

        control = runtime / "meowland-control.sock"
        log_path = root / "server.log"
        with log_path.open("w") as log:
            server = subprocess.Popen(
                [str(meowland), "server"],
                cwd=REPO_ROOT,
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=log,
                stderr=subprocess.STDOUT,
            )
            print(f"smoke: started server as pid {server.pid}")

            def control_ready() -> bool | None:
                if server.poll() is not None:
                    raise SmokeError(f"server exited with status {server.returncode}")
                return True if control.exists() else None

            wait_for("control socket", arguments_.timeout, control_ready)
            print("smoke: control socket ready")

            command(
                meowland,
                env,
                "run",
                "--",
                str(workload),
                str(workload_socket),
                "640",
                "360",
                "sparse",
                timeout=arguments_.timeout,
            )
            print("smoke: workload launched")

            listener.settimeout(arguments_.timeout)
            connection, _ = listener.accept()
            print("smoke: workload connected")
            listener.close()
            listener = None

            def workload_window() -> int | None:
                found = [
                    identifier
                    for identifier, app_id in windows(
                        meowland, env, arguments_.timeout
                    ).items()
                    if app_id == WORKLOAD_APP_ID
                ]
                return found[0] if found else None

            identifier = wait_for(
                "workload window", arguments_.timeout, workload_window
            )
            print(f"smoke: window {identifier} mapped")

            command(meowland, env, "kill", str(identifier), timeout=arguments_.timeout)
            print(f"smoke: window {identifier} close requested")

            def window_closed() -> bool | None:
                if identifier in windows(meowland, env, arguments_.timeout):
                    return None
                return True

            wait_for("window to close", arguments_.timeout, window_closed)
            print(f"smoke: window {identifier} closed")

            server.terminate()
            status = server.wait(timeout=arguments_.timeout + 5.0)
            if status != 0:
                raise SmokeError(f"server exited with status {status} after SIGTERM")
            print("smoke: server exited cleanly")

        print("smoke: ok")
        return 0
    except (OSError, SmokeError, subprocess.SubprocessError, ValueError) as error:
        print(f"smoke: FAILED: {error}", file=sys.stderr)
        if log_path is not None and log_path.exists():
            text = log_path.read_text(errors="replace")
            if text.strip():
                print("--- server log ---", file=sys.stderr)
                print(text.rstrip(), file=sys.stderr)
                print("--- end server log ---", file=sys.stderr)
        return 1
    finally:
        if connection is not None:
            connection.close()
        if listener is not None:
            listener.close()
        if server is not None and server.poll() is None:
            server.kill()
            try:
                server.wait(timeout=5.0)
            except subprocess.TimeoutExpired:
                pass
        if root is not None:
            shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
