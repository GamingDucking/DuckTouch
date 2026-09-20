#!/usr/bin/env python3
# SPDX-License-Identifier: MPL-2.0
"""Run with python3 dev-scripts/test-symbolicate-android-crash.py."""

import contextlib
import importlib.util
import io
import json
from pathlib import Path
import shutil
import struct
import subprocess
import tempfile
import unittest
from unittest.mock import patch
import zipfile

spec = importlib.util.spec_from_file_location(
    "symbolicate", Path(__file__).with_name("symbolicate-android-crash.py")
)
symbolicate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(symbolicate)


def elf_fixture():
    data = bytearray(1024)
    data[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<H", data, 18, 183)  # AArch64
    struct.pack_into("<Q", data, 32, 64)
    struct.pack_into("<HH", data, 54, 56, 1)
    # PT_LOAD with p_vaddr != p_offset; BSS is deliberately larger.
    struct.pack_into("<IIQQQQQQ", data, 64, 1, 5, 256, 0x10200, 0, 128, 256, 256)
    return data


class SymbolicationTests(unittest.TestCase):
    mapping = {"start": "0x800000", "end": "0x800100", "offset": "0x100"}

    def test_pt_load_conversion_not_file_offset(self):
        segments = symbolicate.load_segments(elf_fixture())
        self.assertEqual(segments, [(256, 0x10200, 128)])
        self.assertEqual(symbolicate.elf_address(0x800010, self.mapping, segments),
                         (0x110, 0x10210))

    def test_mapping_end_is_exclusive(self):
        with self.assertRaisesRegex(ValueError, "outside"):
            symbolicate.elf_address(0x800100, self.mapping, [(256, 0x10200, 256)])

    def test_bss_is_not_file_backed(self):
        with self.assertRaisesRegex(ValueError, "no unique"):
            symbolicate.elf_address(0x800080, self.mapping,
                                   symbolicate.load_segments(elf_fixture()))

    def test_ambiguous_segment_rejected(self):
        with self.assertRaisesRegex(ValueError, "no unique"):
            symbolicate.elf_address(0x800010, self.mapping, [(256, 0, 128)] * 2)

    def test_non_executable_segment_excluded(self):
        data = elf_fixture()
        struct.pack_into("<I", data, 68, 4)
        self.assertEqual(symbolicate.load_segments(data), [])

    def test_truncated_headers_rejected(self):
        for size in (0, 63, 119):
            with self.subTest(size=size), self.assertRaises(ValueError):
                symbolicate.load_segments(elf_fixture()[:size])

    def test_out_of_file_segment_rejected(self):
        with self.assertRaisesRegex(ValueError, "Invalid ELF load"):
            symbolicate.load_segments(elf_fixture()[:300])

    def test_wrong_elf_class_or_endianness_rejected(self):
        for index in (4, 5):
            data = elf_fixture()
            data[index] = 3
            with self.subTest(index=index), self.assertRaises(ValueError):
                symbolicate.load_segments(data)

    def test_wrong_run_rejected_before_reading_apk(self):
        trace = {"run_id": "123", "head_sha": "abc"}
        for run in ({"id": 124, "head_sha": "abc"}, {"id": 123, "head_sha": "def"}):
            with self.subTest(run=run), self.assertRaisesRegex(ValueError, "does not match"):
                symbolicate.report("does-not-exist.apk", trace, run, "")

    def test_annotation_escaping(self):
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            symbolicate.annotate("a%\r\n::error::not a command")
        self.assertEqual(output.getvalue().count("\n"), 1)
        self.assertIn("a%25%0D%0A::error::not a command", output.getvalue())

    def test_stripped_apk_is_explicit_not_guessed(self):
        trace = {"run_id": "123", "head_sha": "abc", "mapping": self.mapping,
                 "frames": [{"index": 5, "pc": "0x800010"}]}
        run = {"id": 123, "head_sha": "abc"}

        def fake_tool(*args):
            if args[0] == "addr2line":
                self.assertEqual(args[-1], "0x10210")
                return "??\n??:0"
            return "no symbols"

        with tempfile.TemporaryDirectory() as directory:
            apk = Path(directory) / "test.apk"
            with zipfile.ZipFile(apk, "w") as archive:
                archive.writestr("lib/arm64-v8a/libtouchHLE.so", elf_fixture())
            with patch.object(symbolicate, "tool", fake_tool):
                report = symbolicate.report(apk, trace, run, "")
            self.assertIn("APK has no .symtab", report)
            self.assertIn("??\n??:0", report)
            self.assertIn("file=0x110, ELF=0x10210", report)

    def test_recorded_rr3_offsets(self):
        fixture = Path(__file__).resolve().parent.parent / "dev-docs/crashes/real-racing-3-30bdf19.json"
        trace = json.loads(fixture.read_text())
        # Synthetic executable segment tests mapping arithmetic only. The real
        # APK's p_vaddr is intentionally NOT assumed equal to its file offset.
        segments = [(0x689000, 0x689000, 0xccd000)]
        frames = {frame["index"]: int(frame["pc"], 0) for frame in trace["frames"]}
        for index, expected in ((5, 0xd65168), (6, 0xd57354), (7, 0x89daf8)):
            self.assertEqual(symbolicate.elf_address(frames[index], trace["mapping"], segments)[0],
                             expected)

    @unittest.skipUnless(all(shutil.which(t) for t in ("gcc", "nm", "addr2line")),
                         "host C toolchain unavailable")
    def test_real_elf_symbol_with_nonzero_load_bias(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "test"
            subprocess.run(["gcc", "-x", "c", "-g", "-no-pie", "-o", str(binary), "-"],
                           input="int rr3_test_marker(void) { return 42; }\nint main(void) { return rr3_test_marker(); }",
                           text=True, check=True)
            segments = symbolicate.load_segments(binary.read_bytes())
            nm = symbolicate.tool("nm", str(binary))
            va = int(next(line.split()[0] for line in nm.splitlines()
                          if line.endswith(" rr3_test_marker")), 16)
            off, base, size = next(seg for seg in segments if seg[1] <= va < seg[1] + seg[2])
            self.assertNotEqual(off, base)
            mapping = {"start": hex(base + 0x10000000), "end": hex(base + size + 0x10000000),
                       "offset": hex(off)}
            _, resolved = symbolicate.elf_address(va + 0x10000000, mapping, segments)
            self.assertEqual(resolved, va)
            symbols = symbolicate.tool("addr2line", "-f", "-e", str(binary), hex(resolved))
            self.assertIn("rr3_test_marker", symbols)


if __name__ == "__main__":
    unittest.main()
