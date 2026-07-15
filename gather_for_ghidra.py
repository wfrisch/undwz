#!/usr/bin/env python3
"""Produce a single self-contained, Ghidra-friendly ELF for one target binary.

Pipeline (target only):
  1. Locate the separate .debug file for the target (via .gnu_debuglink or the
     build-id), the way gdb/elfutils would.
  2. Run `undwz` on that debug file. undwz rewrites the DWARF into a form Ghidra
     can load: DW_UT_partial units become DW_UT_compile, and any dwz supplementary
     (.gnu_debugaltlink / .dwz) content is inlined — resolved via the debug file's
     own relative altlink, so nothing needs to be copied first. (--keep-links is
     passed so the build-id survives for the eu-unstrip match below.)
  3. Combine the (stripped) target binary with the cleaned debug into one ELF
     using `eu-unstrip`.
  4. Strip .note.gnu.build-id and .gnu_debuglink from the result so Ghidra (and
     readelf/gdb) use the embedded DWARF instead of following a link back to the
     system's original dwz'd debug file.

Requires a locally built undwz at <this-dir>/target/release/undwz
(see the undwz README: ./setup-vendor.sh && cargo build --release).

External tools: readelf, objcopy, eu-unstrip (binutils/elfutils).
"""

import argparse
import os
import shutil
import subprocess
import sys
import tempfile

DEFAULT_DEBUG_ROOT = "/usr/lib/debug"
SCRIPT_DIR = os.path.dirname(os.path.realpath(__file__))
UNDWZ = os.path.join(SCRIPT_DIR, "target", "release", "undwz")


def eprint(*args):
    print(*args, file=sys.stderr)


def run(cmd, **kw):
    """Run a command, returning CompletedProcess. Never raises on non-zero."""
    return subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, **kw)


def require_tools(*tools):
    missing = [t for t in tools if shutil.which(t) is None]
    if missing:
        eprint("error: required tool(s) not found in PATH: " + ", ".join(missing))
        eprint("install them (e.g. binutils, elfutils) and retry.")
        sys.exit(1)


def is_elf(path):
    try:
        with open(path, "rb") as fh:
            return fh.read(4) == b"\x7fELF"
    except OSError:
        return False


def dump_section(elf_path, section):
    """Return the raw bytes of an ELF section, or None if it is absent."""
    with tempfile.NamedTemporaryFile(delete=False) as tmp:
        tmp_path = tmp.name
    try:
        cp = run(["objcopy", "--dump-section", f"{section}={tmp_path}", elf_path])
        if cp.returncode != 0:
            return None
        with open(tmp_path, "rb") as fh:
            data = fh.read()
        return data if data else None
    finally:
        os.unlink(tmp_path)


def get_debuglink(elf_path):
    """Return the filename stored in .gnu_debuglink, or None."""
    data = dump_section(elf_path, ".gnu_debuglink")
    if not data:
        return None
    # Layout: NUL-terminated name, padding, 4-byte CRC.
    return data.split(b"\x00", 1)[0].decode("utf-8", "replace") or None


def get_build_id(elf_path):
    """Return the build-id as a lowercase hex string, or None."""
    cp = run(["readelf", "-n", elf_path])
    if cp.returncode != 0:
        return None
    for line in cp.stdout.decode("utf-8", "replace").splitlines():
        line = line.strip()
        if line.startswith("Build ID:"):
            return line.split(":", 1)[1].strip()
    return None


def find_debug_file(elf_path, debug_root):
    """Locate the separate .debug file for elf_path following the standard
    gdb/elfutils search order. Returns an absolute path or None."""
    real = os.path.realpath(elf_path)
    directory = os.path.dirname(real)

    candidates = []
    name = get_debuglink(real)
    if name:
        candidates.append(os.path.join(directory, name))
        candidates.append(os.path.join(directory, ".debug", name))
        # Global debug dir mirrors the full path of the binary.
        candidates.append(os.path.join(debug_root, directory.lstrip("/"), name))

    build_id = get_build_id(real)
    if build_id and len(build_id) > 2:
        candidates.append(
            os.path.join(debug_root, ".build-id", build_id[:2], build_id[2:] + ".debug")
        )

    for cand in candidates:
        cand = os.path.realpath(cand)
        if os.path.isfile(cand) and cand != real:
            return cand
    return None


def present_sections(elf_path, names):
    """Return the subset of `names` that exist as sections in elf_path."""
    cp = run(["readelf", "-SW", elf_path])
    text = cp.stdout.decode("utf-8", "replace")
    return [n for n in names if f" {n} " in text or text.rstrip().endswith(f" {n}")]


def main():
    ap = argparse.ArgumentParser(
        description="Build one self-contained, Ghidra-friendly ELF from a target "
        "binary and its (dwz'd) DWARF debug info, via undwz + eu-unstrip."
    )
    ap.add_argument("target", help="path to the ELF binary (e.g. /usr/bin/bash)")
    ap.add_argument(
        "-o",
        "--output",
        help="output ELF path (default: ./<target>.ghidra)",
    )
    ap.add_argument(
        "--debug-root",
        default=DEFAULT_DEBUG_ROOT,
        help=f"global debug directory (default: {DEFAULT_DEBUG_ROOT})",
    )
    args = ap.parse_args()

    require_tools("readelf", "objcopy", "eu-unstrip")
    if not os.path.isfile(UNDWZ) or not os.access(UNDWZ, os.X_OK):
        eprint(f"error: undwz not found at {UNDWZ}")
        eprint("build it first: ./setup-vendor.sh && cargo build --release")
        sys.exit(1)

    target = os.path.realpath(args.target)
    if not os.path.isfile(target):
        eprint(f"error: no such file: {args.target}")
        sys.exit(1)
    if not is_elf(target):
        eprint(f"error: not an ELF file: {args.target}")
        sys.exit(1)

    output = args.output or os.path.basename(target) + ".ghidra"

    print(f"target: {target}")
    debug_file = find_debug_file(target, args.debug_root)
    if not debug_file:
        eprint("error: no separate .debug file found for this target")
        eprint("(is the matching *-debuginfo package installed?)")
        sys.exit(1)
    print(f"  debug: {debug_file}")

    workdir = tempfile.mkdtemp(prefix="gather-ghidra-")
    try:
        # 1. Normalize the DWARF (partial units -> compile, alt/.dwz inlined).
        #    undwz resolves the supplementary file via the debug file's own
        #    relative altlink, so we point it straight at the system debug file.
        #    --keep-links preserves the build-id that eu-unstrip matches on.
        clean_debug = os.path.join(workdir, "clean.debug")
        print("  undwz: normalizing DWARF ...")
        cp = run([UNDWZ, debug_file, "-o", clean_debug, "--keep-links"])
        if cp.returncode != 0:
            eprint("  undwz failed:")
            eprint("    " + cp.stderr.decode("utf-8", "replace").strip())
            sys.exit(1)

        # 2. Combine the stripped target with the cleaned debug into one ELF.
        print("  eu-unstrip: combining binary + debug ...")
        cp = run(["eu-unstrip", target, clean_debug, "-o", output])
        if cp.returncode != 0:
            eprint("  eu-unstrip failed:")
            eprint("    " + cp.stderr.decode("utf-8", "replace").strip())
            sys.exit(1)

        # 3. Drop external-debug links so Ghidra uses the embedded DWARF and
        #    isn't lured back to the system's original dwz'd debug file.
        #    .gnu_debugaltlink matters here: undwz strips it from the debug file
        #    and inlines the supplementary units, but eu-unstrip re-merges it
        #    from the stripped target binary (which ships its own copy). Left in,
        #    Ghidra would follow it to the system .dwz file and load its
        #    DW_UT_partial units again.
        strip = present_sections(
            output, [".note.gnu.build-id", ".gnu_debuglink", ".gnu_debugaltlink"]
        )
        if strip:
            cmd = ["objcopy"]
            for s in strip:
                cmd += ["--remove-section", s]
            cmd.append(output)
            run(cmd)
            print("  stripped: " + ", ".join(strip))
    finally:
        shutil.rmtree(workdir, ignore_errors=True)

    print(f"\nDone. Ghidra-ready ELF: {os.path.realpath(output)}")


if __name__ == "__main__":
    main()
