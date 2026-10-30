"""Isolated process trees, a deterministic PTY consumer, and optional X11 window readback."""

import errno
import fcntl
import os
import pty
import selectors
import shutil
import signal
import socket
import struct
import subprocess
import tempfile
import termios
import time
import tty
from pathlib import Path

from graphics import Graphics, identify, marker_at, verify


def descendants(pid):
    found = set()
    pending = [pid]
    while pending:
        parent = pending.pop()
        try:
            children = (
                Path(f"/proc/{parent}/task/{parent}/children").read_text().split()
            )
        except OSError:
            continue
        for child in map(int, children):
            if child not in found:
                found.add(child)
                pending.append(child)
    return found


class Session:
    def __init__(self, case, args, directory):
        self.case, self.args, self.directory = case, args, directory
        self.processes = []
        self.logs = []
        self.selector = selectors.DefaultSelector()
        self.runtime = tempfile.TemporaryDirectory(prefix="meowland-bench-")
        self.root = Path(self.runtime.name)
        self.env = os.environ.copy()
        for key in list(self.env):
            if key.startswith(("LISTEN_", "WATCHDOG_", "MEOWLAND_")) or key in (
                "NOTIFY_SOCKET",
                "WAYLAND_SOCKET",
                "WAYLAND_DISPLAY",
                "XDG_ACTIVATION_TOKEN",
                "DESKTOP_STARTUP_ID",
                "DBUS_SESSION_BUS_ADDRESS",
            ):
                self.env.pop(key)
        self.env.update(
            XDG_RUNTIME_DIR=str(self.root),
            XDG_CONFIG_HOME=str(self.root / "config"),
            XDG_CACHE_HOME=str(self.root / "cache"),
            RUST_LOG="warn",
            TERM="xterm-kitty",
            COLORTERM="truecolor",
            DBUS_SESSION_BUS_ADDRESS=f"unix:path={self.root}/no-bus",
            NO_AT_BRIDGE="1",
        )
        self.binary = os.environ["MEOWLAND_BENCH_BINARY"]
        self.workload = os.environ["MEOWLAND_BENCH_WORKLOAD"]
        self.graphics = Graphics(
            case["width"], case["height"], case["transport"], case["pattern"]
        )
        self.commits, self.frames, self.inputs, self.armed = {}, {}, {}, set()
        self.ready = False
        self.control = None
        self.control_buffer = bytearray()
        self.master = None
        self.window = None
        self.window_size = None
        self.throttle = False
        self.next_read = 0
        self.app_pid = None
        self.last_rgb = None

    def spawn(self, command, name, **kwargs):
        log = (self.directory / f"{name}.log").open("wb")
        self.logs.append(log)
        process = subprocess.Popen(
            command, env=self.env, stderr=log, start_new_session=True, **kwargs
        )
        self.processes.append(process)
        return process

    def start(self):
        server = self.spawn(
            [self.binary, "server"],
            "server",
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
        )
        self.until(
            lambda: (self.root / "meowland-pane.sock").exists(),
            "server socket",
            seconds=5,
        )
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(str(self.root / "workload.sock"))
        self.listener.listen(1)
        self.listener.setblocking(False)
        self.selector.register(self.listener, selectors.EVENT_READ, "listener")
        launch = [
            self.binary,
            "launch",
            "--app-id",
            "meowland-workload",
            "--",
            self.workload,
            str(self.root / "workload.sock"),
            str(self.case["width"]),
            str(self.case["height"]),
            self.case["pattern"],
        ]
        if self.case["kind"] == "gpu":
            launch = [
                self.binary,
                "launch",
                "--",
                "vkcube",
                "--wsi",
                "wayland",
                "--width",
                str(self.case["width"]),
                "--height",
                str(self.case["height"]),
                "--gpu_number",
                str(self.case["gpu_number"]),
            ]
        if self.case["name"] == "attach" or self.case["kind"] == "gpu":
            launcher = self.spawn(
                launch, "launch", stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL
            )
            launcher.wait(timeout=5)
            if launcher.returncode:
                raise RuntimeError("meowland launch failed before focus")
            self.processes.remove(launcher)
            focus = ["--follow", "--app-id", "meowland-workload"]
            if self.case["kind"] == "gpu":
                deadline = time.monotonic() + 5
                while True:
                    listing = subprocess.run(
                        [self.binary, "list"],
                        check=False,
                        env=self.env,
                        capture_output=True,
                        text=True,
                        timeout=2,
                    )
                    if listing.returncode == 0 and listing.stdout.strip():
                        focus = [listing.stdout.split("\t", 1)[0]]
                        break
                    if time.monotonic() > deadline:
                        raise RuntimeError("Vulkan application did not create a window")
                    self.pump(0.01)
            launch = [self.binary, "focus", *focus]
        if self.args.terminal == "headless":
            master, slave = pty.openpty()
            self.master = master
            tty.setraw(slave)
            size = struct.pack(
                "HHHH",
                self.case["height"] // 20,
                self.case["width"] // 10,
                self.case["width"],
                self.case["height"],
            )
            fcntl.ioctl(slave, termios.TIOCSWINSZ, size)
            os.set_blocking(master, False)
            self.selector.register(master, selectors.EVENT_READ, "pty")
            try:
                self.spawn(launch, "pane", stdin=slave, stdout=slave)
            finally:
                os.close(slave)
        else:
            title = f"meowland-bench-{server.pid}"
            options = {
                "linux_display_server": "x11",
                "remember_window_size": "no",
                "initial_window_width": str(self.case["width"] + 32),
                "initial_window_height": str(self.case["height"] + 32),
                "placement_strategy": "top-left",
                "window_padding_width": "0",
                "window_margin_width": "0",
                "hide_window_decorations": "yes",
                "background_opacity": "1",
                "confirm_os_window_close": "0",
                "enable_audio_bell": "no",
            }
            command = ["kitty", "--config", "NONE", "--class", title, "--title", title]
            for key, value in options.items():
                command += ["--override", f"{key}={value}"]
            self.spawn(
                command + launch,
                "kitty",
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
            )
            deadline = time.monotonic() + 10
            while self.window is None:
                self.pump(0.02)
                result = subprocess.run(
                    ["xdotool", "search", "--onlyvisible", "--class", title],
                    check=False,
                    capture_output=True,
                    timeout=2,
                )
                if result.returncode == 0 and result.stdout.strip():
                    self.window = result.stdout.splitlines()[-1].decode()
                if time.monotonic() > deadline:
                    raise RuntimeError(
                        "Kitty X11 window did not appear; inspect kitty.log"
                    )
            previous = None
            stable_since = time.monotonic()
            deadline = stable_since + 5
            while True:
                geometry = subprocess.run(
                    ["xdotool", "getwindowgeometry", "--shell", self.window],
                    capture_output=True,
                    text=True,
                    check=True,
                    timeout=2,
                )
                fields = dict(
                    line.split("=", 1) for line in geometry.stdout.splitlines()
                )
                size = (int(fields["WIDTH"]), int(fields["HEIGHT"]))
                if size != previous:
                    previous, stable_since = size, time.monotonic()
                if time.monotonic() - stable_since >= 0.25:
                    self.window_size = size
                    break
                if time.monotonic() > deadline:
                    raise RuntimeError(
                        "Kitty window geometry did not settle during startup"
                    )
                self.pump(0.05)
            if (
                self.window_size[0] < self.case["width"] + 16
                or self.window_size[1] < self.case["height"] + 16
            ):
                raise RuntimeError(
                    "Kitty window is constrained; use an X11 desktop larger than the workload or Xvfb"
                )
        if self.case["kind"] == "gpu":
            self.until(
                lambda: bool(self.frames), "nonuniform Vulkan image output", seconds=15
            )
        else:
            self.until(
                lambda: self.ready and 0 in self.frames,
                "initial content and keyboard focus",
                seconds=15,
            )

    def check(self):
        for process in self.processes:
            if process.poll() is not None:
                raise RuntimeError(
                    f"process {process.args[0]} exited with {process.returncode}; logs: {self.directory}"
                )

    def reply(self, data):
        if self.master is None:
            raise RuntimeError("no PTY for input")
        written = os.write(self.master, data)
        if written != len(data):
            raise RuntimeError("short PTY input write")

    def command(self, kind, frame, suffix=""):
        if self.control is None:
            raise RuntimeError("workload has not connected")
        self.control.sendall(f"{kind} {frame} {suffix}\n".encode())

    def input(self):
        started = time.monotonic_ns()
        if self.window is None:
            self.reply(b"\x1b[97;1:1u")
        else:
            subprocess.run(
                ["xdotool", "key", "--window", self.window, "--delay", "0", "a"],
                check=True,
                capture_output=True,
                timeout=2,
            )
        return started

    def release(self):
        if self.window is None:
            self.reply(b"\x1b[97;1:3u")

    def screenshot(self):
        result = subprocess.run(
            ["import", "-window", self.window, "-depth", "8", "rgb:-"],
            check=False,
            capture_output=True,
            timeout=3,
        )
        if result.returncode:
            raise RuntimeError(
                f"X11 readback failed: {result.stderr.decode(errors='replace')}"
            )
        rgb = result.stdout
        self.last_rgb = rgb
        width, height = self.window_size
        if len(rgb) != width * height * 3:
            raise RuntimeError("Kitty window was resized during readback")
        x, y = marker_at(self.case["pattern"], self.case["width"], self.case["height"])
        at = (y * width + x) * 3
        frame = identify(rgb[at : at + 48])
        if frame is not None and frame not in self.frames:
            cropped = b"".join(
                rgb[y * width * 3 : (y * width + self.case["width"]) * 3]
                for y in range(self.case["height"])
            )
            verify(
                cropped,
                self.case["width"],
                self.case["height"],
                self.case["pattern"],
                frame,
                tolerance=1,
            )
            self.frames[frame] = time.monotonic_ns()

    def pump(self, seconds=0.01):
        self.check()
        for key, _ in self.selector.select(seconds):
            if key.data == "listener":
                self.control, _ = self.listener.accept()
                self.control.settimeout(2)
                self.selector.unregister(self.listener)
                self.selector.register(self.control, selectors.EVENT_READ, "control")
            elif key.data == "control":
                data = self.control.recv(65536)
                if not data:
                    raise RuntimeError("workload control socket closed")
                self.control_buffer.extend(data)
                while b"\n" in self.control_buffer:
                    line, _, remaining = self.control_buffer.partition(b"\n")
                    self.control_buffer = bytearray(remaining)
                    kind, frame, at = line.decode().split()
                    frame, at = int(frame), int(at)
                    if kind == "pid":
                        self.app_pid = frame
                    elif kind == "ready":
                        self.ready = True
                    elif kind == "armed":
                        self.armed.add(frame)
                    elif kind == "commit":
                        self.commits[frame] = at
                    elif kind == "input":
                        self.inputs[frame] = at
                    else:
                        raise RuntimeError(f"unknown workload event: {line!r}")
            elif key.data == "pty":
                if self.throttle and time.monotonic() < self.next_read:
                    continue
                try:
                    data = os.read(self.master, 16384)
                except OSError as error:
                    if error.errno == errno.EAGAIN:
                        continue
                    raise
                if not data:
                    raise RuntimeError("pane PTY closed")
                for frame, at in self.graphics.feed(data, self.reply):
                    self.frames.setdefault(frame, at)
                if self.throttle:
                    self.next_read = time.monotonic() + 0.002
        if self.window_size is not None and self.ready and self.commits:
            self.screenshot()
        if self.throttle and time.monotonic() < self.next_read:
            time.sleep(min(0.001, self.next_read - time.monotonic()))

    def until(self, predicate, label, seconds=5):
        deadline = time.monotonic() + seconds
        while not predicate():
            if time.monotonic() >= deadline:
                if self.last_rgb is not None:
                    (self.directory / "last-readback.ppm").write_bytes(
                        f"P6\n{self.window_size[0]} {self.window_size[1]}\n255\n".encode()
                        + self.last_rgb
                    )
                listing = subprocess.run(
                    [self.binary, "list"],
                    check=False,
                    env=self.env,
                    capture_output=True,
                    timeout=2,
                )
                (self.directory / "windows.txt").write_bytes(
                    listing.stdout + listing.stderr
                )
                raise RuntimeError(
                    f"timed out after {seconds}s: {label}; ready={self.ready} app_pid={self.app_pid} "
                    f"commits={len(self.commits)} frames={len(self.frames)}; logs: {self.directory}"
                )
            self.pump(min(0.02, max(0, deadline - time.monotonic())))

    def settle(self, seconds):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            self.pump(min(0.01, deadline - time.monotonic()))

    def counters(self):
        return {
            "output_bytes": self.graphics.bytes,
            "shared_uploads": self.graphics.shared,
            "inline_uploads": self.graphics.inline,
            "whole_uploads": self.graphics.whole,
            "edit_uploads": self.graphics.edits,
            "uploaded_rgb_bytes": self.graphics.pixel_bytes,
        }

    def close(self):
        owned = set()
        for process in self.processes:
            owned |= descendants(process.pid)
        if self.app_pid:
            owned.add(self.app_pid)
        handles = []
        for pid in owned | {p.pid for p in self.processes}:
            try:
                handles.append(os.pidfd_open(pid))
            except ProcessLookupError:
                pass
        names = set(self.graphics.names)
        for pid in owned | {p.pid for p in self.processes}:
            try:
                for fd in Path(f"/proc/{pid}/fd").iterdir():
                    target = os.readlink(fd).removesuffix(" (deleted)")
                    if target.startswith("/dev/shm/meowland-"):
                        names.add(Path(target))
            except OSError:
                pass
        if self.control is not None:
            try:
                self.control.sendall(b"stop\n")
            except OSError:
                pass
        for handle in handles:
            try:
                signal.pidfd_send_signal(handle, signal.SIGTERM)
            except ProcessLookupError:
                pass
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline and any(
            p.poll() is None for p in self.processes
        ):
            time.sleep(0.01)
        for handle in handles:
            try:
                signal.pidfd_send_signal(handle, signal.SIGKILL)
            except ProcessLookupError:
                pass
            finally:
                os.close(handle)
        for process in self.processes:
            process.wait(timeout=2)
        for name in names:
            name.unlink(missing_ok=True)
        self.selector.close()
        if self.control is not None:
            self.control.close()
        if hasattr(self, "listener"):
            self.listener.close()
        if self.master is not None:
            os.close(self.master)
        for log in self.logs:
            log.close()
        self.runtime.cleanup()


def terminal_requirement():
    missing = [
        tool for tool in ("kitty", "xdotool", "import") if not shutil.which(tool)
    ]
    if missing:
        return "missing tools: " + ", ".join(missing)
    if not os.environ.get("DISPLAY"):
        return "X11 DISPLAY is unset; Wayland-only capture is not implemented"
    return None
