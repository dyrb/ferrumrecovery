//! FerrumRecovery command line.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use fr_core::filter::{Filter, Pattern, format_size, parse_size};
use fr_core::model::{Catalog, Data, Run, State, TRACE_ID_BASE, ef};
use fr_core::part::{self, FsKind, Part};
use fr_core::recover::{self, Outcome};
use fr_core::source::Source;
use fr_core::types::Group;
use fr_core::{check, time};

const USAGE: &str = concat!(
    "FerrumRecovery ",
    env!("CARGO_PKG_VERSION"),
    "
Recovers deleted files from NTFS, FAT and exFAT volumes, disks and disk images.

Usage:
  ferrumrecovery disks
  ferrumrecovery scan <source> [filters] [options]
  ferrumrecovery info <source> <id>
  ferrumrecovery partitions <disk or image>
  ferrumrecovery image <source> <file.img>
  ferrumrecovery recover <source> --to <folder> [filters] [options]

Source:
  C:                 drive letter (needs administrator rights)
  disk1              whole physical disk (needs administrator rights)
  shadow3            volume shadow copy (previous versions), see `disks`
  image.img          raw disk or volume image, or a fixed VHD
  scan.frs           a scan saved with --save
  --part N           partition N of a disk or disk image
  --offset BYTES     a file system at this byte offset (see `partitions`)

Filters (deleted files and Recycle Bin items are listed; folders, empty and system
files are hidden):
  --name MASK        file name, * and ? wildcards; repeat for several masks
  --path MASK        full path, wildcards or a part of it; repeatable
  --type GROUP       photo, video, audio, document, archive, mail, other
  --min-size SIZE    for example 100K, 10M, 1.5G
  --max-size SIZE
  --after DATE       modified on or after YYYY-MM-DD
  --before DATE      modified before YYYY-MM-DD
  --deleted-after DATE   deleted on or after YYYY-MM-DD (time from the change journal
                         or the Recycle Bin)
  --deleted-before DATE  deleted before YYYY-MM-DD
  --state STATE      good, partial, overwritten, zeroed, no-data, encrypted, mismatch,
                     unknown (allocation metadata is unavailable)
  --id N[,N...]      only these ids
  --existing         also files that are not deleted
  --dirs             also folders
  --empty            also empty files
  --system           also file system metadata
  --streams          also the named data streams of files (NTFS), recovered as
                     name.ext_stream
  --traces           also names of deleted files whose data location is gone, from the
                     change journal and folder indexes (reads every folder index)

Options:
  --deep             also read all free space: folders cut off from the tree and the
                     folders left by a quick format (FAT, exFAT), file records outside
                     the MFT (NTFS), and files by their signatures (photos, video,
                     audio, documents, archives); takes as long as reading the free space
  --full             metadata plus sector-step search of the entire selected range,
                     including occupied space and surviving historical file systems
  --raw              no file system: look for files by their signatures over the whole
                     source (or from --offset), sector by sector
  --signatures FILE  more signatures for --deep and --raw, one per line:
                     extension hex-bytes [offset] [max-size], e.g. blend 424C454E444552 0 2G
  --check            sample file data for zeros and unexpected type signatures
  --sort KEY         path, size, date, deleted or state (default path)
  --limit N          rows to show, 0 for all (default 100)
  --csv FILE         write the list to a CSV file
  --save FILE.frs    save the scan; give FILE.frs as the source later to list, filter
                     and recover without scanning again
  --flat             recover all files into one folder instead of their original folders
  --threads N        worker threads
"
);

#[derive(Default)]
struct Args {
    cmd: String,
    source: Option<String>,
    id: Option<u64>,
    part: Option<u32>,
    offset: Option<u64>,
    deep: bool,
    full: bool,
    raw: bool,
    filter: Filter,
    check: bool,
    sort: String,
    limit: usize,
    csv: Option<PathBuf>,
    save: Option<PathBuf>,
    to: Option<PathBuf>,
    flat: bool,
    threads: Option<usize>,
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let mut a = Args { sort: "path".into(), limit: 100, ..Default::default() };
    a.cmd = it.next().ok_or_else(|| USAGE.to_string())?;
    if matches!(a.cmd.as_str(), "-h" | "--help" | "help") {
        return Err(USAGE.to_string());
    }
    let mut free = Vec::new();
    while let Some(s) = it.next() {
        let mut val = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        let f = &mut a.filter;
        match s.as_str() {
            "--name" => f.name.push(Pattern::new(&val("--name")?)),
            "--path" => f.path.push(Pattern::new(&val("--path")?)),
            "--type" => {
                let v = val("--type")?;
                f.groups.push(Group::parse(&v).ok_or_else(|| format!("unknown type {v}"))?);
            }
            "--min-size" => f.min_size = Some(parse_size(&val("--min-size")?).ok_or("bad --min-size")?),
            "--max-size" => f.max_size = Some(parse_size(&val("--max-size")?).ok_or("bad --max-size")?),
            "--after" => f.after = Some(time::parse_date(&val("--after")?).ok_or("--after expects YYYY-MM-DD")?),
            "--before" => f.before = Some(time::parse_date(&val("--before")?).ok_or("--before expects YYYY-MM-DD")?),
            "--deleted-after" => {
                f.deleted_after = Some(time::parse_date(&val("--deleted-after")?).ok_or("--deleted-after expects YYYY-MM-DD")?)
            }
            "--deleted-before" => {
                f.deleted_before = Some(time::parse_date(&val("--deleted-before")?).ok_or("--deleted-before expects YYYY-MM-DD")?)
            }
            "--traces" => f.traces = true,
            "--streams" => f.streams = true,
            "--state" => {
                let v = val("--state")?;
                f.states.push(State::parse(&v).ok_or_else(|| format!("unknown state {v}"))?);
            }
            "--id" => {
                for p in val("--id")?.split(',') {
                    f.ids.push(p.trim().parse().map_err(|_| format!("bad id {p}"))?);
                }
            }
            "--existing" => f.existing = true,
            "--dirs" => f.dirs = true,
            "--empty" => f.empty = true,
            "--system" => f.system = true,
            "--check" => a.check = true,
            "--sort" => a.sort = val("--sort")?,
            "--limit" => a.limit = val("--limit")?.parse().map_err(|_| "bad --limit")?,
            "--csv" => a.csv = Some(val("--csv")?.into()),
            "--save" => a.save = Some(val("--save")?.into()),
            "--to" => a.to = Some(val("--to")?.into()),
            "--flat" => a.flat = true,
            "--part" => a.part = Some(val("--part")?.parse().map_err(|_| "bad --part")?),
            "--offset" => a.offset = Some(val("--offset")?.parse().map_err(|_| "bad --offset")?),
            "--deep" => a.deep = true,
            "--full" => a.full = true,
            "--raw" => a.raw = true,
            "--signatures" => {
                let path = val("--signatures")?;
                let text = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
                let sigs = fr_core::carve::formats::parse_user_signatures(&text).map_err(|e| format!("{path}: {e}"))?;
                fr_core::carve::formats::set_user_signatures(sigs);
            }
            "--threads" => a.threads = Some(val("--threads")?.parse().map_err(|_| "bad --threads")?),
            "-h" | "--help" => return Err(USAGE.to_string()),
            _ if s.starts_with("--") => return Err(format!("unknown option {s}\n\n{USAGE}")),
            _ => free.push(s),
        }
    }
    let mut free = free.into_iter();
    a.source = free.next();
    if a.cmd == "info" {
        a.id = Some(free.next().ok_or("info needs an id")?.parse().map_err(|_| "bad id")?);
    }
    if a.cmd == "image" && a.to.is_none() {
        a.to = Some(free.next().ok_or("image needs a file to write")?.into());
    }
    if let Some(x) = free.next() {
        return Err(format!("unexpected argument {x}"));
    }
    if !["path", "size", "date", "deleted", "state"].contains(&a.sort.as_str()) {
        return Err("--sort expects path, size, date, deleted or state".into());
    }
    Ok(a)
}

/// `1 234 567` with thin grouping, for counts.
fn num(v: u64) -> String {
    let s = v.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

use fr_scan::{Scanned, open_source, pick_part};

/// Scan with progress on stderr.
fn scan_source(src: &Source, part: &Part, args: &Args, existing: bool, owners: bool) -> Result<Scanned, String> {
    let req = fr_scan::Request {
        existing,
        owners,
        traces: args.filter.traces || args.id.is_some_and(|id| id >= TRACE_ID_BASE),
        deep: args.deep,
        full: args.full,
        check: args.check && args.full,
        streams: args.filter.streams,
        threads: args.threads,
    };
    let p = fr_scan::Progress::default();
    let done = AtomicBool::new(false);
    let first = match part.fs {
        FsKind::Ntfs => "Reading MFT",
        FsKind::Unknown => "Reading",
        _ => "Reading folders",
    };
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut shown = false;
            let mut sweep_start: Option<Instant> = None;
            while !done.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(200));
                let (phase, frac) = p.fraction();
                let line = match (phase, frac) {
                    (0, Some(f)) if part.fs == FsKind::Ntfs => format!("{first} {:.0}% ({} deleted found)", f * 100.0, num(p.found())),
                    (0, _) => continue,
                    (2, f) => format!("Checking data {:.0}%", f.unwrap_or(0.0) * 100.0),
                    (3, _) => "Analyzing file systems".to_owned(),
                    (4, f) => format!(
                        "Analyzing file formats {:.0}% ({} / {})",
                        f.unwrap_or(0.0) * 100.0,
                        num(p.analysis.done.load(Ordering::Relaxed)),
                        num(p.analysis.total.load(Ordering::Relaxed))
                    ),
                    (5, _) => "Assembling results".to_owned(),
                    (_, f) => {
                        let f = f.unwrap_or(0.0);
                        let t0 = *sweep_start.get_or_insert_with(Instant::now);
                        let secs = t0.elapsed().as_secs_f64();
                        let left = if f > 0.01 { format!(", {:.0} s left", secs / f - secs) } else { String::new() };
                        let label = if args.full {
                            "Reading full range"
                        } else if part.fs == FsKind::Unknown {
                            first
                        } else {
                            "Reading free space"
                        };
                        format!("{label} {:.0}%{left}", f * 100.0)
                    }
                };
                eprint!("\r{line:70}");
                shown = true;
            }
            if shown {
                eprint!("\r{:70}\r", "");
            }
        });
        let r = fr_scan::scan(src, part, &req, &p);
        done.store(true, Ordering::Relaxed);
        r.map_err(String::from)
    })
}

static STOP: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn on_ctrl(_: u32) -> i32 {
    STOP.store(true, Ordering::Relaxed);
    1
}

/// Copy a whole disk, image or volume (or a partition of it) into an image file, healthy
/// areas first, bad ones again afterwards; resumes from `<file>.map`.
fn cmd_image(args: &Args) -> Result<(), String> {
    let spec = args.source.as_deref().ok_or("image needs a source")?;
    let out = args.to.as_ref().ok_or("image needs a file to write")?;
    let src = open_source(spec)?;
    let (start, len) = if args.part.is_some() || args.offset.is_some() {
        let p = pick_part(&src, args.part, args.offset, false)?;
        (p.offset, p.size)
    } else {
        (0, src.size)
    };
    let dir = std::path::absolute(out).map_err(|e| e.to_string())?;
    let parent = dir.parent().ok_or("bad image path")?;
    recover::check_target(&src, start, len, parent)?;
    let mut map_path = dir.clone().into_os_string();
    map_path.push(".map");
    let map_path = PathBuf::from(map_path);
    if map_path.exists() {
        println!("Resuming from {}", map_path.display());
    }
    println!("Imaging {} of {} to {} (Ctrl+C stops and keeps the place)", format_size(len), src.name, dir.display());
    // SAFETY: registers a plain function; it only sets an atomic flag.
    unsafe { windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(on_ctrl), 1) };
    let mut last = Instant::now();
    let map = fr_core::image::image(
        &src,
        start,
        len,
        &dir,
        &map_path,
        &mut |p| {
            if last.elapsed() < Duration::from_millis(250) && p.done < p.total {
                return;
            }
            last = Instant::now();
            let secs = p.elapsed.as_secs_f64().max(0.001);
            let speed = p.copied as f64 / secs;
            let left =
                if speed > 0.0 && p.pass == 1 { format!(", {:.0} s left", (p.total - p.done) as f64 / speed) } else { String::new() };
            eprint!(
                "\rPass {}: {:.1}%  {} copied, {} bad, {}/s{left}      ",
                p.pass,
                p.done as f64 * 100.0 / p.total.max(1) as f64,
                format_size(p.copied),
                format_size(p.bad),
                format_size(speed as u64)
            );
        },
        &|| STOP.load(Ordering::Relaxed),
    )
    .map_err(|e| format!("{}: {e}", dir.display()))?;
    eprintln!();
    use fr_core::image::Status;
    if STOP.load(Ordering::Relaxed) {
        println!("Stopped; run the same command again to continue.");
    } else {
        println!(
            "Done: {} copied, {} unreadable (left as zeros)",
            format_size(map.bytes(Status::Copied)),
            format_size(map.bytes(Status::Bad))
        );
    }
    Ok(())
}

fn cmd_partitions(args: &Args) -> Result<(), String> {
    let spec = args.source.as_deref().ok_or("partitions needs a disk or an image")?;
    let src = open_source(spec)?;
    println!("Partition table of {}:", src.name);
    match part::find(&src) {
        Ok(parts) if !parts.is_empty() => {
            for p in parts {
                println!("  partition {}  {:<7}  {:>9}  at {}", p.index, p.fs.name(), format_size(p.size), p.offset);
            }
        }
        _ => println!("  none"),
    }
    println!("Searching for file systems by their boot sectors...");
    let found = part::search(&src, &mut |d, t| {
        if t > 0 {
            eprint!("\r  {:.0}%   ", d as f64 * 100.0 / t as f64);
        }
    });
    eprint!("\r{:20}\r", "");
    if found.is_empty() {
        println!("  nothing found");
    }
    for f in &found {
        let note = match (f.in_table, f.from_backup) {
            (true, _) => "in the table",
            (false, false) => "not in the table",
            (false, true) => "not in the table, found by its backup boot sector",
        };
        println!("  {:<7}  {:>9}  at {:>14}  {note}", f.fs.name(), format_size(f.size), f.offset);
    }
    if found.iter().any(|f| !f.in_table) {
        println!("Scan one with: ferrumrecovery scan {} --offset <bytes>", src.name);
    }
    Ok(())
}

fn is_session(args: &Args) -> bool {
    args.source.as_deref().is_some_and(|s| s.to_ascii_lowercase().ends_with(".frs"))
}

/// How to open the source again later: image paths become absolute.
fn source_spec(args: &Args) -> Result<String, String> {
    let spec = args.source.clone().ok_or("no source")?;
    let p = std::path::Path::new(&spec);
    if p.is_file() {
        return Ok(std::path::absolute(p).map_err(|e| e.to_string())?.to_string_lossy().into_owned());
    }
    Ok(spec)
}

/// The source, its volume and the catalog: scanned now, or loaded from a saved scan.
fn obtain(args: &Args, existing: bool, owners: bool) -> Result<(Source, Part, Scanned), String> {
    let spec = args.source.as_deref().ok_or("needs a source")?;
    if is_session(args) {
        return fr_scan::load_session(std::path::Path::new(spec)).map_err(String::from);
    }
    let src = open_source(spec)?;
    let part = fr_scan::pick_part_mode(&src, args.part, args.offset, args.raw, args.full)?;
    let scanned = scan_source(&src, &part, args, existing, owners)?;
    Ok((src, part, scanned))
}

/// Entries the filters accept. With `--check` the state filter waits for the check.
fn select(args: &Args, cat: &Catalog) -> Vec<u32> {
    if args.check && !args.filter.states.is_empty() {
        let mut f = args.filter.clone();
        f.states.clear();
        f.select(cat)
    } else {
        args.filter.select(cat)
    }
}

fn sort_list(cat: &Catalog, list: &mut [u32], key: &str) {
    match key {
        "size" => list.sort_by_key(|&i| std::cmp::Reverse(cat.entries[i as usize].size)),
        "date" => list.sort_by_key(|&i| std::cmp::Reverse(cat.entries[i as usize].modified)),
        "deleted" => list.sort_by_key(|&i| std::cmp::Reverse(cat.entries[i as usize].deleted_at)),
        "state" => list.sort_by_key(|&i| cat.entries[i as usize].state),
        _ => {
            let mut keyed: Vec<(String, u32)> = list.iter().map(|&i| (cat.path(i).to_lowercase(), i)).collect();
            keyed.sort();
            for (dst, (_, i)) in list.iter_mut().zip(keyed) {
                *dst = i;
            }
        }
    }
}

fn state_label(s: State) -> &'static str {
    s.name()
}

fn write_csv(cat: &Catalog, list: &[u32], path: &PathBuf) -> io::Result<()> {
    let mut out = io::BufWriter::new(std::fs::File::create_new(path)?);
    out.write_all("\u{FEFF}".as_bytes())?;
    writeln!(out, "id,state,deleted,deleted_at,size,modified,created,path")?;
    for &i in list {
        let e = &cat.entries[i as usize];
        let p = cat.path(i).replace('"', "\"\"");
        writeln!(
            out,
            "{},{},{},{},{},{},{},\"{}\"",
            e.id,
            state_label(e.state),
            if e.is_deleted() {
                "yes"
            } else if e.flags & ef::RECYCLED != 0 {
                "recycle bin"
            } else if e.flags & ef::FOUND != 0 {
                "unknown"
            } else {
                "no"
            },
            time::format(e.deleted_at),
            e.size,
            time::format(e.modified),
            time::format(e.created),
            p
        )?;
    }
    out.flush()
}

fn print_header(src: &Source, part: &Part, s: &Scanned) {
    let where_ = if part.index == 0 { src.name.clone() } else { format!("{} partition {}", src.name, part.index) };
    let cat = &s.catalog;
    println!("{where_} {}, {}, cluster {}", cat.fs.kind.name(), format_size(cat.fs.size), format_size(cat.fs.cluster_size as u64));
    for line in fr_scan::english(s) {
        println!("{line}");
    }
}

fn summary(cat: &Catalog, list: &[u32]) {
    let mut by_state = [0u64; State::ALL.len()];
    let mut bytes = 0u64;
    for &i in list {
        let e = &cat.entries[i as usize];
        by_state[e.state as usize] += 1;
        bytes += e.size;
    }
    let parts: Vec<String> =
        State::ALL.iter().filter(|s| by_state[**s as usize] > 0).map(|s| format!("{} {}", s.name(), num(by_state[*s as usize]))).collect();
    println!("Found: {} ({}){}{}", num(list.len() as u64), format_size(bytes), if parts.is_empty() { "" } else { ": " }, parts.join(", "));
}

fn print_rows(cat: &Catalog, list: &[u32], limit: usize) {
    if list.is_empty() {
        return;
    }
    let shown = if limit == 0 { list.len() } else { limit.min(list.len()) };
    let idw = list[..shown].iter().map(|&i| cat.entries[i as usize].id.to_string().len()).max().unwrap_or(2).max(2);
    println!();
    println!("{:>idw$}  {:<11}  {:>9}  {:<16}  {:<16}  Path", "ID", "State", "Size", "Modified", "Deleted");
    for &i in &list[..shown] {
        let e = &cat.entries[i as usize];
        let size = if e.is_dir() { "folder".to_string() } else { format_size(e.size) };
        let mark = if e.flags & ef::TRACE != 0 {
            "  (name only)"
        } else if e.flags & ef::RECYCLED != 0 {
            "  (in the Recycle Bin)"
        } else if e.flags & ef::HISTORICAL != 0 {
            "  (historical metadata)"
        } else if e.flags & ef::FOUND != 0 && !e.is_deleted() {
            "  (signature; deletion unknown)"
        } else if !e.is_deleted() {
            "  (not deleted)"
        } else {
            ""
        };
        println!(
            "{:>idw$}  {:<11}  {:>9}  {:<16}  {:<16}  {}{}",
            e.id,
            state_label(e.state),
            size,
            time::format(e.modified),
            time::format(e.deleted_at),
            cat.path(i),
            mark
        );
    }
    if shown < list.len() {
        println!("... {} more; use --limit 0 to show all, or --csv FILE", num((list.len() - shown) as u64));
    }
}

fn threads(args: &Args) -> usize {
    args.threads.unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4))
}

fn cmd_scan(args: &Args) -> Result<(), String> {
    let (src, part, mut scan) = obtain(args, args.filter.existing, false)?;
    print_header(&src, &part, &scan);
    let mut list = select(args, &scan.catalog);
    if args.check {
        let t = Instant::now();
        let (st, elapsed) = if let fr_scan::Report::Full { scan: report, .. } = &scan.report
            && let Some(checks) = report.checks
        {
            (checks.stats, report.timings.checking.elapsed)
        } else {
            let stats = check::check(&src, &mut scan.catalog, &list, threads(args).min(16));
            (stats, t.elapsed())
        };
        println!(
            "Check: {} files have zero-filled samples, {} do not start like their type, {:.1} s",
            num(st.zeroed as u64),
            num(st.mismatch as u64),
            elapsed.as_secs_f64()
        );
        if !args.filter.states.is_empty() {
            list.retain(|&i| args.filter.states.contains(&scan.catalog.entries[i as usize].state));
        }
    }
    if let Some(p) = &args.save {
        let _target = output_target(&src, &part, p)?;
        let boot = src.read_bytes(part.offset, 512).map_err(|e| format!("{}: {e}", src.name))?;
        fr_core::session::save_catalog(p, &source_spec(args)?, &part, &boot, &fr_scan::english(&scan), &scan.catalog)
            .map_err(|e| format!("{}: {e}", p.display()))?;
        println!("Scan saved to {}", p.display());
    }
    let cat = &scan.catalog;
    sort_list(cat, &mut list, &args.sort);
    summary(cat, &list);
    print_rows(cat, &list, args.limit);
    if let Some(p) = &args.csv {
        let _target = output_target(&src, &part, p)?;
        write_csv(cat, &list, p).map_err(|e| format!("{}: {e}", p.display()))?;
        println!("List written to {}", p.display());
    }
    Ok(())
}

fn output_target(src: &Source, part: &Part, path: &std::path::Path) -> Result<recover::Target, String> {
    let abs = std::path::absolute(path).map_err(|e| e.to_string())?;
    let parent = abs.parent().ok_or("Output has no parent directory")?;
    recover::check_target(src, part.offset, part.size, parent).map_err(String::from)
}

fn cmd_info(args: &Args) -> Result<(), String> {
    let id = args.id.ok_or("info needs an id")?;
    let (src, _part, mut scan) = obtain(args, true, true)?;
    let i = scan.catalog.find(id).ok_or_else(|| format!("no file with id {id} (it may be in use by a file that is not deleted)"))?;
    check::check(&src, &mut scan.catalog, &[i], 1);
    let cat = &scan.catalog;
    let e = cat.entries[i as usize];
    let cs = cat.unit as u64;
    println!("Path:      {}", cat.path(i));
    println!("Id:        {} (sequence {})", e.id, e.seq);
    let when = if e.deleted_at != 0 { format!(", {}", time::format(e.deleted_at)) } else { String::new() };
    if e.flags & ef::RECYCLED != 0 {
        println!("Deleted:   in the Recycle Bin{when}");
    } else if e.flags & ef::FOUND != 0 && !e.is_deleted() {
        println!("Deleted:   unknown{when}");
    } else {
        println!("Deleted:   {}{when}", if e.is_deleted() { "yes" } else { "no" });
    }
    if e.flags & ef::TRACE != 0 {
        println!("Note:      only the name survived (change journal or folder index); there is no data to recover");
    }
    println!("State:     {}", e.state.name());
    if !e.is_dir() {
        println!("Size:      {} ({} bytes)", format_size(e.size), num(e.size));
        if e.valid < e.size {
            println!("Written:   {} bytes, the rest reads as zeros", num(e.valid));
        }
    }
    for (label, t) in [("Created:", e.created), ("Modified:", e.modified), ("Accessed:", e.accessed), ("Changed:", e.changed)] {
        println!("{label:<10} {}", time::format(t));
    }
    let mut flags = Vec::new();
    for (bit, name) in [
        (ef::DIR, "folder"),
        (ef::RESIDENT, "stored in the file record"),
        (ef::COMPRESSED, "compressed"),
        (ef::SPARSE, "sparse"),
        (ef::ENCRYPTED, "encrypted"),
        (ef::HAS_ADS, "has alternate data streams"),
        (ef::ORPHAN, "parent folder unknown"),
        (ef::SYSTEM, "file system metadata"),
        (ef::INCOMPLETE, "part of the data location is lost"),
        (ef::FOUND, "found by the deep search"),
    ] {
        if e.flags & bit != 0 {
            flags.push(name);
        }
    }
    if !flags.is_empty() {
        println!("Flags:     {}", flags.join(", "));
    }
    match e.data {
        Data::None if !e.is_dir() => println!("Data:      location lost"),
        Data::Resident { len, .. } => println!("Data:      {len} bytes inside the file record"),
        Data::Inline { at, len, .. } => println!("Data:      {len} bytes inside a file record found at volume byte {at}"),
        Data::RecordRuns { len, .. } => println!("Data:      {len} bytes inside a fragmented file record"),
        Data::Runs { .. } => {
            let runs = cat.runs_of(i);
            println!(
                "Data:      {} runs, {} clusters on disk, {} now used by other files",
                runs.len(),
                num(cat.real_clusters(i)),
                num(e.lost)
            );
            let mut vcn = 0u64;
            for r in runs.iter().take(32) {
                let what = match r.lcn {
                    Run::SPARSE => "sparse".to_string(),
                    Run::MISSING => "location lost".to_string(),
                    lcn => {
                        let owners = fr_ntfs::owners_of(&scan.owners, lcn, r.len);
                        if owners.is_empty() || !e.is_deleted() {
                            format!("clusters {}..{}", num(lcn), num(lcn + r.len))
                        } else {
                            let names: Vec<String> =
                                owners.iter().take(3).map(|&o| cat.find(o).map_or(format!("record {o}"), |k| cat.path(k))).collect();
                            format!("clusters {}..{}, used by {}", num(lcn), num(lcn + r.len), names.join(", "))
                        }
                    }
                };
                println!("  {:>12} +{:<8} {}", format_size(vcn * cs), num(r.len), what);
                vcn += r.len;
            }
            if runs.len() > 32 {
                println!("  ... {} more runs", runs.len() - 32);
            }
        }
        _ => {}
    }
    Ok(())
}

fn cmd_recover(args: &Args) -> Result<(), String> {
    let to = args.to.as_ref().ok_or("recover needs --to <folder>")?;
    // Check the target before anything else: a wrong target must not cost a scan.
    if !is_session(args) {
        let spec = args.source.as_deref().ok_or("recover needs a source")?;
        let src = open_source(spec)?;
        let part = fr_scan::pick_part_mode(&src, args.part, args.offset, args.raw, args.full)?;
        recover::check_target(&src, part.offset, part.size, to)?;
    }
    let (src, part, mut scan) = obtain(args, args.filter.existing, false)?;
    let target = recover::check_target(&src, part.offset, part.size, to)?;
    print_header(&src, &part, &scan);
    let mut list = select(args, &scan.catalog);
    if args.check {
        if !scan.checks_complete() {
            check::check(&src, &mut scan.catalog, &list, threads(args).min(16));
        }
        if !args.filter.states.is_empty() {
            list.retain(|&i| args.filter.states.contains(&scan.catalog.entries[i as usize].state));
        }
    }
    let cat = &scan.catalog;
    sort_list(cat, &mut list, "path");
    if list.is_empty() {
        println!("Nothing matches the filters.");
        return Ok(());
    }
    let need: u64 = list.iter().map(|&i| cat.entries[i as usize].size).sum();
    if need > target.free {
        return Err(format!("{} needs {}, the target has {} free", num(list.len() as u64), format_size(need), format_size(target.free)));
    }
    if target.same_disk {
        println!("Note: the target is on the same physical disk as the source.");
    }
    println!("Recovering {} files ({}) to {}", num(list.len() as u64), format_size(need), target.dir.display());
    let opt = recover::Options { structure: !args.flat };
    let mut counts = [0u64; 5];
    let mut bytes = 0u64;
    let total = list.len();
    let mut done = 0usize;
    let mut last = Instant::now();
    recover::recover(&src, cat, &list, &target, &opt, &mut |r| {
        done += 1;
        let k = match &r.outcome {
            Outcome::Ok => 0,
            Outcome::Partial { .. } => 1,
            Outcome::Zeroed => 2,
            Outcome::Skipped(_) => 3,
            Outcome::Failed(_) => 4,
        };
        counts[k] += 1;
        if k <= 2 {
            bytes += r.bytes;
        }
        if k != 0 {
            eprint!("\r{:60}\r", "");
            println!("{}: {}", cat.path(r.index), recover::describe(&r.outcome));
            if let Some(e) = &r.metadata_error {
                eprintln!("  metadata: {e}");
            }
        }
        if last.elapsed() >= Duration::from_millis(200) || done == total {
            last = Instant::now();
            eprint!("\rRecovered {} of {} ({})   ", num(done as u64), num(total as u64), format_size(bytes));
        }
    });
    eprint!("\r{:60}\r", "");
    println!(
        "Done: {} recovered, {} partly, {} saved with zeros, {} skipped, {} failed; {} written",
        num(counts[0]),
        num(counts[1]),
        num(counts[2]),
        num(counts[3]),
        num(counts[4]),
        format_size(bytes)
    );
    if counts[4] != 0 {
        return Err(format!("{} files could not be recovered", counts[4]));
    }
    Ok(())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Drive letter whose NTFS serial number (low 32 bits as Windows reports it) matches.
fn volume_of_serial(serial: u64) -> Option<char> {
    use windows_sys::Win32::Storage::FileSystem::{GetLogicalDrives, GetVolumeInformationW};
    let mask = unsafe { GetLogicalDrives() };
    (0..26u32).filter(|k| mask >> k & 1 != 0).map(|k| (b'A' + k as u8) as char).find(|l| {
        let root = wide(&format!("{l}:\\"));
        let mut vs = 0u32;
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                std::ptr::null_mut(),
                0,
                &mut vs,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            )
        } != 0;
        ok && vs == serial as u32
    })
}

fn cmd_disks() -> Result<(), String> {
    use windows_sys::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW};
    println!("Volumes:");
    let mask = unsafe { GetLogicalDrives() };
    for k in 0..26u32 {
        if mask >> k & 1 == 0 {
            continue;
        }
        let l = (b'A' + k as u8) as char;
        let root = wide(&format!("{l}:\\"));
        let kind = match unsafe { GetDriveTypeW(root.as_ptr()) } {
            2 => "removable",
            3 => "fixed",
            4 => "network",
            5 => "optical",
            6 => "ram disk",
            _ => "other",
        };
        let mut label = [0u16; 261];
        let mut fsname = [0u16; 261];
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fsname.as_mut_ptr(),
                fsname.len() as u32,
            )
        } != 0;
        let s = |b: &[u16]| String::from_utf16_lossy(&b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())]);
        let (mut total, mut free) = (0u64, 0u64);
        unsafe { GetDiskFreeSpaceExW(root.as_ptr(), std::ptr::null_mut(), &mut total, &mut free) };
        if ok {
            println!("  {l}:  {:<9}  {:<6}  {:>9}  {:>9} free  {}", kind, s(&fsname), format_size(total), format_size(free), s(&label));
        } else {
            println!("  {l}:  {kind}, not ready");
        }
    }
    println!("Shadow copies:");
    let mut shadows = 0;
    for n in fr_core::devices::shadow_copies().map_err(|e| e.to_string())? {
        let Ok(src) = Source::open(&format!("shadow{n}")) else { continue };
        let Ok(b) = src.read_bytes(0, 512) else { continue };
        if part::detect_fs(&b) != FsKind::Ntfs {
            continue;
        }
        let serial = u64::from_le_bytes(b[0x48..0x50].try_into().unwrap());
        let of = volume_of_serial(serial).map(|l| format!("copy of {l}:")).unwrap_or_else(|| "volume not mounted".into());
        println!("  shadow{n}  {of}");
        shadows += 1;
    }
    if shadows == 0 {
        println!("  none (or no administrator rights)");
    }
    println!("Physical disks:");
    let mut any = false;
    let mut denied = false;
    for n in fr_core::source::physical_disks().map_err(|e| e.to_string())? {
        match Source::open(&format!("disk{n}")) {
            Ok(src) => {
                any = true;
                println!("  disk{n}  {}", format_size(src.size));
                match part::find(&src) {
                    Ok(parts) if !parts.is_empty() => {
                        for p in parts {
                            let label = if p.label.is_empty() { String::new() } else { format!("  {}", p.label) };
                            println!(
                                "    partition {}  {:<7}  {:>9}  at {}{}",
                                p.index,
                                p.fs.name(),
                                format_size(p.size),
                                format_size(p.offset),
                                label
                            );
                        }
                    }
                    _ => println!("    no partition table"),
                }
            }
            Err(e) if e.raw_os_error() == Some(5) => denied = true,
            Err(_) => {}
        }
    }
    if !any {
        println!("  {}", if denied { "listing physical disks needs administrator rights" } else { "none found" });
    }
    Ok(())
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };
    let r = match args.cmd.as_str() {
        "disks" => cmd_disks(),
        "scan" => cmd_scan(&args),
        "info" => cmd_info(&args),
        "partitions" => cmd_partitions(&args),
        "image" => cmd_image(&args),
        "recover" => cmd_recover(&args),
        other => Err(format!("unknown command {other}\n\n{USAGE}")),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
    }
}
