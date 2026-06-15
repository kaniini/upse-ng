#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-2.1-or-later
"""Generate the public synthetic PSF1 fixtures used by C interface tests."""

import argparse
import struct
import zlib
from pathlib import Path


def instruction_lui(register: int, immediate: int) -> int:
    return (0x0F << 26) | (register << 16) | (immediate & 0xFFFF)


def instruction_addiu(register: int, immediate: int) -> int:
    return (0x09 << 26) | (register << 16) | (immediate & 0xFFFF)


def instruction_sh(register: int, offset: int) -> int:
    return (0x29 << 26) | (8 << 21) | (register << 16) | offset


def synthetic_executable() -> bytes:
    words = [instruction_lui(8, 0x1F80)]
    registers = [
        (0x3FFF, 0x1C00),
        (0x3FFF, 0x1C02),
        (0x1000, 0x1C04),
        (0x00FF, 0x1C08),
        (0x1F00, 0x1C0A),
        (0x3FFF, 0x1D80),
        (0x3FFF, 0x1D82),
        (1, 0x1D94),
        (-32768, 0x1DAA),
        (1, 0x1D88),
    ]
    for value, offset in registers:
        words.append(instruction_addiu(9, value))
        words.append(instruction_sh(9, offset))
    loop_address = 0x80010000 + len(words) * 4
    words.extend([0x08000000 | ((loop_address >> 2) & 0x03FFFFFF), 0])
    text = b"".join(struct.pack("<I", word) for word in words)
    executable = bytearray(0x800 + len(text))
    executable[0:8] = b"PS-X EXE"
    executable[0x10:0x14] = struct.pack("<I", 0x80010000)
    executable[0x18:0x1C] = struct.pack("<I", 0x80010000)
    executable[0x1C:0x20] = struct.pack("<I", len(text))
    executable[0x30:0x34] = struct.pack("<I", 0x801FFF00)
    executable[0x4C:0x51] = b"Japan"
    executable[0x800:] = text
    return bytes(executable)


def overlay_executable() -> bytes:
    executable = bytearray(0x800)
    executable[0:8] = b"PS-X EXE"
    executable[0x10:0x14] = struct.pack("<I", 0x80010000)
    executable[0x18:0x1C] = struct.pack("<I", 0x80010000)
    executable[0x30:0x34] = struct.pack("<I", 0x801FFF00)
    executable[0x4C:0x51] = b"Japan"
    return bytes(executable)


def psf(program: bytes, tags: list[tuple[str, str]]) -> bytes:
    compressed = zlib.compress(program, level=9)
    header = b"PSF\x01" + struct.pack(
        "<III", 0, len(compressed), zlib.crc32(compressed)
    )
    tag_data = "".join(f"{key}={value}\n" for key, value in tags).encode()
    return header + compressed + b"[TAG]" + tag_data


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)

    executable = synthetic_executable()
    common_tags = [
        ("title", "UPSE-NG synthetic noise"),
        ("game", "Generated fixture"),
        ("artist", "UPSE-NG tests"),
        ("length", "0.020"),
        ("fade", "0.005"),
    ]
    (args.output / "synthetic.psf").write_bytes(psf(executable, common_tags))
    (args.output / "library.psflib").write_bytes(psf(executable, []))
    (args.output / "synthetic.minipsf").write_bytes(
        psf(overlay_executable(), [("_lib", "library.psflib"), *common_tags])
    )


if __name__ == "__main__":
    main()
