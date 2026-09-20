#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Symbolicate a recorded Android mapping using the EXACT run's APK.

Only Python's standard library and GNU binutils are needed. File offsets are
converted through ELF PT_LOAD segments, not treated as ELF virtual addresses.
Unknown symbols remain unknown; a stripped APK is not a successful diagnosis.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import struct
import subprocess
import tempfile
import zipfile


def load_segments(data):
    """Read executable, file-backed PT_LOAD ranges from little-endian ELF64."""
    if len(data) < 64 or data[:6] != b"\x7fELF\x02\x01":
        raise ValueError("Expected a little-endian ELF64 library")
    phoff = struct.unpack_from("<Q", data, 32)[0]
    phentsize, phnum = struct.unpack_from("<HH", data, 54)
    if phentsize < 56 or not phnum or phnum == 0xffff:
        raise ValueError("Invalid or unsupported ELF program header table")
    if phoff + phentsize * phnum > len(data):
        raise ValueError("Truncated ELF program header table")
    segments = []
    for i in range(phnum):
        kind, flags, offset, vaddr, _, filesz, memsz, _ = struct.unpack_from(
            "<IIQQQQQQ", data, phoff + phentsize * i
        )
        if kind == 1:
            if filesz > memsz or offset + filesz > len(data):
                raise ValueError("Invalid ELF load segment")
            if flags & 1:
                segments.append((offset, vaddr, filesz))
    return segments


def elf_address(pc, mapping, segments):
    start, end, offset = (int(mapping[key], 0) for key in ("start", "end", "offset"))
    if not start <= pc < end:
        raise ValueError(f"PC {pc:#x} is outside the recorded mapping")
    file_offset = pc - start + offset
    matches = [
        va + file_offset - off
        for off, va, size in segments
        if off <= file_offset < off + size
    ]
    if len(matches) != 1:
        raise ValueError(f"File offset {file_offset:#x} has no unique executable PT_LOAD")
    return file_offset, matches[0]


def tool(*args):
    return subprocess.check_output(args, text=True, env={**os.environ, "LC_ALL": "C"}).strip()


def annotate(message, level="notice"):
    # Workflow commands must not interpret symbol text as additional commands.
    escaped = message.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
    print(f"::{level} title=RR3 native crash::{escaped}")


def report(apk, trace, run, prefix, annotations=False):
    if str(run["id"]) != trace["run_id"] or run["head_sha"] != trace["head_sha"]:
        raise ValueError("Source run does not match the crash fixture; do not use a rebuilt APK")
    library = "lib/arm64-v8a/libtouchHLE.so"
    with zipfile.ZipFile(apk) as archive:
        data = archive.read(library)
    if len(data) < 64 or struct.unpack_from("<H", data, 18)[0] != 183:
        raise ValueError("The APK library is not AArch64")
    segments = load_segments(data)
    lines = [
        "# RR3 native crash: exact saved Android artifact",
        "",
        f"Source run: {trace['run_id']}; commit: {trace['head_sha']}",
        f"APK SHA256: {hashlib.sha256(Path(apk).read_bytes()).hexdigest()}",
        f"ELF SHA256: {hashlib.sha256(data).hexdigest()}",
        "",
        "Frames #0–#4 include signal reporting/trampolines. #5 is the first",
        "libtouchHLE frame after the signal trampoline, not a proven root cause.",
        "Addresses below are exact recorded PCs (no return-address subtraction).",
        "Only frames in the recorded libtouchHLE mapping are included.",
        "",
    ]
    with tempfile.TemporaryDirectory() as directory:
        elf = Path(directory) / "libtouchHLE.so"
        elf.write_bytes(data)
        notes = tool(prefix + "readelf", "-nW", str(elf))
        lines += ["## ELF notes / build ID", "```text", notes, "```", ""]
        sections = tool(prefix + "readelf", "-SW", str(elf))
        if ".symtab" not in sections:
            warning = "APK has no .symtab: internal functions may be unknown; matching unstripped symbols are needed."
            lines += ["**" + warning + "**", ""]
            if annotations:
                annotate(warning, "warning")
        addresses = {}
        for frame in trace["frames"]:
            pc = int(frame["pc"], 0)
            offset, va = elf_address(pc, trace["mapping"], segments)
            addresses[frame["index"]] = va
            symbols = tool(prefix + "addr2line", "-f", "-C", "-i", "-e", str(elf), hex(va))
            heading = f"#{frame['index']}: PC={pc:#x}, file={offset:#x}, ELF={va:#x}"
            lines += ["## " + heading, "```text", symbols, "```", ""]
            if annotations:
                annotate(heading + "\n" + symbols)
        # Useful even if Gradle stripped the private symbol table. This shows
        # instructions, not a guessed function name or guessed root cause.
        for index in (5, 6):
            if index not in addresses:
                continue
            va = addresses[index]
            disassembly = tool(
                prefix + "objdump", "-d", "-C",
                f"--start-address={max(0, va - 64):#x}",
                f"--stop-address={va + 68:#x}", str(elf),
            )
            lines += [f"## Instructions near frame #{index}", "```text", disassembly, "```", ""]
            if annotations:
                annotate(f"Instructions near frame #{index}\n" + disassembly)
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apk", type=Path, required=True)
    parser.add_argument("--trace", type=Path, required=True)
    parser.add_argument("--source-run", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--tool-prefix", default="aarch64-linux-gnu-")
    parser.add_argument("--github-annotations", action="store_true")
    args = parser.parse_args()
    text = report(args.apk, json.loads(args.trace.read_text()),
                  json.loads(args.source_run.read_text()), args.tool_prefix,
                  args.github_annotations)
    args.output.write_text(text)
    if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(summary, "a") as stream:
            stream.write(text + "\n")
    print(text)


if __name__ == "__main__":
    main()
