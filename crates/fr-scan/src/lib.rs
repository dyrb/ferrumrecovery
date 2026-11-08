//! # fr-scan
//!
//! One entry point for every scanner, shared by the command line and the window: open a
//! source, choose its volume, run the scanner its file system needs (or the signature
//! search for a source with none), and report what was found as data, so each interface
//! words it in its own language.

use fr_core::error::Error;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fr_core::filter::format_size;
use fr_core::model::Catalog;
use fr_core::part::{self, FsKind, Part};
use fr_core::source::Source;
use fr_core::sweep::SweepProgress;

pub use fr_fat::FatReport;
pub use fr_ntfs::{Owner, ScanReport};
mod full;
pub use full::FullReport;

/// What to look for.
#[derive(Clone, Debug, Default)]
pub struct Request {
    /// Files that are not deleted too.
    pub existing: bool,
    /// Which file owns every cluster (NTFS, for "overwritten by").
    pub owners: bool,
    /// Names without data (NTFS change journal and folder index slack).
    pub traces: bool,
    /// Read free space: lost folders, records outside the MFT, file signatures.
    pub deep: bool,
    /// Metadata plus sector-step signature/layout search over the whole selected range.
    pub full: bool,
    /// Full search reuses sweep bytes for content checks of all eligible entries.
    pub check: bool,
    /// Named data streams (NTFS).
    pub streams: bool,
    pub threads: Option<usize>,
}

/// Live progress of a scan, readable from another thread.
#[derive(Clone, Default)]
pub struct Progress {
    /// Reading the MFT (NTFS).
    pub mft: Arc<fr_ntfs::Progress>,
    /// Reading free space (deep search, raw search).
    pub sweep: Arc<SweepProgress>,
    pub checks: Arc<fr_core::check::Progress>,
    pub analysis: Arc<fr_core::carve::ResolveProgress>,
    pub analysis_stage: Arc<std::sync::atomic::AtomicU8>,
}

impl Progress {
    /// Phases: metadata, sweep, checks, layouts, formats, assembling results.
    pub fn fraction(&self) -> (u8, Option<f64>) {
        use std::sync::atomic::Ordering::Relaxed;
        let ct = self.checks.total.load(Relaxed);
        if ct > 0 {
            return (2, Some(self.checks.done.load(Relaxed) as f64 / ct as f64));
        }
        let phase = self.analysis_stage.load(Relaxed);
        if phase != 0 {
            let total = self.analysis.total.load(Relaxed);
            return (phase, (phase == 4 && total > 0).then(|| self.analysis.done.load(Relaxed) as f64 / total as f64));
        }
        let st = self.sweep.total.load(Relaxed);
        if st > 0 {
            return (1, Some(self.sweep.done.load(Relaxed) as f64 / st as f64));
        }
        let t = self.mft.total.load(Relaxed);
        (0, (t > 0).then(|| self.mft.done.load(Relaxed) as f64 / t as f64))
    }

    pub fn found(&self) -> u64 {
        self.mft.found.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[cfg(test)]
mod progress_tests {
    use super::Progress;
    use std::sync::atomic::Ordering::Relaxed;

    #[test]
    fn completed_sweep_does_not_mask_analysis_or_checks() {
        let p = Progress::default();
        p.sweep.total.store(100, Relaxed);
        p.sweep.done.store(100, Relaxed);
        assert_eq!(p.fraction(), (1, Some(1.0)));
        p.analysis_stage.store(3, Relaxed);
        assert_eq!(p.fraction(), (3, None));
        p.analysis_stage.store(4, Relaxed);
        p.analysis.total.store(500, Relaxed);
        p.analysis.done.store(125, Relaxed);
        assert_eq!(p.fraction(), (4, Some(0.25)));
        p.analysis_stage.store(5, Relaxed);
        assert_eq!(p.fraction(), (5, None));
        p.checks.total.store(200, Relaxed);
        p.checks.done.store(100, Relaxed);
        assert_eq!(p.fraction(), (2, Some(0.5)));
    }
}

/// How the scan went.
#[derive(Clone, Debug)]
pub enum Report {
    Full {
        metadata: Option<Box<Report>>,
        metadata_time: Duration,
        error: Option<Error>,
        scan: FullReport,
    },
    Ntfs(ScanReport),
    Fat {
        rep: FatReport,
        exfat: bool,
        deep: bool,
    },
    Raw {
        bytes: u64,
        found: u64,
    },
    /// Loaded from a saved scan; the notes it was saved with.
    Saved {
        path: String,
        notes: Vec<String>,
    },
}

pub struct Scanned {
    pub catalog: Catalog,
    pub owners: Vec<Owner>,
    pub report: Report,
    pub took: Duration,
}

impl Scanned {
    pub fn checks_complete(&self) -> bool {
        matches!(&self.report, Report::Full { scan, .. } if scan.checks.is_some())
    }
    pub fn warnings(&self) -> Vec<Error> {
        Self::warnings_for(&self.report)
    }

    fn warnings_for(report: &Report) -> Vec<Error> {
        if let Report::Full { metadata, error, scan, .. } = report {
            let mut out = error.iter().cloned().collect::<Vec<_>>();
            if scan.bad_bytes != 0 {
                out.push(Error::value("{} bytes could not be read", scan.bad_bytes));
            }
            if scan.candidate_limit {
                out.push(Error::new("historical layout candidate limit reached; some layouts were not processed"));
            }
            if let Some(meta) = metadata {
                out.extend(Self::warnings_for(meta));
            }
            return out;
        }
        let (bad, corrupt, no_allocation, deep_skipped) = match report {
            Report::Ntfs(rep) => (rep.bad_bytes, rep.corrupt, rep.no_bitmap, rep.deep_skipped),
            Report::Fat { rep, .. } => (rep.bad_bytes, 0, rep.no_allocation, false),
            _ => (0, 0, false, false),
        };
        let mut out = Vec::new();
        if bad != 0 {
            out.push(Error::value("{} bytes could not be read", bad));
        }
        if corrupt != 0 {
            out.push(Error::value("{} damaged file records were skipped", corrupt));
        }
        if no_allocation {
            out.push(Error::new("allocation metadata could not be read; file states may be unknown"));
        }
        if deep_skipped {
            out.push(Error::new("deep search of old MFT records was skipped for this cluster size"));
        }
        out
    }
}

/// Open `C:`, `disk1`, `shadow3` or an image, with a plain message when admin rights are
/// missing.
pub fn open_source(spec: &str) -> Result<Source, Error> {
    Source::open(spec).map_err(|e| {
        let denied = e.raw_os_error() == Some(5);
        let error = Error::from(e);
        if denied { Error::cause("reading a drive directly needs administrator rights", error).context(spec) } else { error.context(spec) }
    })
}

pub fn supported(fs: FsKind) -> bool {
    matches!(fs, FsKind::Ntfs | FsKind::Fat32 | FsKind::Fat16 | FsKind::ExFat)
}

/// The volume of `src` to work on: partition `want`, the file system at byte `offset`,
/// the whole source as raw data, or the only supported volume.
pub fn pick_part(src: &Source, want: Option<u32>, offset: Option<u64>, raw: bool) -> Result<Part, Error> {
    pick_part_mode(src, want, offset, raw, false)
}

/// Full search can also examine an unrecognized, explicitly selected range.
pub fn pick_part_mode(src: &Source, want: Option<u32>, offset: Option<u64>, raw: bool, full: bool) -> Result<Part, Error> {
    if raw {
        let off = offset.unwrap_or(0);
        return Ok(Part { index: 0, offset: off, size: src.size.saturating_sub(off), fs: FsKind::Unknown, label: String::new() });
    }
    if let Some(off) = offset {
        let b = src.read_bytes(off, 512).map_err(|e| Error::from(e).context(&src.name))?;
        let (fs, size) = part::boot_size(&b)
            .or_else(|| full.then_some((FsKind::Unknown, src.size.saturating_sub(off))))
            .ok_or_else(|| Error::value("no file system at offset {}", off).context(&src.name))?;
        return Ok(Part { index: 0, offset: off, size, fs, label: String::new() });
    }
    let parts = part::find(src).map_err(|e| Error::from(e).context(&src.name))?;
    if parts.is_empty() {
        if full && want.is_none() {
            return Ok(Part { index: 0, offset: 0, size: src.size, fs: FsKind::Unknown, label: String::new() });
        }
        return Err(Error::new("no file system or partition table found").context(&src.name));
    }
    let chosen = match want {
        Some(n) => parts.iter().find(|p| p.index == n).ok_or_else(|| Error::value("no partition {}", n).context(&src.name))?,
        None => {
            let usable: Vec<&Part> = parts.iter().filter(|p| supported(p.fs)).collect();
            match usable.as_slice() {
                [one] => *one,
                [] => &parts[0],
                _ => {
                    let list: Vec<String> =
                        usable.iter().map(|p| format!("  --part {}  {} {}", p.index, p.fs.name(), format_size(p.size))).collect();
                    return Err(Error::value("several partitions, choose one:\n{}", list.join("\n")).context(&src.name));
                }
            }
        }
    };
    if !supported(chosen.fs) && !full {
        return Err(Error::new("no NTFS, FAT or exFAT file system found; --raw looks for files by their signatures").context(&src.name));
    }
    Ok(chosen.clone())
}

/// Scan the volume `part` of `src`.
pub fn scan(src: &Source, part: &Part, req: &Request, progress: &Progress) -> Result<Scanned, Error> {
    let t = Instant::now();
    if part.size == 0 || part.offset.checked_add(part.size).is_none_or(|end| end > src.size) {
        return Err(Error::new("scan range is outside the source").context(&src.name));
    }
    if req.full {
        let measured = src.try_clone()?.with_stats(Arc::new(Default::default()));
        let src = &measured;
        let metadata_time;
        let mut scan = if part.fs == FsKind::Unknown {
            None
        } else {
            let mut metadata = req.clone();
            metadata.full = false;
            metadata.deep = false;
            metadata.check = false;
            metadata.existing = true;
            Some(self::scan(src, part, &metadata, progress))
        };
        let (mut catalog, mut owners, report, error) = match scan.take() {
            Some(Ok(s)) => {
                metadata_time = s.took;
                (s.catalog, s.owners, Some(Box::new(s.report)), None)
            }
            Some(Err(e)) => {
                metadata_time = t.elapsed();
                (full::empty(part), vec![], None, Some(e))
            }
            None => {
                metadata_time = Duration::ZERO;
                (full::empty(part), vec![], None, None)
            }
        };
        src.checkpoint()?;
        catalog.fs.size = part.size;
        let factor = catalog.unit as u64 / 512;
        for owner in &mut owners {
            owner.lcn *= factor;
            owner.len *= factor;
        }
        let mut scanned = full::run(src, part, &mut catalog, req, progress, report.is_some())?;
        scanned.io = src.io_snapshot();
        return Ok(Scanned {
            catalog,
            owners,
            report: Report::Full { metadata: report, metadata_time, error, scan: scanned },
            took: t.elapsed(),
        });
    }
    if part.fs == FsKind::Unknown {
        let catalog = fr_core::carve::scan_raw(src, part.offset, part.size, Some(&progress.sweep));
        let found = catalog.entries.iter().filter(|e| !e.is_dir()).count() as u64;
        return Ok(Scanned { catalog, owners: Vec::new(), report: Report::Raw { bytes: part.size, found }, took: t.elapsed() });
    }
    if part::boot_size(&src.read_bytes(part.offset, 512)?).is_some_and(|(_, size)| size > part.size) {
        return Err(Error::new("scan range is outside the source").context(&src.name));
    }
    if part.fs != FsKind::Ntfs {
        let fopts = fr_fat::ScanOptions { existing: req.existing, deep: req.deep, progress: Some(progress.sweep.clone()) };
        let s = fr_fat::scan(src, part.offset, part.fs, &fopts).map_err(|e| e.context(&src.name))?;
        let report = Report::Fat { rep: s.report, exfat: part.fs == FsKind::ExFat, deep: req.deep };
        return Ok(Scanned { catalog: s.catalog, owners: Vec::new(), report, took: t.elapsed() });
    }
    let mut opts = fr_ntfs::ScanOptions {
        existing: req.existing,
        owners: req.owners,
        traces: req.traces,
        deep: req.deep,
        streams: req.streams,
        sweep_progress: Some(progress.sweep.clone()),
        progress: Some(progress.mft.clone()),
        ..Default::default()
    };
    if let Some(n) = req.threads {
        opts.threads = n.max(1);
    }
    let s = fr_ntfs::scan(src, part.offset, &opts).map_err(|e| Error::from(e).context(&src.name))?;
    Ok(Scanned { catalog: s.catalog, owners: s.owners, report: Report::Ntfs(s.report), took: t.elapsed() })
}

/// A saved scan with its source opened again, checked to be the same volume.
pub fn load_session(path: &Path) -> Result<(Source, Part, Scanned), Error> {
    let shown = path.display().to_string();
    let s = fr_core::session::load(path).map_err(|e| Error::from(e).context(&shown))?;
    let src = open_source(&s.source)?;
    if s.part.offset.checked_add(s.part.size).is_none_or(|end| end > src.size) {
        return Err("Saved scan lies outside the source".into());
    }
    let now = src.read_bytes(s.part.offset, 512).map_err(|e| Error::from(e).context(&s.source))?;
    if now != s.boot {
        return Err(Error::new("this is not the volume that was scanned (its boot sector differs)").context(&s.source));
    }
    let report = Report::Saved { path: shown, notes: s.notes };
    Ok((src, s.part, Scanned { catalog: s.catalog, owners: Vec::new(), report, took: Duration::ZERO }))
}

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

/// The report in English, one line per fact (the command line, saved scans).
pub fn english(s: &Scanned) -> Vec<String> {
    english_report(&s.report, s.took)
}

fn english_report(report: &Report, took: Duration) -> Vec<String> {
    let secs = took.as_secs_f64();
    match report {
        Report::Full { metadata, metadata_time, error, scan } => {
            let mut lines = metadata.as_deref().map(|r| english_report(r, *metadata_time)).unwrap_or_default();
            if let Some(e) = error {
                lines.push(format!("Metadata: {e}"));
            }
            lines.push(format!(
                "Full search: {} read in {secs:.1} s, {} volume layouts, {} loose records, {} signature files, {} text fragments",
                format_size(scan.bytes),
                num(scan.volumes),
                num(scan.records),
                num(scan.carved.saturating_sub(scan.text_fragments)),
                num(scan.text_fragments)
            ));
            for (name, stage) in [
                ("Reading", &scan.timings.reading),
                ("Layouts", &scan.timings.layouts),
                ("Formats", &scan.timings.formats),
                ("Checks", &scan.timings.checking),
                ("Assembly", &scan.timings.assembly),
            ] {
                lines.push(format!(
                    "  {name}: {:.3} s, {} read requests, {} bytes requested",
                    stage.elapsed.as_secs_f64(),
                    num(stage.io.calls),
                    num(stage.io.bytes)
                ));
            }
            lines.push(format!("  Total I/O: {} read requests, {} bytes requested", num(scan.io.calls), num(scan.io.bytes)));
            if let Some(checks) = scan.checks {
                lines.push(format!(
                    "  Checks: {} reused from sweep, {} direct, {} zero-filled, {} mismatched",
                    num(checks.reused as u64),
                    num(checks.fallback as u64),
                    num(checks.stats.zeroed as u64),
                    num(checks.stats.mismatch as u64)
                ));
            }
            lines
        }
        Report::Raw { bytes, found } => {
            vec![format!("Raw search: {} read in {secs:.1} s, {} files found by signature", format_size(*bytes), num(*found))]
        }
        Report::Fat { rep, exfat, deep } => {
            let mut v = vec![format!("Folders: {} read in {secs:.1} s", num(rep.dirs))];
            if rep.bad_bytes > 0 {
                v.push(format!("  {} could not be read (bad sectors)", format_size(rep.bad_bytes)));
            }
            if *deep {
                v.push(format!("  deep search: {} lost folders, {} files by signature", num(rep.lost_dirs), num(rep.carved)));
            }
            if rep.no_allocation {
                v.push("  allocation metadata could not be read: states are not known".into());
            }
            if !exfat {
                v.push("  FAT keeps no cluster chain for deleted files: they are read as one piece from their first cluster".into());
            }
            v
        }
        Report::Saved { path, notes } => {
            let mut v = vec![format!("Saved scan {path}")];
            v.extend(notes.iter().cloned());
            v
        }
        Report::Ntfs(rep) => {
            let mut v = vec![format!("MFT: {} records read in {secs:.1} s", num(rep.records))];
            if rep.from_mirror {
                v.push("  the start of the MFT is damaged, its copy ($MFTMirr) was used".into());
            }
            if rep.bad_bytes > 0 {
                v.push(format!("  {} could not be read (bad sectors)", format_size(rep.bad_bytes)));
            }
            if rep.corrupt > 0 {
                v.push(format!("  {} damaged records skipped", num(rep.corrupt)));
            }
            if rep.no_bitmap {
                v.push("  the cluster bitmap could not be read: states are not known".into());
            }
            let mut more = Vec::new();
            if rep.recycled > 0 {
                more.push(format!("{} Recycle Bin items with their original paths", num(rep.recycled)));
            }
            if rep.dated > 0 {
                more.push(format!("{} deletion times from the change journal", num(rep.dated)));
            }
            if rep.traces > 0 {
                more.push(format!("{} names without data", num(rep.traces)));
            }
            if !more.is_empty() {
                v.push(format!("  {}", more.join(", ")));
            }
            if rep.no_journal {
                v.push("  no change journal: deletion times are not known".into());
            }
            if rep.found > 0 || rep.carved > 0 {
                v.push(format!("  deep search: {} file records in free space, {} files by signature", num(rep.found), num(rep.carved)));
            }
            if rep.deep_skipped {
                v.push("  deep search skipped: clusters smaller than file records".into());
            }
            v
        }
    }
}
