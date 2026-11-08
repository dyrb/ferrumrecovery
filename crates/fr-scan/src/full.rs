//! One sequential sector pass, then bounded reconstruction of discovered layouts.
use super::{Progress, Request};
use fr_core::carve::{self, Area, Carver};
use fr_core::error::Error;
use fr_core::{Catalog, Entry, FsInfo, FsKind, Part, Source, ef};
use fr_core::{
    check::{SweepChecks, SweepStats},
    source::IoSnapshot,
};
use std::collections::HashSet;
use std::time::{Duration, Instant};

#[derive(Default, Clone, Debug)]
pub struct Stage {
    pub elapsed: Duration,
    pub io: IoSnapshot,
}

impl Stage {
    fn finish(src: &Source, start: Instant, before: IoSnapshot) -> Self {
        Self { elapsed: start.elapsed(), io: src.io_snapshot().since(before) }
    }
}

#[derive(Default, Clone, Debug)]
pub struct Timings {
    pub reading: Stage,
    pub layouts: Stage,
    pub formats: Stage,
    pub checking: Stage,
    pub assembly: Stage,
}

#[derive(Default, Clone, Debug)]
pub struct FullReport {
    pub timings: Box<Timings>,
    pub checks: Option<SweepStats>,
    pub io: IoSnapshot,
    pub bytes: u64,
    pub bad_bytes: u64,
    pub carved: u64,
    pub text_fragments: u64,
    pub volumes: u64,
    pub records: u64,
    pub candidate_limit: bool,
    pub rejected_layouts: u64,
}

pub fn empty(part: &Part) -> Catalog {
    Catalog::new(
        FsInfo {
            kind: part.fs,
            offset: part.offset,
            size: part.size,
            cluster_size: 512,
            record_size: if part.fs == FsKind::Ntfs { 1024 } else { 0 },
            mft: vec![],
            serial: 0,
            root: 0,
            data_offset: 0,
        },
        vec![Entry { id: 0, parent: fr_core::NONE, flags: ef::DIR, ..Default::default() }],
        vec![],
        vec![],
    )
}

pub fn run(
    src: &Source,
    part: &Part,
    cat: &mut Catalog,
    req: &Request,
    progress: &Progress,
    has_metadata: bool,
) -> Result<FullReport, Error> {
    let reading_start = Instant::now();
    let reading_io = src.io_snapshot();
    let mut ntfs_geometry = has_metadata && cat.fs.kind == FsKind::Ntfs;
    if !has_metadata
        && part.fs == FsKind::Ntfs
        && let Ok(header) = src.read_bytes(part.offset, 512)
        && let Ok(boot) = fr_ntfs::boot::BootSector::parse(&header)
        && boot.volume_bytes() <= part.size
    {
        cat.fs.cluster_size = boot.cluster_size;
        cat.fs.record_size = boot.record_size;
        cat.fs.serial = boot.serial;
        cat.unit = boot.cluster_size;
        ntfs_geometry = true;
    }
    cat.sector_units();
    let mut checks = req.check.then(|| SweepChecks::new(cat));
    let mut report = FullReport::default();
    let mut carver = Carver::default();
    let mut boots: Vec<(u64, [u8; 512])> = Vec::new();
    let mut records = fr_ntfs::loose::Records::new(req.streams);
    let current = fr_core::part::find(src).unwrap_or_default();
    let mut covered = fr_ntfs::loose::mft_spans(cat);
    covered.sort_unstable_by_key(|s| s.start);
    let inside_mft = |at: u64, len: u64| {
        let i = covered.partition_point(|s| s.start <= at);
        i != 0 && covered[i - 1].contains(at, len)
    };
    // Physical I/O stays aligned to the actual device; candidate headers use 512-byte steps.
    let sector = 512;
    let units = part.size / sector as u64;
    report.bytes = part.size;
    let mut boot_limit = false;
    let mut tail_records = RecordTail::default();
    let mut prepare = |start: u64, bytes: &[u8], bad: u64| {
        let chunk_at = start * 512;
        tail_records.complete(src, part.offset, chunk_at, bytes, bad == 0, &mut records);
        let mut candidates = Vec::new();
        for (k, h) in bytes.as_chunks::<512>().0.iter().enumerate() {
            let at = chunk_at + k as u64 * 512;
            if let Some(fmt) = carve::detect(h) {
                candidates.push((at / 512, fmt));
            }
            if h[510..512] == [0x55, 0xaa] && [0xeb, 0xe9].contains(&h[0]) {
                let plausible = fr_ntfs::boot::BootSector::parse(h).is_ok()
                    || fr_fat::exfat::parse_boot(h).is_some()
                    || fr_core::part::detect_fs(h) == FsKind::Fat32 && fr_fat::fat::parse_boot(h).is_some()
                    || fr_core::part::detect_fs(h) == FsKind::Fat16 && fr_fat::fat::parse_boot(h).is_some();
                if plausible {
                    if boots.len() < 1024 {
                        boots.push((at, *h));
                    } else {
                        boot_limit = true;
                    }
                }
            }
            if &h[..4] != b"FILE" {
                continue;
            }
            let rs = u32::from_le_bytes(h[28..32].try_into().unwrap());
            if !rs.is_power_of_two()
                || !(512..=65536).contains(&rs)
                || inside_mft(at, rs as u64)
                || at.checked_add(rs as u64).is_none_or(|end| end > part.size)
            {
                continue;
            }
            let p = k * 512;
            if tail_records.pending.is_empty()
                && let Some(record) = bytes.get(p..p + rs as usize)
            {
                records.observe(at, record);
            } else {
                // Keep later records behind the first crossing record, including group-limit decisions.
                tail_records.pending.push((at, rs));
            }
        }
        tail_records.remember(chunk_at, bytes, bad == 0);
        candidates
    };
    let mut commit = |start: u64, bytes: &[u8], bad: u64, candidates| {
        let chunk_at = start * 512;
        if let Some(checks) = &mut checks {
            checks.observe(part.offset + chunk_at, bytes, bad == 0);
        }
        carver.observe_content_status(chunk_at, bytes, 512, bad == 0);
        carver.cands.extend(candidates);
    };
    report.bad_bytes = if req.threads == Some(1) {
        fr_core::sweep::sweep_all_status(src, part.offset, sector, units, Some(&progress.sweep), &mut |start, bytes, bad| {
            let candidates = prepare(start, bytes, bad);
            commit(start, bytes, bad, candidates);
        })
    } else {
        fr_core::sweep::sweep_all_prepared(src, part.offset, sector, units, Some(&progress.sweep), &mut prepare, &mut commit)
    };
    report.candidate_limit |= boot_limit;
    let tail = (part.size % 512) as usize;
    if tail != 0 {
        match src.read_bytes(part.offset + units * 512, tail) {
            Ok(mut bytes) => {
                if let Some(checks) = &mut checks {
                    checks.observe(part.offset + units * 512, &bytes, true);
                }
                carver.observe_content(units * 512, &bytes, 512);
                bytes.resize(512, 0);
                carver.observe_start(units, &bytes);
            }
            Err(_) => report.bad_bytes += tail as u64,
        }
    }
    src.checkpoint()?;
    carver.finish_content(true);
    report.timings.reading = Stage::finish(src, reading_start, reading_io);
    let layout_start = Instant::now();
    let layout_io = src.io_snapshot();
    progress.analysis_stage.store(3, std::sync::atomic::Ordering::Relaxed);
    report.candidate_limit |= records.rejected_groups != 0;
    let mut next = 50_000_000_000u64;
    let mut seen = HashSet::new();
    for (at, header) in boots {
        src.checkpoint()?;
        if let Ok(boot) = fr_ntfs::boot::BootSector::parse(&header) {
            let size = boot.volume_bytes();
            let mut bases =
                vec![Some(at), at.checked_sub(size), at.checked_add(boot.bytes_per_sector as u64).and_then(|p| p.checked_sub(size))];
            bases.sort_unstable();
            bases.dedup();
            for base in bases.into_iter().flatten() {
                if base.checked_add(size).is_none_or(|end| end > part.size) {
                    continue;
                }
                let key = (base, size, boot.serial, boot.cluster_size, boot.record_size, boot.mft_lcn);
                if !seen.insert(key) {
                    continue;
                }
                if has_metadata
                    && base == 0
                    && cat.fs.kind == FsKind::Ntfs
                    && cat.fs.serial == boot.serial
                    && cat.fs.cluster_size == boot.cluster_size
                    && cat.fs.record_size == boot.record_size
                    && cat.fs.mft.first().is_some_and(|r| r.lcn * 512 == boot.mft_lcn * boot.cluster_size as u64)
                {
                    continue;
                }
                let absolute = part.offset + base;
                let options = fr_ntfs::ScanOptions {
                    existing: true,
                    streams: req.streams,
                    threads: req.threads.unwrap_or(2).max(1),
                    ..Default::default()
                };
                match fr_ntfs::scan::scan_with_boot(src, absolute, boot, &options) {
                    Ok(mut scan) => {
                        // A metadata claim outside the selected range is never imported.
                        scan.catalog.sector_units();
                        let spans = fr_ntfs::loose::mft_spans(&scan.catalog).into_iter().map(|mut span| {
                            span.start += base;
                            span.end += base;
                            span
                        });
                        let historical = !current.iter().any(|p| p.offset == absolute && p.fs == fr_core::FsKind::Ntfs)
                            || src.read_bytes(absolute, 512).ok().and_then(|h| fr_ntfs::boot::BootSector::parse(&h).ok()) != Some(boot);
                        match cat.import_volume(scan.catalog, &format!("NTFS {base}"), next, historical) {
                            Ok(n) => {
                                covered.extend(spans);
                                next += n + 1;
                                report.volumes += 1;
                            }
                            Err(_) => report.rejected_layouts += 1,
                        }
                    }
                    Err(_) => report.rejected_layouts += 1,
                }
            }
        } else {
            let kind = fr_core::part::detect_fs(&header);
            let exfat = fr_fat::exfat::parse_boot(&header);
            let fat = fr_fat::fat::parse_boot(&header);
            let (size, bps, cs, serial, fat_off, backup) = if let Some(b) = &exfat {
                (b.total_bytes, b.bps, b.cluster_size, b.serial, b.fat_off, 12)
            } else if let Some(b) = &fat {
                let backup = if b.bits == 32 { u16::from_le_bytes([header[50], header[51]]) as u64 } else { 0 };
                (b.total_bytes, b.bps, b.cluster_size, b.serial, b.fat_off, backup)
            } else {
                continue;
            };
            let mut bases = vec![Some(at)];
            if backup != 0 && backup != 0xffff {
                bases.push(at.checked_sub(backup * bps as u64));
            }
            bases.sort_unstable();
            bases.dedup();
            for base in bases.into_iter().flatten() {
                if base.checked_add(size).is_none_or(|end| end > part.size) {
                    continue;
                }
                if has_metadata && base == 0 && kind == cat.fs.kind && cat.fs.serial == serial as u64 && cat.fs.cluster_size == cs {
                    break;
                }
                if !seen.insert((base, size, serial as u64, cs, 0, fat_off)) {
                    continue;
                }
                let absolute = part.offset + base;
                let options = fr_fat::ScanOptions { existing: true, ..Default::default() };
                let result = if let Some(b) = &exfat {
                    fr_fat::exfat::scan_with_boot(src, absolute, b.clone(), &options)
                } else {
                    fr_fat::fat::scan_with_boot(src, absolute, fat.clone().unwrap(), &options)
                };
                match result {
                    Ok(scan) => {
                        let valid_table = src.read_bytes(absolute + fat_off, 8).ok().is_some_and(|h| {
                            let a = u32::from_le_bytes(h[..4].try_into().unwrap());
                            let b = u32::from_le_bytes(h[4..8].try_into().unwrap());
                            match fat.as_ref().map(|b| b.bits) {
                                Some(12) => a & 0x00ff_fff0 == 0x00ff_fff0,
                                Some(16) => a & 0xffff_fff0 == 0xffff_fff0,
                                _ => a & 0x0fff_ffff >= 0x0fff_fff0 && b & 0x0fff_ffff >= 0x0fff_fff8,
                            }
                        });
                        if scan.catalog.entries.len() == 1 && !valid_table {
                            report.rejected_layouts += 1;
                            continue;
                        }
                        let historical = !current.iter().any(|p| p.offset == absolute && p.fs == kind)
                            || src.read_bytes(absolute, 512).ok().is_none_or(|h| {
                                if let Some(b) = &exfat {
                                    fr_fat::exfat::parse_boot(&h).as_ref() != Some(b)
                                } else {
                                    fr_fat::fat::parse_boot(&h) != fat
                                }
                            });
                        match cat.import_volume(scan.catalog, &format!("{} {base}", kind.name()), next, historical) {
                            Ok(n) => {
                                next += n + 1;
                                report.volumes += 1;
                                break;
                            }
                            Err(_) => report.rejected_layouts += 1,
                        }
                    }
                    Err(_) => report.rejected_layouts += 1,
                }
            }
        }
    }
    report.records = records.finish(src, cat, &covered, &mut next, ntfs_geometry)?;
    report.timings.layouts = Stage::finish(src, layout_start, layout_io);
    src.checkpoint()?;
    let area = Area { src, base: part.offset, cs: 512, clusters: part.size.div_ceil(512) };
    progress.analysis_stage.store(4, std::sync::atomic::Ordering::Relaxed);
    let format_start = Instant::now();
    let format_io = src.io_snapshot();
    let carved = carve::resolve_indexed(&area, &carver, part.size, Some(&progress.analysis));
    drop(carver);
    report.timings.formats = Stage::finish(src, format_start, format_io);
    report.text_fragments = carved.iter().filter(|c| c.fmt == carve::Fmt::Text).count() as u64;
    src.checkpoint()?;
    progress.analysis_stage.store(5, std::sync::atomic::Ordering::Relaxed);
    let assembly_start = Instant::now();
    let assembly_io = src.io_snapshot();
    report.carved = carve::merge_carved(cat, carved, true);
    report.timings.assembly = Stage::finish(src, assembly_start, assembly_io);
    if let Some(checks) = checks {
        let start = Instant::now();
        let before = src.io_snapshot();
        report.checks = Some(checks.finish(src, cat, if req.threads == Some(1) { 1 } else { 4 }, Some(&progress.checks)));
        src.checkpoint()?;
        report.timings.checking = Stage::finish(src, start, before);
    }
    let sort_start = Instant::now();
    cat.sort_ids();
    report.timings.assembly.elapsed += sort_start.elapsed();
    Ok(report)
}

/// Keep only record fragments crossing a sweep edge, never seek backwards during healthy reads.
#[derive(Default)]
struct RecordTail {
    bytes: Vec<u8>,
    pending: Vec<(u64, u32)>,
    start: u64,
    end: u64,
    readable: bool,
}

impl RecordTail {
    fn complete(&mut self, src: &Source, base: u64, at: u64, bytes: &[u8], readable: bool, records: &mut fr_ntfs::loose::Records) {
        let contiguous = self.end == at && self.readable && readable;
        if contiguous && !self.pending.is_empty() {
            let needed = self.pending.iter().map(|&(p, n)| (p + n as u64).saturating_sub(at)).max().unwrap_or(0);
            self.bytes.extend_from_slice(&bytes[..bytes.len().min(needed as usize)]);
        }
        for &(pos, len) in &self.pending {
            let record = contiguous
                .then(|| pos.checked_sub(self.start))
                .flatten()
                .and_then(|offset| self.bytes.get(offset as usize..offset as usize + len as usize));
            if let Some(record) = record {
                records.observe(pos, record);
            } else if let Ok(record) = src.read_bytes(base + pos, len as usize) {
                // Bad/missing observations retain the original exact-read behavior.
                records.observe(pos, &record);
            }
        }
        self.pending.clear();
        self.bytes.clear();
    }

    fn remember(&mut self, at: u64, bytes: &[u8], readable: bool) {
        if let Some(&(first, _)) = self.pending.first() {
            self.start = first;
            self.end = at + bytes.len() as u64;
            self.readable = readable;
            self.bytes.extend_from_slice(&bytes[(first - at) as usize..]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_ntfs::record::{attr, ns, synth::RecordBuilder};

    #[test]
    fn crossing_records_reuse_observed_bytes_and_bad_blocks_keep_the_read_fallback() {
        let path = std::env::temp_dir().join(format!("fr-record-tail-{}.img", std::process::id()));
        let mut bytes = vec![0; 8192];
        let mut record = RecordBuilder::new(4096)
            .unused()
            .file_name(5, 1, "tail.txt", ns::WIN32)
            .resident(attr::DATA, "", b"exact resident bytes")
            .build(0x1234);
        record[44..48].copy_from_slice(&12345u32.to_le_bytes());
        bytes[512..512 + record.len()].copy_from_slice(&record);
        std::fs::write(&path, &bytes).unwrap();
        let src = Source::open(path.to_str().unwrap()).unwrap().with_stats(std::sync::Arc::new(fr_core::source::IoStats::default()));
        let part = Part { index: 0, offset: 0, size: bytes.len() as u64, fs: FsKind::Unknown, label: String::new() };
        for readable in [true, false] {
            let before = src.io_snapshot();
            let mut tail = RecordTail::default();
            let mut records = fr_ntfs::loose::Records::new(false);
            tail.pending.push((512, 4096));
            tail.remember(0, &bytes[..1024], readable);
            tail.complete(&src, 0, 1024, &bytes[1024..], true, &mut records);
            assert!(tail.pending.is_empty());
            assert_eq!(src.io_snapshot().since(before).calls, u64::from(!readable));
            let mut cat = empty(&part);
            records.finish(&src, &mut cat, &[], &mut 50_000_000_000, false).unwrap();
            let i = (0..cat.entries.len() as u32).find(|&i| cat.name(i) == "tail.txt").unwrap();
            let mut restored = Vec::new();
            fr_core::data::Reader::new(&src, &cat)
                .unwrap()
                .read(i, &mut |b| {
                    restored.extend_from_slice(b);
                    Ok(())
                })
                .unwrap();
            assert_eq!(restored, b"exact resident bytes");
        }
        drop(src);
        std::fs::remove_file(path).unwrap();
    }
}
