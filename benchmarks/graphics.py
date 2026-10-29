"""Bounded decoder for Meowland's Kitty RGB output, including retained frame edits."""

import base64
import os
import re
import struct
import time
import zlib
from pathlib import Path


def marker(frame):
    return b"MEOW" + struct.pack(
        "<QI", frame, (frame ^ (frame >> 32) ^ 0x91A7485E) & 0xFFFFFFFF
    )


def damage_rect(pattern, width, height):
    """Client damage for bounded patterns, mirroring `fixture::Pattern::damage`."""
    if pattern == "tall":
        return (width // 2, 0, 16, height * 5 // 9)
    if pattern == "wide":
        return (0, height * 5 // 18, width * 5 // 6, height // 18)
    return None


def edit_rect(pattern, width, height):
    """Rectangle an inline edit must cover once damage is widened to whole rows."""
    rect = damage_rect(pattern, width, height)
    if rect is None:
        return None
    y, w, h = rect[1:]
    return (0, y, width, h) if w * 100 >= width * 75 else rect


def marker_at(pattern, width, height):
    """Pixel position of the frame marker, which every bounded pattern keeps in its damage."""
    rect = damage_rect(pattern, width, height)
    return (0, 0) if rect is None else rect[:2]


def identify(rgb):
    if len(rgb) < 48 or any(
        rgb[i] != rgb[i + 1] or rgb[i] != rgb[i + 2] for i in range(0, 48, 3)
    ):
        return None
    data = bytes(rgb[::3][:16])
    frame = int.from_bytes(data[4:12], "little")
    return frame if data == marker(frame) else None


def content(x, y, width, frame):
    value = ((y * width + x) * 0x9E3779B9) ^ (frame * 0x010101)
    return bytes(((value >> 16) & 255, (value >> 8) & 255, value & 255))


def verify(rgb, width, height, pattern, frame, tolerance=0):
    """Check sampled pixels of a reconstructed frame.

    Bounded patterns paint frame 0 outside their damage rectangle, so pixels
    outside it must keep the initial content whether the pane was updated by an
    edit or by a full reread of a correctly declared buffer.
    """
    probes = [
        (20, 0),
        (85, 105),
        (105, 105),
        (125, 105),
        (0, height - 1),
        (width - 1, height - 1),
    ]
    probes += [(width * i // 13, height * i // 13) for i in range(1, 13)]
    rect = damage_rect(pattern, width, height)
    if rect is not None:
        x, y, w, h = rect
        probes += [
            (x, y),
            (x + w - 1, y),
            (x, y + h - 1),
            (x + w - 1, y + h - 1),
            (x + w // 2, y + h // 2),
        ]
        probes += [(x - 1, y)] if x > 0 else []
        probes += [(x + w, y)] if x + w < width else []
        probes += [(x, y - 1)] if y > 0 else []
        probes += [(x, y + h)] if y + h < height else []
    marker_x, marker_y = marker_at(pattern, width, height)
    for x, y in probes:
        if y == marker_y and marker_x <= x < marker_x + 16:
            continue
        where = ""
        if pattern in ("full", "resize"):
            content_width = width // 2 if pattern == "resize" and frame % 2 else width
            content_height = (
                height // 2 if pattern == "resize" and frame % 2 else height
            )
            expected = (
                bytes(3)
                if x >= content_width or y >= content_height
                else content(x, y, content_width, frame)
            )
        elif rect is not None:
            inside = (
                rect[0] <= x < rect[0] + rect[2] and rect[1] <= y < rect[1] + rect[3]
            )
            expected = content(x, y, width, frame if inside else 0)
            if not inside:
                where = f" outside damage {rect}"
        else:
            left = 80 if frame % 2 == 0 else 100
            expected = (
                bytes([173 if 100 <= y < 120 and left <= x < left + 20 else 31]) * 3
            )
        at = (y * width + x) * 3
        actual = rgb[at : at + 3]
        if len(actual) != 3 or any(
            abs(a - b) > tolerance for a, b in zip(actual, expected)
        ):
            raise RuntimeError(
                f"frame {frame}: RGB mismatch at {x},{y}{where}: "
                f"{actual.hex()} != {expected.hex()}"
            )


class Graphics:
    def __init__(self, width, height, transport, pattern):
        self.width, self.height, self.transport, self.pattern = (
            width,
            height,
            transport,
            pattern,
        )
        self.buffer = bytearray()
        self.image = None
        self.transfer = None
        self.last = None
        self.bytes = self.shared = self.inline = 0
        self.whole = self.edits = 0
        self.names = set()
        self.pixel_bytes = 0
        self.probe = False
        self.gpu_sequence = 0

    def shared_read(self, payload, length):
        name = base64.b64decode(payload, validate=True).decode("ascii")
        if not re.fullmatch(r"/meowland-[a-zA-Z0-9-]+", name):
            raise RuntimeError(f"invalid SHM name: {name!r}")
        path = Path("/dev/shm") / name[1:]
        self.names.add(path)
        with path.open("rb") as file:
            if os.fstat(file.fileno()).st_size != length:
                raise RuntimeError("SHM length does not match image dimensions")
            data = file.read(length + 1)
        path.unlink(missing_ok=True)
        return data

    def command(self, body, reply):
        header, payload = body.split(b";", 1)
        fields = dict(
            part.split("=", 1) for part in header.decode("ascii").split(",") if part
        )
        action = fields.get("a")
        if action == "q":
            supported = fields.get("t") != "s" or self.transport == "shared"
            if supported and fields.get("t") == "s":
                self.shared_read(payload, 3)
            reply(
                f"\x1b_Gi={fields.get('i', '0')};{'OK' if supported else 'ENOTSUP'}\x1b\\".encode()
            )
            return
        if (
            fields.get("i") == "30"
            and fields.get("s") == "1"
            and fields.get("v") == "1"
        ):
            if base64.b64decode(payload, validate=True) != b"\0\0\0":
                raise RuntimeError("unexpected frame-edit probe content")
            if action == "t":
                self.probe = True
            elif action == "f" and self.probe and fields.get("r") == "1":
                reply(b"\x1b_Gi=30;OK\x1b\\")
            else:
                raise RuntimeError("unsupported frame-edit probe")
            return
        if action == "d":
            if fields.get("d") == "I" and fields.get("i") == "30":
                self.probe = False
            elif fields.get("d") == "A" or fields.get("i") == "1":
                self.image = None
            else:
                raise RuntimeError("unsupported image deletion")
            return
        if self.transfer is None:
            if action not in ("T", "t", "f") or fields.get("f") != "24":
                raise RuntimeError(f"unsupported Kitty command: {fields}")
            width, height = int(fields["s"]), int(fields["v"])
            if not (0 < width <= self.width and 0 < height <= self.height):
                raise RuntimeError(f"invalid image dimensions {width}x{height}")
            if action == "f" and (fields.get("r") != "1" or fields.get("X") != "1"):
                raise RuntimeError("unsupported frame edit")
            if fields.get("i") != "1":
                raise RuntimeError("only the root image is supported")
            self.transfer = (fields, bytearray())
        elif action is not None and not (
            action == "f" and self.transfer[0].get("a") == "f"
        ):
            raise RuntimeError("interleaved Kitty transfers")
        original, data = self.transfer
        length = int(original["s"]) * int(original["v"]) * 3
        data.extend(
            self.shared_read(payload, length)
            if fields.get("t") == "s"
            else base64.b64decode(payload, validate=True)
        )
        if len(data) > length * 2 + 65536:
            raise RuntimeError("oversized Kitty transfer")
        if fields.get("m", "0") != "0":
            return
        if original.get("o") == "z":
            decoder = zlib.decompressobj()
            data = decoder.decompress(data, length + 1)
            if not decoder.eof or decoder.unused_data:
                raise RuntimeError("invalid compressed image")
        if len(data) != length:
            raise RuntimeError("incomplete Kitty image")
        width, height = int(original["s"]), int(original["v"])
        if original.get("a") == "f":
            x, y = int(original.get("x", 0)), int(original.get("y", 0))
            expected = edit_rect(self.pattern, self.width, self.height)
            if expected is not None and (x, y, width, height) != expected:
                raise RuntimeError(
                    f"frame edit {x},{y} {width}x{height} != expected {expected}"
                )
            if self.image is None or x + width > self.width or y + height > self.height:
                raise RuntimeError("frame edit exceeds its base image")
            for row in range(height):
                at = ((y + row) * self.width + x) * 3
                self.image[at : at + width * 3] = data[
                    row * width * 3 : (row + 1) * width * 3
                ]
        else:
            if (width, height) != (self.width, self.height):
                raise RuntimeError(
                    f"expected whole image {self.width}x{self.height}, got {width}x{height}"
                )
            self.image = bytearray(data)
        self.shared += original.get("t") == "s"
        self.inline += original.get("t") != "s"
        self.whole += original.get("a") != "f"
        self.edits += original.get("a") == "f"
        self.pixel_bytes += length
        self.transfer = None

    def feed(self, data, reply):
        self.bytes += len(data)
        self.buffer.extend(data)
        seen = []
        while self.buffer:
            if self.buffer[0] != 27:
                end = self.buffer.find(27)
                del self.buffer[: end if end >= 0 else len(self.buffer)]
                continue
            if len(self.buffer) < 2:
                break
            kind = self.buffer[1]
            if kind in (ord("_"), ord("]")):
                end = self.buffer.find(b"\x1b\\", 2)
                if end < 0:
                    break
                body = bytes(self.buffer[2:end])
                del self.buffer[: end + 2]
                if kind == ord("_") and body.startswith(b"G"):
                    self.command(body[1:], reply)
                elif body.startswith(b"52;") and body.endswith(b";?"):
                    reply(b"\x1b]52;c;\x1b\\")
            elif kind == ord("["):
                match = re.match(rb"\x1b\[([0-?]*)([ -/]*)([@-~])", self.buffer)
                if not match:
                    break
                params, _, final = match.groups()
                del self.buffer[: match.end()]
                response = {
                    (b"16", b"t"): b"\x1b[6;20;10t",
                    (b"14", b"t"): f"\x1b[4;{self.height};{self.width}t".encode(),
                    (b"", b"c"): b"\x1b[?1;2c",
                    (b">", b"c"): b"\x1b[>1;1;0c",
                    (b"?", b"u"): b"\x1b[?31u",
                    (b"?1016", b"p"): b"\x1b[?1016;2$y",
                    (b"?5522", b"p"): b"\x1b[?5522;2$y",
                }.get((params, final))
                if response:
                    reply(response)
                if params == b"?2026" and final == b"l" and self.image is not None:
                    if self.pattern == "gpu":
                        stride = max(1, self.width * self.height // 1024) * 3
                        signature = b"".join(
                            self.image[at : at + 3]
                            for at in range(0, len(self.image), stride)
                        )
                        if not any(
                            signature[at : at + 3] != signature[:3]
                            for at in range(3, len(signature), 3)
                        ):
                            continue
                        if signature != self.last:
                            self.last = signature
                            self.gpu_sequence += 1
                            seen.append((self.gpu_sequence, time.monotonic_ns()))
                        continue
                    x, y = marker_at(self.pattern, self.width, self.height)
                    at = (y * self.width + x) * 3
                    frame = identify(self.image[at : at + 48])
                    if frame is None:
                        raise RuntimeError("corrupt frame marker")
                    if frame != self.last:
                        verify(self.image, self.width, self.height, self.pattern, frame)
                        self.last = frame
                        seen.append((frame, time.monotonic_ns()))
            else:
                del self.buffer[:2]
        if len(self.buffer) > 65536:
            raise RuntimeError("unterminated terminal escape sequence")
        return seen
