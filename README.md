> NOTICE: This is merely a one-off experiment. I do NOT intend to maintain or
> support this.

# undwz — normalize dwz-optimized DWARF for Ghidra

Ghidra cannot parse `DW_UT_partial` compilation units and throws
`Unsupported unitType 3, DW_UT_partial` (NSA/ghidra#6850). openSUSE's debug info
is run through `dwz`, which emits exactly those partial units (both in the
per-file debug info *and*, for `dwz -m` multifile builds, in a shared
supplementary `.dwz` file referenced via `.gnu_debugaltlink`).

This tool reads the DWARF with [`gimli`](https://github.com/gimli-rs/gimli),
re-emits it, and splices the rebuilt sections back into a copy of the ELF with
`objcopy`. gimli's writer emits every unit as `DW_UT_compile`, which removes the
partial-unit headers Ghidra rejects — while preserving the DIE tree and line
programs.

## Status

| Input | Result |
|-------|--------|
| **No `.gnu_debugaltlink`** (bash, libc, libtinfo, ld-linux, …) | ✅ **Fully handled.** Output is self-contained: all units `DW_UT_compile`, line info preserved, `readelf`/`llvm-dwarfdump` clean. |
| **Has `.gnu_debugaltlink`** (dwz `-m` multifile, e.g. cpio) | ✅ **Fully handled.** The supplementary (`.dwz`) units are pulled in as ordinary units and every alt-reference (`DW_FORM_GNU_ref_alt`/`strp_alt`) is inlined/resolved. Output is self-contained: no alt forms, no `.gnu_debugaltlink`. Verified: e.g. cpio → 184 main + 121 sup = 305 units, all `DW_UT_compile`, 0 alt refs, 0 `llvm-dwarfdump` errors. |

### Verification
A simple way to check if an ELF contains problematic `DW_UT_partial` units:
```
readelf --debug-dump=info SOME_ELF_WITH_DEBUGINFO | grep -i 'Unit Type' |sort -u
```

The output shout not contain `DW_UT_partial`.

### External debug links are stripped by default

To keep the output unambiguously self-contained, `undwz` removes
`.note.gnu.build-id` and `.gnu_debuglink` by default. Otherwise a consumer that
follows those links (readelf, gdb, and Ghidra all do) could load the *original*
dwz'd debug file from the system (e.g. `/usr/lib/debug/.build-id/…`, which still
has `DW_UT_partial` and `<alt …>` refs) instead of the clean embedded DWARF.
Pass `--keep-links` to retain them.

Note: with links intact, `readelf --debug-dump=…` follows them by default —
inspect the actual embedded DWARF with `--debug-dump=no-follow-links` or
`llvm-dwarfdump`.

## Build

Requires a C toolchain for linking (`cc`) and `objcopy` (binutils) at runtime.

```sh
./setup-vendor.sh        # one-time: materialize vendor/gimli from the patch
cargo build --release
```

undwz depends on a **patched gimli**. Rather than committing a full copy of the
crate, the repo ships only the diff — `patches/gimli-0.34.0-undwz.patch` — and
`setup-vendor.sh` reconstructs `vendor/gimli` (pristine gimli 0.34.0 + patch),
which `[patch.crates-io]` in `Cargo.toml` overrides the crates.io gimli with.
`vendor/` is git-ignored, so the patch file is the single source of truth. The
script needs only `tar`, `patch`, and `curl`/`wget` (it reuses cargo's crate
cache when present, no network otherwise) — no extra Rust tooling.

> Why not `cargo patch-crate` / a `build.rs` patcher? Those pull ~400 crates
> (openssl, gitoxide, crypto) as a build-dependency, compiled before every build.
> A plain patch + `patch(1)` is far lighter for the same result.

### To change the gimli patch

```sh
cd vendor/gimli && edit …                        # hack on the vendored copy
cd - && diff -u <(pristine) vendor/gimli > …     # or regenerate the patch:
#   see the header of patches/gimli-0.34.0-undwz.patch for the exact form
```
In practice: edit `vendor/gimli`, then regenerate the patch by diffing against a
pristine extraction of `gimli-0.34.0.crate` (paths as `a/src/...` `b/src/...`,
apply with `patch -p1`).

### What the patch changes (two edits on top of gimli 0.34.0)

1. **Line-writer fix** (`src/write/line.rs`). gimli asserts
   `line_base + (line_range as i8) > 0`, but `line_range` is a `u8` that GCC
   routinely sets to `242`. Casting to `i8` makes it negative and the assertion
   aborts, even though the row-generation code itself uses the full `u8` value
   correctly. The two assertions (lines 112, 445) are widened to `i16`.

2. **Supplementary (alt) inlining** (`src/write/unit.rs`, in `mod convert`).
   Stock gimli passes `.gnu_debugaltlink` references through unchanged
   (`DebugInfoRefSup`/`DebugStrRefSup`). The extension makes the converter also
   reserve and convert the sup file's units into the output (recorded in a
   separate `sup_entry_ids` map, since sup and main share one flat
   `UnitSectionOffset` space), then resolves each alt-reference to the inlined
   DIE (`DW_FORM_GNU_ref_alt` → local `ref_addr`) or inlines the alt-string
   (`DW_FORM_GNU_strp_alt` → `strp`). The result has no supplementary references.

## Usage

```sh
# One shot: the sibling meta script locates the target's debug info, runs undwz,
# and merges everything into a single self-contained ELF:
./gather_for_ghidra.py /usr/bin/bash -o bash.ghidra

# Or run undwz directly on an ELF that already carries its DWARF:
undwz bash.with-debug -o bash.ghidra

# Report only (no output file):
undwz bash.with-debug

# Keep .note.gnu.build-id / .gnu_debuglink (stripped by default):
undwz bash.with-debug -o bash.ghidra --keep-links
```

Typical run:

```
input: out/bash
  altlink: none
  units: 890 (compile 160, partial 730)
  DW_TAG_imported_unit DIEs: 3233
  uses alt refs: ref=false, str=false
rebuilding DWARF (partial units -> DW_UT_compile, alt refs inlined) ...
  emit .debug_info: 919363 bytes
  ...
wrote out/bash.ghidra
```

## How it works

1. `object` opens the ELF; `gimli::Dwarf::load` loads the `.debug_*` sections.
2. If `.gnu_debugaltlink` is present, the supplementary file is located
   (relative to the debug file) and attached via `load_sup`, so alt-references
   resolve during reading.
3. `gimli::write::Dwarf::from` converts read → write DWARF. gimli's writer emits
   `DW_UT_compile` for every unit, and the vendored converter additionally
   inlines the sup file's units and resolves all alt-references (see the gimli
   patch notes above).
4. The rebuilt sections are written to temp files and spliced into a copy of the
   input with `objcopy --remove-section`/`--add-section` (all original
   `.debug_*` and `.gnu_debugaltlink` sections are removed first).

## Possible follow-ups

* Upstream the line-writer `u8`/`i8` fix to gimli.
* The vendored converter change routes `DebugStrRefSup` string resolution through
  `read_unit.dwarf.sup()`; a de-dup pass already happens via gimli's string
  table, so no extra work is needed there.
