#!/usr/bin/env python3
# SPDX-License-Identifier: LGPL-2.1-or-later
"""Generate the public synthetic PSF1 and PSF2 C-interface fixtures."""

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


def instruction_sh_base(register: int, offset: int, base: int) -> int:
    return (0x29 << 26) | (base << 21) | (register << 16) | (offset & 0xFFFF)


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


def synthetic_irx() -> bytes:
    """Build a stripped relocatable IRX which drives SPU2 MMIO directly."""
    words = [instruction_lui(8, 0x1F90)]

    def write(value: int, offset: int) -> None:
        words.append(instruction_addiu(9, value))
        words.append(instruction_sh_base(9, offset, 8))

    # Core 1 transfer address and one generated looping ADPCM block.
    write(0, 0x05A8)
    write(0x0800, 0x05AA)
    for halfword in [0x0700, *([0x1111] * 7)]:
        write(halfword, 0x05AC)

    # Core 1, voice 0, final mixer, and key-on.
    for value, offset in [
        (0x3FFF, 0x0400),
        (0x3FFF, 0x0402),
        (0x1000, 0x0404),
        (0x00FF, 0x0406),
        (0x1F00, 0x0408),
        (0, 0x05C0),
        (0x0800, 0x05C2),
        (1, 0x0588),
        (1, 0x0590),
        (0x0030, 0x0598),
        (-16384, 0x059A),
        (0x3FFF, 0x0788),
        (0x3FFF, 0x078A),
        (1, 0x05A0),
    ]:
        write(value, offset)
    words.extend([0x1000FFFF, 0])
    image = b"".join(struct.pack("<I", word) for word in words)

    phoff = 52
    metadata_offset = 0x80
    image_offset = 0x100
    metadata = struct.pack(
        "<IIIIIIH", 0xFFFFFFFF, 0, 0, len(image), 0, 0, 0x0100
    ) + b"fixture\0"
    elf = bytearray(image_offset + len(image))
    elf[0:16] = b"\x7fELF\x01\x01\x01" + bytes(9)
    struct.pack_into("<HHI", elf, 16, 0xFF81, 8, 1)
    struct.pack_into("<III", elf, 24, 0, phoff, 0)
    struct.pack_into("<HHH", elf, 40, 52, 32, 2)

    def program_header(index: int, kind: int, offset: int, size: int,
                       alignment: int) -> None:
        struct.pack_into(
            "<IIIIIIII", elf, phoff + index * 32, kind, offset, 0, 0,
            size, size, 7, alignment
        )

    program_header(0, 0x70000080, metadata_offset, len(metadata), 4)
    program_header(1, 1, image_offset, len(image), 16)
    elf[metadata_offset:metadata_offset + len(metadata)] = metadata
    elf[image_offset:] = image
    return bytes(elf)


def psf2_filesystem(files: list[tuple[str, bytes]]) -> bytes:
    output = bytearray(4 + len(files) * 48)
    struct.pack_into("<I", output, 0, len(files))
    for index, (name, data) in enumerate(files):
        entry = 4 + index * 48
        encoded_name = name.encode()
        output[entry:entry + len(encoded_name)] = encoded_name
        block = zlib.compress(data, level=9)
        data_offset = len(output)
        output.extend(struct.pack("<I", len(block)))
        output.extend(block)
        struct.pack_into("<III", output, entry + 36, data_offset,
                         len(data), len(data))
    return bytes(output)


def psf2(reserved: bytes, tags: list[tuple[str, str]]) -> bytes:
    compressed = zlib.compress(b"", level=9)
    header = b"PSF\x02" + struct.pack(
        "<III", len(reserved), len(compressed), zlib.crc32(compressed)
    )
    tag_data = "".join(f"{key}={value}\n" for key, value in tags).encode()
    return header + reserved + compressed + b"[TAG]" + tag_data


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
    psf2_tags = [
        ("title", "UPSE-NG synthetic PSF2"),
        ("game", "Generated fixture"),
        ("artist", "UPSE-NG tests"),
        ("length", "0.020"),
        ("fade", "0.005"),
    ]
    reserved = psf2_filesystem([("psf2.irx", synthetic_irx())])
    (args.output / "synthetic.psf2").write_bytes(psf2(reserved, psf2_tags))


if __name__ == "__main__":
    main()
