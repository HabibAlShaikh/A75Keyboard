#!/usr/bin/env python3
"""Wrap a raw YC3121 firmware image in a minimal ELF32 container for REA.

Usage:
    python3 make_elf.py firmwareFile.bin firmware.elf [base]

The image is mapped at `base` (default 0x01000000) and the section is marked
ARM/Thumb + executable so Hopper/Ghidra can analyse it.  The payload bytes are
copied verbatim; only file offsets are shifted to virtual addresses.
"""
from __future__ import annotations

import struct
import sys


def wrap(src: str, dst: str, base: int = 0x01000000) -> None:
    data = open(src, "rb").read()
    ehsz, phsz = 52, 32
    shstr = b"\x00.text\x00.shstrtab\x00"
    off_data = 0x1000
    shstr_off = (off_data + len(data) + 3) & ~3
    shoff = (shstr_off + len(shstr) + 3) & ~3

    ehdr = bytearray(52)
    ehdr[0:4] = b"\x7fELF"
    ehdr[4] = 1          # ELFCLASS32
    ehdr[5] = 1          # ELFDATA2LSB
    ehdr[6] = 1          # EV_CURRENT
    struct.pack_into(
        "<HHIIIIIHHHHHH", ehdr, 16,
        2,               # ET_EXEC
        40,              # EM_ARM
        1,
        base + 1,        # entry (Thumb bit)
        ehsz,
        shoff,
        0x05000200,      # EABI5
        ehsz, phsz, 1, 40, 3, 2,
    )
    phdr = struct.pack(
        "<IIIIIIII",
        1,               # PT_LOAD
        off_data,
        base,
        base,
        len(data),
        len(data),
        5,               # PF_R | PF_X
        0x1000,
    )
    sh_text = struct.pack(
        "<IIIIIIIIII",
        1, 1, 6, base, off_data, len(data),
        0, 0, 4, 0,
    )
    sh_str = struct.pack(
        "<IIIIIIIIII",
        0, 3, 0, 0, shstr_off, len(shstr),
        0, 0, 1, 0,
    )
    out = bytearray(ehdr + phdr + shstr)
    out += bytes(off_data - len(out))
    out += data
    out += bytes(shoff - len(out))
    out += b"\x00" * 40 + sh_text + sh_str
    open(dst, "wb").write(out)
    print(f"{dst}: {len(out)} bytes, base 0x{base:08x}")


if __name__ == "__main__":
    if len(sys.argv) < 3:
        raise SystemExit(__doc__)
    base = int(sys.argv[3], 0) if len(sys.argv) > 3 else 0x01000000
    wrap(sys.argv[1], sys.argv[2], base)
