//! undwz — reassemble dwz-optimized DWARF into self-contained DWARF that Ghidra
//! can load: no DW_UT_partial unit headers and no .gnu_debugaltlink references.
//!
//! Pipeline:
//!   1. open an ELF, load its DWARF (and, if present, the .gnu_debugaltlink
//!      supplementary/.dwz file so alt-references resolve while reading),
//!   2. report unit counts, partial units, imported_unit DIEs, alt-form usage,
//!   3. with -o: convert the read DWARF to writable DWARF and re-emit it. gimli's
//!      writer emits every unit as DW_UT_compile, which removes the DW_UT_partial
//!      unit headers Ghidra chokes on (NSA/ghidra#6850). The rebuilt .debug_*
//!      sections are spliced back into a copy of the input ELF with objcopy.
//!
//! STATUS: fully handled for both no-altlink inputs (bash, libc, libtinfo,
//! ld-linux, ...) and dwz multifile/altlink inputs (e.g. cpio). The vendored
//! gimli converter pulls the supplementary (.dwz) units into the output and
//! resolves every alt-reference (DW_FORM_GNU_ref_alt/strp_alt), so the result is
//! always self-contained: no DW_UT_partial headers, no alt forms, no
//! .gnu_debugaltlink.

use std::borrow::Cow;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use object::{Object, ObjectSection};

type Endian = gimli::RunTimeEndian;

/// A section loader for `obj`: returns owned bytes for the given DWARF section.
fn section_loader<'a>(
    obj: &'a object::File,
) -> impl FnMut(gimli::SectionId) -> Result<Cow<'static, [u8]>, Box<dyn Error>> + 'a {
    move |id: gimli::SectionId| match obj.section_by_name(id.name()) {
        Some(section) => Ok(Cow::Owned(section.uncompressed_data()?.into_owned())),
        None => Ok(Cow::Owned(Vec::new())),
    }
}

fn endian_of(obj: &object::File) -> Endian {
    if obj.is_little_endian() {
        gimli::RunTimeEndian::Little
    } else {
        gimli::RunTimeEndian::Big
    }
}

/// Parse the .gnu_debugaltlink section: returns the (path, build_id) it names.
fn altlink(obj: &object::File) -> Option<(String, Vec<u8>)> {
    let data = obj.section_by_name(".gnu_debugaltlink")?.data().ok()?;
    let nul = data.iter().position(|&b| b == 0)?;
    let path = String::from_utf8_lossy(&data[..nul]).into_owned();
    let build_id = data[nul + 1..].to_vec();
    Some((path, build_id))
}

/// Resolve the supplementary file path (relative to the debug file's directory).
fn resolve_sup(input: &Path, altpath: &str) -> PathBuf {
    let p = Path::new(altpath);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        input.parent().unwrap_or(Path::new(".")).join(p)
    }
}

#[derive(Default)]
struct UnitStats {
    total: usize,
    partial: usize,
    compile: usize,
    imported_units: usize,
    uses_alt_ref: bool,
    uses_alt_str: bool,
}

fn scan<R: gimli::Reader>(dwarf: &gimli::Dwarf<R>) -> Result<UnitStats, Box<dyn Error>> {
    let mut s = UnitStats::default();
    let mut units = dwarf.units();
    while let Some(header) = units.next()? {
        s.total += 1;
        match header.type_() {
            gimli::UnitType::Partial => s.partial += 1,
            _ => s.compile += 1,
        }
        let unit = dwarf.unit(header)?;
        let mut entries = unit.entries();
        while let Some(entry) = entries.next_dfs()? {
            if entry.tag() == gimli::DW_TAG_imported_unit {
                s.imported_units += 1;
            }
            for attr in entry.attrs() {
                match attr.raw_value() {
                    gimli::AttributeValue::DebugInfoRefSup(_) => s.uses_alt_ref = true,
                    gimli::AttributeValue::DebugStrRefSup(_) => s.uses_alt_str = true,
                    _ => {}
                }
            }
        }
    }
    Ok(s)
}

/// Convert the read DWARF to writable DWARF and emit fresh .debug_* section
/// bytes. gimli's writer emits every unit as DW_UT_compile, dropping the
/// DW_UT_partial headers. Returns (section name, bytes) pairs (non-empty only).
fn rebuild_sections<R: gimli::Reader<Offset = usize>>(
    dwarf: &gimli::Dwarf<R>,
    endian: Endian,
) -> Result<Vec<(&'static str, Vec<u8>)>, Box<dyn Error>> {
    // Debug info carries absolute addresses; keep them as-is.
    let convert_address = &|addr| Some(gimli::write::Address::Constant(addr));
    let write_dwarf = gimli::write::Dwarf::from(dwarf, convert_address)?;

    let mut sections = gimli::write::Sections::new(gimli::write::EndianVec::new(endian));
    let mut wd = write_dwarf;
    wd.write(&mut sections)?;

    let mut out = Vec::new();
    sections.for_each(|id, data: &gimli::write::EndianVec<Endian>| -> Result<(), Box<dyn Error>> {
        let bytes = data.slice();
        if !bytes.is_empty() {
            out.push((id.name(), bytes.to_vec()));
        }
        Ok(())
    })?;
    Ok(out)
}

/// Splice rebuilt debug sections into a copy of `input`, written to `output`.
/// Removes every existing .debug_* section plus .gnu_debugaltlink, then adds the
/// freshly generated ones. Uses objcopy for the ELF surgery.
fn splice(
    input: &Path,
    output: &Path,
    obj: &object::File,
    new_sections: &[(&'static str, Vec<u8>)],
    strip_links: bool,
) -> Result<(), Box<dyn Error>> {
    fs::copy(input, output)?;

    let mut cmd = Command::new("objcopy");

    // Remove all existing debug sections and the alt link. Unless the caller
    // opts out, also remove the build-id note and .gnu_debuglink so no consumer
    // follows them to an external (stale, dwz'd) debug file instead of the
    // self-contained DWARF we embed here.
    for section in obj.sections() {
        if let Ok(name) = section.name() {
            let is_debug = name.starts_with(".debug_") || name == ".gnu_debugaltlink";
            let is_link = strip_links
                && (name == ".gnu_debuglink" || name == ".note.gnu.build-id");
            if is_debug || is_link {
                cmd.arg("--remove-section").arg(name);
            }
        }
    }

    // Stage new section contents to temp files and add them.
    let tmpdir = std::env::temp_dir().join(format!("undwz-{}", std::process::id()));
    fs::create_dir_all(&tmpdir)?;
    let mut tmpfiles = Vec::new();
    for (name, bytes) in new_sections {
        let f = tmpdir.join(name.trim_start_matches('.'));
        fs::write(&f, bytes)?;
        cmd.arg("--add-section").arg(format!("{name}={}", f.display()));
        cmd.arg("--set-section-flags").arg(format!("{name}=readonly"));
        tmpfiles.push(f);
    }

    cmd.arg(output);
    let status = cmd.status()?;
    for f in tmpfiles {
        let _ = fs::remove_file(f);
    }
    let _ = fs::remove_dir(&tmpdir);
    if !status.success() {
        return Err(format!("objcopy failed with status {status}").into());
    }
    Ok(())
}

struct Args {
    input: PathBuf,
    output: Option<PathBuf>,
    /// Keep .note.gnu.build-id and .gnu_debuglink in the output. By default they
    /// are stripped so no consumer (readelf, Ghidra, gdb) can be lured to an
    /// external/stale debug file instead of the self-contained embedded DWARF.
    keep_links: bool,
}

const USAGE: &str = "usage: undwz <elf-with-dwarf> [-o <output-elf>] [--keep-links]";

fn parse_args() -> Args {
    let mut input = None;
    let mut output = None;
    let mut keep_links = false;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" | "--output" => output = it.next().map(PathBuf::from),
            "--keep-links" => keep_links = true,
            "-h" | "--help" => {
                eprintln!("{USAGE}");
                std::process::exit(0);
            }
            _ => input = Some(PathBuf::from(a)),
        }
    }
    let input = input.unwrap_or_else(|| {
        eprintln!("{USAGE}");
        std::process::exit(2);
    });
    Args {
        input,
        output,
        keep_links,
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = parse_args();

    let main_bytes = fs::read(&args.input)?;
    let obj = object::File::parse(&*main_bytes)?;
    let endian = endian_of(&obj);

    println!("input: {}", args.input.display());

    let mut owned = gimli::Dwarf::load(section_loader(&obj))?;

    // Follow .gnu_debugaltlink -> supplementary file, if present. `owned.borrow`
    // below automatically borrows the sup sections too, once set here.
    let sup_bytes;
    let sup_obj;
    if let Some((altpath, _build_id)) = altlink(&obj) {
        let sup_path = resolve_sup(&args.input, &altpath);
        println!("  altlink: {altpath}  ->  {}", sup_path.display());
        sup_bytes = fs::read(&sup_path)
            .map_err(|e| format!("cannot read supplementary file {}: {e}", sup_path.display()))?;
        sup_obj = object::File::parse(&*sup_bytes)?;
        owned.load_sup(section_loader(&sup_obj))?;
    } else {
        println!("  altlink: none");
    }

    // `Dwarf::borrow` is deprecated in favour of `DwarfSections::borrow`, but it
    // is the documented way to borrow an owned Dwarf together with its sup file.
    #[allow(deprecated)]
    let dwarf = owned.borrow(|s| gimli::EndianSlice::new(s, endian));

    let s = scan(&dwarf)?;
    println!(
        "  units: {} (compile {}, partial {})",
        s.total, s.compile, s.partial
    );
    println!("  DW_TAG_imported_unit DIEs: {}", s.imported_units);
    println!(
        "  uses alt refs: ref={}, str={}",
        s.uses_alt_ref, s.uses_alt_str
    );

    let Some(output) = args.output else {
        return Ok(());
    };

    println!("rebuilding DWARF (partial units -> DW_UT_compile, alt refs inlined) ...");
    let new_sections = rebuild_sections(&dwarf, endian)?;
    for (name, bytes) in &new_sections {
        println!("  emit {name}: {} bytes", bytes.len());
    }
    if !args.keep_links {
        println!("  stripping .note.gnu.build-id / .gnu_debuglink (use --keep-links to retain)");
    }
    splice(&args.input, &output, &obj, &new_sections, !args.keep_links)?;
    println!("wrote {}", output.display());
    Ok(())
}
