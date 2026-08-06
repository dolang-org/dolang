#!/usr/bin/env python3
"""Generate platform system error-code name tables.

The source documents are downloaded from pinned URLs by default. Local copies
can be supplied for offline regeneration and are verified against the same
pinned hashes.

Current source revisions:
- Linux UAPI asm-generic/errno-base.h and asm-generic/errno.h at Linux v6.18
- FreeBSD sys/errno.h at a3a884c0d43ab02187022be9ae9084e6c725ba68
- apple-oss-distributions/xnu bsd/sys/errno.h at f6217f891ac0bb64f3d375211650a4c1ff8ca1ea
- MS-ERREF revision 23.0 DOCX, published 2024-11-19
"""

import argparse
import hashlib
import re
import sys
import tempfile
import urllib.request
import xml.etree.ElementTree as ET
import zipfile
from pathlib import Path


DEFINE = re.compile(r"^\s*#\s*define\s+(E[A-Z0-9_]+)\s+([^\s/]+)", re.MULTILINE)
NUMBER = re.compile(r"^\(?([0-9]+)\)?$")
WIN32 = re.compile(r"^(0x[0-9A-Fa-f]{8})([A-Z][A-Z0-9_]+)$")
WORD_NS = "{http://schemas.openxmlformats.org/wordprocessingml/2006/main}"
LINUX_REVISION = "7d0a66e4bb9081d75c82ec4957c50034cb0ea449"
FREEBSD_REVISION = "a3a884c0d43ab02187022be9ae9084e6c725ba68"
MACOS_REVISION = "f6217f891ac0bb64f3d375211650a4c1ff8ca1ea"
PINNED_SOURCES = {
    "linux-errno-base.h": (
        "https://raw.githubusercontent.com/torvalds/linux/"
        f"{LINUX_REVISION}/include/uapi/asm-generic/errno-base.h",
        "2c148e92b8318deeb767fabd60822113e575ee664ff09a1873aed8f7a495793c",
    ),
    "linux-errno.h": (
        "https://raw.githubusercontent.com/torvalds/linux/"
        f"{LINUX_REVISION}/include/uapi/asm-generic/errno.h",
        "fb7b5a504015f3a9074c641e7371b250d867d751d90e4a22a8ac17fced3d50af",
    ),
    "freebsd-errno.h": (
        "https://raw.githubusercontent.com/freebsd/freebsd-src/"
        f"{FREEBSD_REVISION}/sys/sys/errno.h",
        "4e615f248a900c6c240c0c87844fd60a8bdffc8a34259d876d5dfde74bd9e42c",
    ),
    "macos-errno.h": (
        "https://raw.githubusercontent.com/apple-oss-distributions/xnu/"
        f"{MACOS_REVISION}/bsd/sys/errno.h",
        "b16e5eb195f153793b14c3826e6b04db945e9ed6a37c5a9046196555bded6906",
    ),
    "ms-erref-23.docx": (
        "https://winprotocoldocs-bhdugrdyduf5h2e4.b02.azurefd.net/"
        "MS-ERREF/%5BMS-ERREF%5D-241119.docx",
        "3851d065ff4a4a6ee8e69c7ddaaa2883947ad46f4f7ecc9b8de30ff5660e4abe",
    ),
}


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def verify_source(name: str, path: Path) -> None:
    expected = PINNED_SOURCES[name][1]
    actual = digest(path)
    if actual != expected:
        raise ValueError(f"{name}: expected sha256:{expected}, got sha256:{actual}")


def download_sources(directory: Path) -> dict[str, Path]:
    result = {}
    for name, (url, _) in PINNED_SOURCES.items():
        path = directory / name
        with urllib.request.urlopen(url, timeout=60) as response:
            path.write_bytes(response.read())
        verify_source(name, path)
        result[name] = path
    return result


def parse_errno(
    paths: list[Path],
) -> tuple[list[tuple[int, str]], list[tuple[str, int]]]:
    definitions: dict[str, str] = {}
    order: list[str] = []
    for path in paths:
        for name, expression in DEFINE.findall(path.read_text()):
            if name not in definitions:
                order.append(name)
            definitions[name] = expression

    resolved: dict[str, int] = {}

    def resolve(name: str, stack: set[str]) -> int | None:
        if name in resolved:
            return resolved[name]
        if name in stack:
            raise ValueError(f"cyclic errno alias involving {name}")
        expression = definitions.get(name)
        if expression is None:
            return None
        match = NUMBER.match(expression)
        if match:
            value = int(match.group(1))
        elif expression in definitions:
            value = resolve(expression, stack | {name})
            if value is None:
                return None
        else:
            return None
        resolved[name] = value
        return value

    # Numeric definitions are canonical; alias-only names do not replace them.
    result: dict[int, str] = {}
    for name in order:
        if not NUMBER.match(definitions[name]):
            continue
        value = resolve(name, set())
        if value is not None:
            result.setdefault(value, name)
    canonical = sorted(result.items())
    aliases = sorted(
        (name, value)
        for name in order
        if (value := resolve(name, set())) is not None
        and result.get(value) != name
    )
    return canonical, aliases


def parse_windows(
    path: Path,
) -> tuple[list[tuple[int, str]], list[tuple[str, int]]]:
    with zipfile.ZipFile(path) as archive:
        root = ET.fromstring(archive.read("word/document.xml"))

    body = root.find(WORD_NS + "body")
    if body is None:
        raise ValueError("MS-ERREF DOCX has no document body")

    active = False
    result: dict[int, str] = {}
    names: dict[str, int] = {}
    for element in body:
        text = "".join(node.text or "" for node in element.iter(WORD_NS + "t"))
        if element.tag == WORD_NS + "p" and text == "Win32 Error Codes":
            active = True
            continue
        if element.tag == WORD_NS + "p" and text == "NTSTATUS":
            active = False
        if not active or element.tag != WORD_NS + "tbl":
            continue
        for row in element.iter(WORD_NS + "tr"):
            cells = [
                "".join(node.text or "" for node in cell.iter(WORD_NS + "t"))
                for cell in row.findall(WORD_NS + "tc")
            ]
            if not cells:
                continue
            match = WIN32.match(cells[0])
            if match:
                value = int(match.group(1), 16)
                name = match.group(2)
                result.setdefault(value, name)
                names.setdefault(name, value)

    if not result:
        raise ValueError("no Win32 codes found in MS-ERREF DOCX")
    canonical = sorted(result.items())
    aliases = sorted(
        (name, value) for name, value in names.items() if result[value] != name
    )
    return canonical, aliases


def rust_table(
    name: str,
    values: list[tuple[int, str]],
    aliases: list[tuple[str, int]],
    value_type: str,
) -> str:
    rows = "\n".join(
        f'        {value}{value_type} => "{symbol}",' for value, symbol in values
    )
    alias_rows = "\n".join(
        f'        "{symbol}" => {value}{value_type},' for symbol, value in aliases
    )
    return (
        "#[rustfmt::skip]\n"
        f"error_codes!({name}_BY_CODE, {name}_BY_NAME, {value_type}, "
        f"{{\n{rows}\n    }}, {{\n"
        f"{alias_rows}\n    }});\n"
    )


def provenance_header(sources: dict[str, Path]) -> str:
    for name, path in sources.items():
        verify_source(name, path)
    return "\n".join(
        f"// {name}: {PINNED_SOURCES[name][0]} sha256:{digest(path)}"
        for name, path in sources.items()
    )


def generate_errno(args: argparse.Namespace) -> str:
    linux, linux_aliases = parse_errno(args.linux)
    freebsd, freebsd_aliases = parse_errno(args.freebsd)
    macos, macos_aliases = parse_errno(args.macos)
    provenance = provenance_header(
        {
            "linux-errno-base.h": args.linux[0],
            "linux-errno.h": args.linux[1],
            "freebsd-errno.h": args.freebsd[0],
            "macos-errno.h": args.macos[0],
        }
    )
    return (
        "// @generated by tools/gen_system_error_codes.py; do not edit.\n"
        f"{provenance}\n\n"
        + rust_table("LINUX_ERRNO", linux, linux_aliases, "i32")
        + "\n"
        + rust_table("FREEBSD_ERRNO", freebsd, freebsd_aliases, "i32")
        + "\n"
        + rust_table("MACOS_ERRNO", macos, macos_aliases, "i32")
    )


def generate_windows(args: argparse.Namespace) -> str:
    windows, windows_aliases = parse_windows(args.windows)
    provenance = provenance_header({"ms-erref-23.docx": args.windows})
    return (
        "// @generated by tools/gen_system_error_codes.py; do not edit.\n"
        "// MS-ERREF revision 23.0 (2024-11-19).\n"
        f"{provenance}\n\n"
        + rust_table("WIN_ERROR", windows, windows_aliases, "u32")
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--linux", type=Path, action="append", default=[])
    parser.add_argument("--freebsd", type=Path, action="append", default=[])
    parser.add_argument("--macos", type=Path, action="append", default=[])
    parser.add_argument("--windows", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--windows-output", type=Path, required=True)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()

    supplied = bool(args.linux or args.freebsd or args.macos or args.windows)
    complete = (
        len(args.linux) == 2
        and len(args.freebsd) == 1
        and len(args.macos) == 1
        and args.windows is not None
    )
    if supplied and not complete:
        parser.error("provide all pinned inputs or omit them to download automatically")

    with tempfile.TemporaryDirectory(prefix="dolang-system-errors-") as temp:
        if not supplied:
            sources = download_sources(Path(temp))
            args.linux = [sources["linux-errno-base.h"], sources["linux-errno.h"]]
            args.freebsd = [sources["freebsd-errno.h"]]
            args.macos = [sources["macos-errno.h"]]
            args.windows = sources["ms-erref-23.docx"]

        errno_generated = generate_errno(args)
        windows_generated = generate_windows(args)
        outputs = [(args.output, errno_generated), (args.windows_output, windows_generated)]
        if args.check:
            stale = [path for path, generated in outputs if path.read_text() != generated]
            for path in stale:
                print(f"generated output is stale: {path}", file=sys.stderr)
            if stale:
                return 1
        else:
            for path, generated in outputs:
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(generated)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
