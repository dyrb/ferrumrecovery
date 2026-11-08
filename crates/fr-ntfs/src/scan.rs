//! Finding deleted files in the MFT.
//!
//! Deleting a file on NTFS clears the "in use" flag of its record and frees its clusters;
//! the record keeps its name, parent folder, timestamps, size and data runs until NTFS
//! hands it to a new file. Every record is read; free records with a name become entries.
//!
//! ```text
//!  workers: record ──► Raw (base record) / Seg (runs of one attribute segment)
//!  merge:   sort by record ──► join data segments (base + extension records, checked by
//!           sequence numbers) ──► parents ──► system flags ──► state from $Bitmap
//!  extras:  Recycle Bin names ──► change journal (deletion times, traces) ──► index slack
//! ```
//!
//! Sequence numbers: NTFS increments a record's sequence number when it frees it. A child
//! refers to its folder as (record, sequence); a folder that was deleted later has the
//! sequence one higher than its children remember.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use fr_core::model::{Catalog, Data, Entry, FsInfo, NONE, Run, State, ef};
use fr_core::part::FsKind;
use fr_core::source::{ReadSlot, Source};

use crate::bytes::*;
use crate::error::Result;
use crate::mft::{self, Layout, Progress, Volume};
use crate::record::{AF_COMPRESSED, AF_ENCRYPTED, AF_SPARSE, REC_DIRECTORY, REC_IN_USE, apply_fixup, attr, ns, push_utf16_as_utf8};
use crate::runlist::{self, Extent};

/// MFT records with a fixed role.
pub const ROOT: u64 = 5;
const BITMAP: u64 = 6;
pub const EXTEND: u64 = 11;
/// Records below this are file system metadata or reserved.
pub(crate) const FIRST_USER: u64 = 24;

const FILE_ATTRIBUTE_ENCRYPTED: u32 = 0x4000;

/// Kinds of attribute segments collected.
pub(crate) const K_DATA: u8 = 0;
pub(crate) const K_I30: u8 = 1;
pub(crate) const K_USN: u8 = 2;

#[derive(Clone, Debug)]
pub struct ScanOptions {
    pub threads: usize,
    /// Keep files that are not deleted too (to recover from a damaged volume, or to name
    /// the owner of overwritten clusters).
    pub existing: bool,
    /// Collect which file owns every allocated cluster run (for "overwritten by").
    pub owners: bool,
    /// Also look for names of files whose records are gone: change journal and the slack
    /// of folder indexes. Reads every folder index, so it costs a few seconds.
    pub traces: bool,
    /// Also read all free space for file records outside the MFT (an old MFT after a
    /// quick format). Takes as long as reading the free space.
    pub deep: bool,
    /// Also keep the named data streams (alternate data streams) of the files kept.
    pub streams: bool,
    pub sweep_progress: Option<Arc<fr_core::sweep::SweepProgress>>,
    pub progress: Option<Arc<Progress>>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
            existing: false,
            owners: false,
            traces: false,
            deep: false,
            streams: false,
            sweep_progress: None,
            progress: None,
        }
    }
}

/// A run of clusters owned by a file in use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner {
    pub lcn: u64,
    pub len: u64,
    pub id: u64,
}

#[derive(Debug, Default, Clone)]
pub struct ScanReport {
    pub records: u64,
    pub corrupt: u64,
    pub bad_bytes: u64,
    pub deleted_files: u64,
    pub deleted_dirs: u64,
    /// Items found in the Recycle Bin (deleted or waiting there) with their original path.
    pub recycled: u64,
    /// Deleted entries that got their deletion time from the change journal.
    pub dated: u64,
    /// Names without data, from the change journal and folder indexes.
    pub traces: u64,
    /// File records found in free space by the deep search.
    pub found: u64,
    /// Files found in free space by their signatures.
    pub carved: u64,
    /// The deep search needs clusters at least as large as file records.
    pub deep_skipped: bool,
    pub from_mirror: bool,
    /// `$Bitmap` could not be read: non-resident file states are unknown.
    pub no_bitmap: bool,
    /// The change journal could not be read (absent or damaged).
    pub no_journal: bool,
    pub t_read: Duration,
    pub t_merge: Duration,
    pub t_extras: Duration,
}

pub struct Scan {
    pub catalog: Catalog,
    /// Sorted by `lcn`; empty unless requested.
    pub owners: Vec<Owner>,
    pub report: ScanReport,
}

/// One base record as found.
#[derive(Clone, Copy, Default)]
pub(crate) struct Raw {
    pub id: u64,
    pub seq: u16,
    pub in_use: bool,
    pub dir: bool,
    pub flags: u16,
    pub attrs: u32,
    pub parent: u64,
    pub name_off: u32,
    pub name_len: u16,
    pub times: [u64; 4],
    pub resident: Option<(u16, u16)>,
    /// Size from `$FILE_NAME`, used when the data attribute is lost.
    pub fn_size: u64,
    /// Preference of the name kept (see `ns_rank`).
    pub rank: u8,
}

/// A name found in an extension record: big folders and files with many links keep
/// their long names there, the base record only has the DOS alias.
#[derive(Clone, Copy)]
struct ExtName {
    base: u64,
    base_seq: u16,
    in_use: bool,
    rank: u8,
    parent: u64,
    name_off: u32,
    name_len: u16,
}

/// Runs of one segment of an attribute: unnamed `$DATA`, `$I30` index or `$J` journal.
#[derive(Clone, Copy)]
pub(crate) struct Seg {
    pub kind: u8,
    pub base: u64,
    pub base_seq: u16,
    /// Comes from the base record itself.
    pub own: bool,
    pub in_use: bool,
    pub lowest_vcn: u64,
    pub start: u32,
    pub count: u32,
    /// From the first segment only: size, valid length, compression unit, attribute flags.
    pub head: Option<(u64, u64, u8, u16)>,
}

#[derive(Default)]
pub(crate) struct Worker {
    pub raws: Vec<Raw>,
    pub names: Vec<u8>,
    pub runs: Vec<Run>,
    pub segs: Vec<Seg>,
    pub owners: Vec<Owner>,
    ext_names: Vec<ExtName>,
    pub(crate) ads: Vec<Ads>,
    /// (record, sequence) of base records in use (only with traces).
    pub live: Vec<(u32, u16)>,
    /// (parent record, name key) of every name of a file in use (only with traces).
    pub live_names: Vec<(u64, u64)>,
    pub bitmap: Option<(Vec<Extent>, u64)>,
    pub usn_jrnl: Option<(u64, u16)>,
    pub corrupt: u64,
    pub found: u64,
    pub ext: Vec<Extent>,
}

/// A named data stream of a base record.
#[derive(Clone, Copy)]
pub(crate) struct Ads {
    pub(crate) base: u64,
    seq: u16,
    name_off: u32,
    name_len: u16,
    resident: Option<(u16, u16)>,
    start: u32,
    count: u32,
    size: u64,
    valid: u64,
}

/// What the extra passes need from the merge.
pub(crate) struct Aux {
    pub segs: Vec<Seg>,
    pub runs: Vec<Run>,
    /// Record number -> entry index.
    pub index: Vec<u32>,
    /// Sequence number of every base record in use, 0 if free (only with traces).
    pub live: Vec<u16>,
    /// (parent record, [`name_key`]) of every name in use (only with traces).
    pub live_names: HashSet<(u64, u64)>,
    pub usn_jrnl: Option<(u64, u16)>,
    /// Cluster allocation bitmap, if it could be read.
    pub bitmap: Option<Vec<u8>>,
    /// Named data streams found (with `streams`).
    ads: Vec<Ads>,
}

impl Aux {
    pub fn entry_of(&self, id: u64) -> Option<u32> {
        self.index.get(id as usize).copied().filter(|&i| i != NONE)
    }

    /// Runs of attribute `kind` of record `base` (sequence `seq`, in use or not), joined.
    pub fn join(&self, base: u64, kind: u8, seq: u16, in_use: bool) -> Joined {
        join(&self.segs, &self.runs, base, kind, seq, in_use)
    }
}

pub(crate) struct Joined {
    pub runs: Vec<Run>,
    pub head: Option<(u64, u64, u8, u16)>,
    /// Clusters covered by the segments found.
    pub vcns: u64,
    pub gaps: bool,
}

fn join(segs: &[Seg], runs_in: &[Run], base: u64, kind: u8, seq: u16, in_use: bool) -> Joined {
    let from = segs.partition_point(|g| (g.base, g.kind) < (base, kind));
    let mine = segs[from..].iter().take_while(|g| g.base == base && g.kind == kind).filter(|g| {
        if g.own {
            return true;
        }
        if in_use { g.in_use && g.base_seq == seq } else { !g.in_use && (g.base_seq == seq || g.base_seq.wrapping_add(1) == seq) }
    });
    let mut j = Joined { runs: Vec::new(), head: None, vcns: 0, gaps: false };
    for g in mine {
        if g.lowest_vcn < j.vcns {
            continue; // a second copy of a segment already taken
        }
        if g.lowest_vcn > j.vcns {
            j.runs.push(Run { lcn: Run::MISSING, len: g.lowest_vcn - j.vcns });
            j.gaps = true;
        }
        if g.head.is_some() {
            j.head = g.head;
        }
        let seg_runs = &runs_in[g.start as usize..(g.start + g.count) as usize];
        j.runs.extend_from_slice(seg_runs);
        j.vcns = g.lowest_vcn + seg_runs.iter().map(|x| x.len).sum::<u64>();
    }
    j
}

/// Name preference: Win32 names over POSIX ones, DOS 8.3 aliases last.
fn ns_rank(n: u8) -> u8 {
    match n {
        ns::WIN32 | ns::WIN32_AND_DOS => 3,
        ns::POSIX => 2,
        _ => 1,
    }
}

/// Case-insensitive hash of a name (FNV-1a over its lower-case UTF-8).
pub(crate) fn name_key(name: &str) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for c in name.chars().flat_map(char::to_lowercase) {
        let mut b = [0u8; 4];
        for &x in c.encode_utf8(&mut b).as_bytes() {
            h = (h ^ x as u64).wrapping_mul(0x100_0000_01b3);
        }
    }
    h
}

fn units_eq(raw: &[u8], s: &str) -> bool {
    raw.len() == 2 * s.len() && raw.as_chunks::<2>().0.iter().zip(s.bytes()).all(|(c, b)| c[0] == b && c[1] == 0)
}

/// `$Rxxxxxx...` / `$Ixxxxxx...`: an item of the Recycle Bin and its description.
fn recycle_name(raw: &[u8]) -> bool {
    raw.len() >= 16 && raw[0] == b'$' && raw[1] == 0 && (raw[2] == b'R' || raw[2] == b'I') && raw[3] == 0
}

pub(crate) fn parse(w: &mut Worker, id: u64, rec: &mut [u8], opts: &ScanOptions) {
    if rec.len() < 0x30 || &rec[..4] != b"FILE" {
        if &rec[..4] == b"BAAD" {
            w.corrupt += 1;
        }
        return;
    }
    let hflags = u16le(rec, 0x16);
    let in_use = hflags & REC_IN_USE != 0;
    let dir = hflags & REC_DIRECTORY != 0;
    let base_ref = u64le(rec, 0x20);
    let base = base_ref & 0xFFFF_FFFF_FFFF;
    // Files in use are most of a live volume; only a few of them matter here (Recycle Bin
    // items, the change journal), found by name below.
    let light = in_use && !dir && base == 0 && !opts.existing && !opts.owners && id != BITMAP;
    if !apply_fixup(rec) {
        w.corrupt += 1;
        return;
    }
    let seq = u16le(rec, 0x10);
    if opts.traces && in_use && base == 0 && id <= u32::MAX as u64 {
        w.live.push((id as u32, seq));
    }
    let mut keep_base = base == 0 && (!in_use || dir || opts.existing || id == BITMAP);
    let mut raw = Raw { id, seq, in_use, dir, ..Default::default() };
    let mut best: Option<(u8, usize, usize, u64)> = None;
    let mut name_times = [0u64; 4];
    let mut have_si = false;

    let used = (u32le(rec, 0x18) as usize).min(rec.len());
    let mut off = u16le(rec, 0x14) as usize;
    while off + 8 <= used {
        let ty = u32le(rec, off);
        if ty == attr::END {
            break;
        }
        let len = u32le(rec, off + 4) as usize;
        if len < 0x18 || off + len > used {
            w.corrupt += 1;
            break;
        }
        let a_off = off;
        off += len;
        let a = &rec[a_off..a_off + len];
        let non_res = a[8] != 0;
        let nlen = a[9] as usize;
        let named = nlen != 0;
        let noff = u16le(a, 0x0A) as usize;
        let aname = if noff + 2 * nlen <= len { &a[noff..noff + 2 * nlen] } else { &[][..] };
        let aflags = u16le(a, 0x0C);
        if !non_res {
            let vlen = u32le(a, 0x10) as usize;
            let voff = u16le(a, 0x14) as usize;
            if voff + vlen > len {
                continue;
            }
            let v = &a[voff..voff + vlen];
            match ty {
                attr::STANDARD_INFORMATION if vlen >= 0x24 => {
                    raw.times = [u64le(v, 0), u64le(v, 8), u64le(v, 0x18), u64le(v, 0x10)];
                    raw.attrs = u32le(v, 0x20);
                    have_si = true;
                }
                attr::FILE_NAME if vlen >= 0x42 => {
                    let n = v[0x40] as usize;
                    let nsp = v[0x41];
                    if 0x42 + 2 * n > vlen || n == 0 {
                        continue;
                    }
                    let name = &v[0x42..0x42 + 2 * n];
                    let parent = u64le(v, 0);
                    if opts.traces && in_use && nsp != ns::DOS {
                        let units: Vec<u16> = name.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                        w.live_names.push((parent & 0xFFFF_FFFF_FFFF, name_key(&String::from_utf16_lossy(&units))));
                    }
                    if in_use && nsp != ns::DOS {
                        if units_eq(name, "$UsnJrnl") && parent & 0xFFFF_FFFF_FFFF == EXTEND {
                            w.usn_jrnl = Some((id, seq));
                        } else if light && recycle_name(name) {
                            keep_base = true;
                        }
                    }
                    let rank = ns_rank(nsp);
                    if best.is_none_or(|b| rank > b.0) {
                        best = Some((rank, a_off + voff + 0x42, 2 * n, parent));
                        name_times = [u64le(v, 0x08), u64le(v, 0x10), u64le(v, 0x20), u64le(v, 0x18)];
                        raw.fn_size = u64le(v, 0x30);
                    }
                }
                attr::DATA if named => {
                    raw.flags |= ef::HAS_ADS;
                    if opts.streams && keep_base && base == 0 {
                        let name_off = w.names.len() as u32;
                        push_utf16_as_utf8(&mut w.names, aname);
                        let name_len = (w.names.len() as u32 - name_off) as u16;
                        let resident = Some(((a_off + voff) as u16, vlen.min(u16::MAX as usize) as u16));
                        w.ads.push(Ads {
                            base: id,
                            seq,
                            name_off,
                            name_len,
                            resident,
                            start: 0,
                            count: 0,
                            size: vlen as u64,
                            valid: vlen as u64,
                        });
                    }
                }
                attr::DATA => {
                    raw.resident = Some(((a_off + voff) as u16, vlen.min(u16::MAX as usize) as u16));
                    raw.flags |= ef::RESIDENT;
                    raw.fn_size = vlen as u64;
                }
                _ => {}
            }
            continue;
        }
        if len < 0x40 {
            continue;
        }
        let mp = u16le(a, 0x20) as usize;
        if mp >= len {
            continue;
        }
        let lowest = u64le(a, 0x10);
        if ty == attr::DATA && named {
            raw.flags |= ef::HAS_ADS;
        }
        let kind = match ty {
            attr::DATA if !named && (keep_base || (base != 0 && (!in_use || opts.existing))) => Some(K_DATA),
            attr::DATA if in_use && units_eq(aname, "$J") => Some(K_USN),
            attr::INDEX_ALLOCATION if opts.traces && (dir || base != 0) && units_eq(aname, "$I30") => Some(K_I30),
            _ => None,
        };
        let want_owner = opts.owners && in_use;
        let is_bitmap = id == BITMAP && ty == attr::DATA && !named;
        let want_ads = opts.streams && ty == attr::DATA && named && keep_base && base == 0 && lowest == 0;
        if kind.is_none() && !want_owner && !is_bitmap && !want_ads {
            continue;
        }
        w.ext.clear();
        if runlist::decode(&a[mp..], lowest, &mut w.ext).is_err() {
            continue;
        }
        if want_owner {
            let owner = if base == 0 { id } else { base };
            w.owners.extend(w.ext.iter().filter_map(|e| e.lcn.map(|lcn| Owner { lcn, len: e.len, id: owner })));
        }
        if is_bitmap && lowest == 0 {
            w.bitmap = Some((w.ext.clone(), u64le(a, 0x30)));
        }
        if want_ads {
            let name_off = w.names.len() as u32;
            push_utf16_as_utf8(&mut w.names, aname);
            let name_len = (w.names.len() as u32 - name_off) as u16;
            let start = w.runs.len() as u32;
            w.runs.extend(w.ext.iter().map(|e| Run { lcn: e.lcn.unwrap_or(Run::SPARSE), len: e.len }));
            w.ads.push(Ads {
                base: id,
                seq,
                name_off,
                name_len,
                resident: None,
                start,
                count: w.ext.len() as u32,
                size: u64le(a, 0x30),
                valid: u64le(a, 0x38),
            });
        }
        if let Some(kind) = kind {
            let start = w.runs.len() as u32;
            w.runs.extend(w.ext.iter().map(|e| Run { lcn: e.lcn.unwrap_or(Run::SPARSE), len: e.len }));
            let head = (lowest == 0).then(|| {
                let cu = if aflags & AF_COMPRESSED != 0 { a[0x22] } else { 0 };
                (u64le(a, 0x30), u64le(a, 0x38), cu, aflags)
            });
            let (sb, sseq) = if base == 0 { (id, seq) } else { (base, (base_ref >> 48) as u16) };
            w.segs.push(Seg {
                kind,
                base: sb,
                base_seq: sseq,
                own: base == 0,
                in_use,
                lowest_vcn: lowest,
                start,
                count: w.ext.len() as u32,
                head,
            });
        }
    }

    if base != 0 {
        if let Some((rank, at, nbytes, parent)) = best {
            let name_off = w.names.len() as u32;
            push_utf16_as_utf8(&mut w.names, &rec[at..at + nbytes]);
            let name_len = (w.names.len() as u32 - name_off) as u16;
            w.ext_names.push(ExtName { base, base_seq: (base_ref >> 48) as u16, in_use, rank, parent, name_off, name_len });
        }
        return;
    }
    if !keep_base {
        return;
    }
    let Some((rank, at, nbytes, parent)) = best else { return };
    raw.rank = rank;
    if !have_si {
        raw.times = name_times;
    }
    raw.parent = parent;
    raw.name_off = w.names.len() as u32;
    push_utf16_as_utf8(&mut w.names, &rec[at..at + nbytes]);
    raw.name_len = (w.names.len() as u32 - raw.name_off) as u16;
    if !in_use {
        w.found += 1;
    }
    w.raws.push(raw);
}

/// Allocation bitmap of the volume: bit n set = cluster n in use.
fn read_bitmap(vol: &Volume, runs: &[Extent], size: u64) -> Option<Vec<u8>> {
    let cs = vol.boot.cluster_size as u64;
    let total = size.div_ceil(cs).checked_mul(cs)?;
    if size < vol.clusters().div_ceil(8) || total > vol.src.size {
        return None;
    }
    let mut out = Vec::new();
    out.try_reserve_exact(total as usize).ok()?;
    out.resize(total as usize, 0);
    let mut ordered: Vec<_> = runs.iter().collect();
    ordered.sort_unstable_by_key(|e| e.vcn);
    let mut next = 0u64;
    let mut buf = vol.src.buffer(4 << 20);
    let mut slot = ReadSlot::new().ok()?;
    for e in ordered {
        if next == total / cs {
            break;
        }
        if e.vcn != next {
            return None;
        }
        let lcn = e.lcn?;
        let mut k = 0u64;
        let clusters = e.len.min(total / cs - next);
        let bytes = clusters * cs;
        while k < bytes {
            let n = (bytes - k).min(buf.len() as u64) as usize;
            let disk = vol.base.checked_add(lcn.checked_mul(cs)?)?.checked_add(k)?;
            if vol.src.read_robust(&mut slot, disk, &mut buf[..n]) != 0 {
                return None;
            }
            let at = (e.vcn * cs + k) as usize;
            out[at..at + n].copy_from_slice(&buf[..n]);
            k += n as u64;
        }
        next += clusters;
    }
    if next != total / cs {
        return None;
    }
    out.truncate(size as usize);
    Some(out)
}

/// Set bits of `bm` in `lcn..lcn+len`; clusters beyond the bitmap count as set.
pub(crate) fn count_set(bm: &[u8], lcn: u64, len: u64) -> u64 {
    let total = bm.len() as u64 * 8;
    let end = (lcn + len).min(total);
    let mut n = (lcn + len).saturating_sub(total.max(lcn));
    let mut i = lcn;
    while i < end && !i.is_multiple_of(8) {
        n += (bm[(i / 8) as usize] >> (i % 8) & 1) as u64;
        i += 1;
    }
    while i + 64 <= end {
        let b = (i / 8) as usize;
        n += u64::from_le_bytes(bm[b..b + 8].try_into().unwrap()).count_ones() as u64;
        i += 64;
    }
    while i < end {
        n += (bm[(i / 8) as usize] >> (i % 8) & 1) as u64;
        i += 1;
    }
    n
}

/// Scan the NTFS volume at `base` of `src`.
pub fn scan(src: &Source, base: u64, opts: &ScanOptions) -> Result<Scan> {
    let vol = Volume::open(src, base)?;
    scan_volume(vol, opts)
}

/// Reconstruct a volume using a validated surviving boot-sector copy.
pub fn scan_with_boot(src: &Source, base: u64, boot: crate::boot::BootSector, opts: &ScanOptions) -> Result<Scan> {
    scan_volume(Volume { src, base, boot }, opts)
}

fn scan_volume(vol: Volume<'_>, opts: &ScanOptions) -> Result<Scan> {
    let t0 = Instant::now();
    let layout = mft::load_layout(&vol)?;
    let progress = opts.progress.as_deref();
    let (workers, bad_bytes) = mft::read_records(&vol, &layout, opts.threads, progress, Worker::default, |w: &mut Worker, id, rec| {
        let before = w.found;
        parse(w, id, rec, opts);
        if w.found != before
            && let Some(p) = progress
        {
            p.found.fetch_add(1, Ordering::Relaxed);
        }
    })?;
    let t_read = t0.elapsed();
    let t1 = Instant::now();
    let mut report = ScanReport { records: layout.records(), bad_bytes, from_mirror: layout.from_mirror, t_read, ..Default::default() };
    let (mut s, aux) = merge(&vol, &layout, workers, &mut report, opts.traces);
    if opts.owners {
        s.owners.sort_unstable_by_key(|o| o.lcn);
    }
    report.t_merge = t1.elapsed();
    let t2 = Instant::now();
    crate::extras::run(&vol, &mut s.catalog, &aux, opts, &mut report);
    if opts.deep {
        crate::deep::run(&vol, &mut s.catalog, &aux, opts, &mut report);
    }
    add_streams(&mut s.catalog, &aux, vol.boot.cluster_size as u64);
    report.t_extras = t2.elapsed();
    for e in &s.catalog.entries {
        if e.is_deleted() && e.flags & ef::TRACE == 0 {
            if e.is_dir() {
                report.deleted_dirs += 1;
            } else {
                report.deleted_files += 1;
            }
        }
    }
    s.report = report;
    Ok(s)
}

fn merge(vol: &Volume, layout: &Layout, workers: Vec<Worker>, report: &mut ScanReport, traces: bool) -> (Scan, Aux) {
    let cs = vol.boot.cluster_size as u64;
    let clusters = vol.clusters();
    let records = layout.records() as usize;
    // Concatenate worker outputs.
    let mut raws = Vec::new();
    let mut names = Vec::new();
    let mut runs_in = Vec::new();
    let mut segs = Vec::new();
    let mut owners = Vec::new();
    let mut ext_names = Vec::new();
    let mut ads = Vec::new();
    let mut bitmap = None;
    let mut usn_jrnl = None;
    let mut live = if traces { vec![0u16; records] } else { Vec::new() };
    let mut live_names = HashSet::new();
    for w in workers {
        report.corrupt += w.corrupt;
        let (nb, rb) = (names.len() as u32, runs_in.len() as u32);
        raws.extend(w.raws.into_iter().map(|r| Raw { name_off: r.name_off + nb, ..r }));
        segs.extend(w.segs.into_iter().map(|g| Seg { start: g.start + rb, ..g }));
        names.extend_from_slice(&w.names);
        runs_in.extend_from_slice(&w.runs);
        owners.extend_from_slice(&w.owners);
        ext_names.extend(w.ext_names.into_iter().map(|x| ExtName { name_off: x.name_off + nb, ..x }));
        ads.extend(w.ads.into_iter().map(|x| Ads { name_off: x.name_off + nb, start: x.start + rb, ..x }));
        live_names.extend(w.live_names);
        for (id, seq) in w.live {
            if let Some(x) = live.get_mut(id as usize) {
                *x = seq.max(1);
            }
        }
        if w.bitmap.is_some() {
            bitmap = w.bitmap;
        }
        if w.usn_jrnl.is_some() {
            usn_jrnl = w.usn_jrnl;
        }
    }
    raws.sort_unstable_by_key(|r| r.id);
    segs.sort_unstable_by_key(|g| (g.base, g.kind, g.lowest_vcn, !g.own));
    ext_names.sort_unstable_by_key(|x| x.base);
    for r in raws.iter_mut() {
        let from = ext_names.partition_point(|x| x.base < r.id);
        for x in ext_names[from..].iter().take_while(|x| x.base == r.id) {
            let same = if r.in_use {
                x.in_use && x.base_seq == r.seq
            } else {
                !x.in_use && (x.base_seq == r.seq || x.base_seq.wrapping_add(1) == r.seq)
            };
            if same && x.rank > r.rank {
                r.rank = x.rank;
                r.name_off = x.name_off;
                r.name_len = x.name_len;
                r.parent = x.parent;
            }
        }
    }

    let mut entries: Vec<Entry> = Vec::with_capacity(raws.len());
    let mut runs: Vec<Run> = Vec::new();
    let mut parents: Vec<u64> = Vec::with_capacity(raws.len());
    for r in &raws {
        let mut e = Entry {
            id: r.id,
            seq: r.seq,
            flags: r.flags,
            attrs: r.attrs,
            parent: NONE,
            name_off: r.name_off,
            name_len: r.name_len,
            created: r.times[0],
            modified: r.times[1],
            accessed: r.times[2],
            changed: r.times[3],
            ..Default::default()
        };
        if !r.in_use {
            e.flags |= ef::DELETED;
        }
        if r.dir {
            e.flags |= ef::DIR;
        }
        if r.attrs & FILE_ATTRIBUTE_ENCRYPTED != 0 {
            e.flags |= ef::ENCRYPTED;
        }
        if !r.dir {
            build_data(&mut e, r, &segs, &runs_in, &mut runs, cs, clusters);
        }
        parents.push(r.parent);
        entries.push(e);
    }

    // Parents.
    let mut index = vec![NONE; records.max(entries.last().map_or(0, |e| e.id as usize + 1))];
    for (i, e) in entries.iter().enumerate() {
        index[e.id as usize] = i as u32;
    }
    for i in 0..entries.len() {
        if entries[i].id == ROOT {
            continue;
        }
        match resolve_parent(&entries, &index, parents[i]) {
            Some(j) if j as usize != i => entries[i].parent = j,
            _ => entries[i].flags |= ef::ORPHAN,
        }
    }

    // States.
    let bm = bitmap.and_then(|(ext, size)| read_bitmap(vol, &ext, size));
    report.no_bitmap = bm.is_none();
    let fs = FsInfo {
        kind: FsKind::Ntfs,
        offset: vol.base,
        size: vol.boot.volume_bytes(),
        cluster_size: vol.boot.cluster_size,
        record_size: layout.record_size,
        mft: layout.extents.iter().map(|e| Run { lcn: e.lcn.unwrap_or(Run::SPARSE), len: e.len }).collect(),
        serial: vol.boot.serial,
        root: ROOT,
        data_offset: 0,
    };
    let mut cat = Catalog::new(fs, entries, names, runs);
    cat.classify_ntfs_system();
    assign_states(&mut cat, bm.as_deref());
    let aux = Aux { segs, runs: runs_in, index, live, live_names, usn_jrnl, bitmap: bm, ads };
    (Scan { catalog: cat, owners, report: ScanReport::default() }, aux)
}

/// Entry index of the folder a (record, sequence) reference points to, if that folder is
/// still the same one: in use with the same sequence, or deleted with the next one.
pub(crate) fn resolve_parent(entries: &[Entry], index: &[u32], pref: u64) -> Option<u32> {
    let (pr, ps) = (pref & 0xFFFF_FFFF_FFFF, (pref >> 48) as u16);
    let j = index.get(pr as usize).copied().filter(|&j| j != NONE)?;
    let p = &entries[j as usize];
    (p.is_dir() && (p.seq == ps || (p.is_deleted() && p.seq == ps.wrapping_add(1)))).then_some(j)
}

/// Join the data segments of base record `r` into `e.data`.
pub(crate) fn build_data(e: &mut Entry, r: &Raw, segs: &[Seg], runs_in: &[Run], runs: &mut Vec<Run>, cs: u64, clusters: u64) {
    if let Some((off, len)) = r.resident {
        e.data = Data::Resident { off, len };
        e.size = len as u64;
        e.valid = len as u64;
        return;
    }
    let j = join(segs, runs_in, r.id, K_DATA, r.seq, r.in_use);
    let start = runs.len();
    runs.extend_from_slice(&j.runs);
    if j.gaps {
        e.flags |= ef::INCOMPLETE;
    }
    match j.head {
        Some((size, valid, cu, aflags)) => {
            e.size = size;
            e.valid = valid.min(size);
            if aflags & AF_COMPRESSED != 0 && (1..=8).contains(&cu) {
                e.cu = cu;
                e.flags |= ef::COMPRESSED;
            }
            if aflags & AF_SPARSE != 0 {
                e.flags |= ef::SPARSE;
            }
            if aflags & AF_ENCRYPTED != 0 {
                e.flags |= ef::ENCRYPTED;
            }
        }
        None => {
            e.size = r.fn_size;
            e.valid = r.fn_size;
            if runs.len() > start {
                e.flags |= ef::INCOMPLETE;
            }
        }
    }
    let unit = if e.cu != 0 { 1u64 << e.cu } else { 1 };
    let need = e.size.div_ceil(cs).div_ceil(unit) * unit;
    if runs.len() > start && j.vcns < need {
        runs.push(Run { lcn: Run::MISSING, len: need - j.vcns });
        e.flags |= ef::INCOMPLETE;
    }
    // Runs past the end of the volume mean a damaged record: its location is worthless.
    if runs[start..].iter().any(|x| x.is_real() && x.lcn.saturating_add(x.len) > clusters) {
        runs.truncate(start);
    }
    if runs.len() > start {
        e.data = Data::Runs { start: start as u32, count: (runs.len() - start) as u32 };
    }
}

/// State of every deleted file from the allocation bitmap and from newer deleted files
/// that claim the same clusters.
fn assign_states(cat: &mut Catalog, bm: Option<&[u8]>) {
    // Clusters now allocated to something else.
    for i in 0..cat.entries.len() {
        let e = cat.entries[i];
        if !e.is_deleted() || e.is_dir() {
            continue;
        }
        if let Some(bm) = bm {
            let lost: u64 = cat.runs_of(i as u32).iter().filter(|r| r.is_real()).map(|r| count_set(bm, r.lcn, r.len)).sum();
            cat.entries[i].lost = lost;
        }
    }
    // Two deleted files on the same free clusters: the older one lost them to the newer.
    let mut iv: Vec<(u64, u64, u32)> = Vec::new();
    for (i, e) in cat.entries.iter().enumerate() {
        if e.is_deleted() && !e.is_dir() {
            iv.extend(cat.runs_of(i as u32).iter().filter(|r| r.is_real()).map(|r| (r.lcn, r.lcn + r.len, i as u32)));
        }
    }
    iv.sort_unstable();
    let mut active: Vec<(u64, u64, u32)> = Vec::new();
    let mut extra = vec![0u64; cat.entries.len()];
    for &(s, end, i) in &iv {
        active.retain(|a| a.1 > s);
        for &(_, ae, j) in &active {
            if j == i {
                continue;
            }
            let overlap = ae.min(end) - s;
            let (ei, ej) = (&cat.entries[i as usize], &cat.entries[j as usize]);
            let loser = if ei.newest_time() >= ej.newest_time() { j } else { i };
            extra[loser as usize] += overlap;
        }
        // Many files on one range would make this quadratic; the first few decide it.
        if active.len() < 64 {
            active.push((s, end, i));
        }
    }
    for (i, &more) in extra.iter().enumerate() {
        let e = cat.entries[i];
        let real = cat.real_clusters(i as u32);
        let lost = (e.lost + more).min(real);
        let state = if !e.is_deleted() || e.is_dir() {
            State::Good
        } else if e.flags & ef::ENCRYPTED != 0 {
            State::Encrypted
        } else if e.size == 0 || matches!(e.data, Data::Resident { .. }) {
            State::Good
        } else if e.data == Data::None {
            State::NoData
        } else if real > 0 && lost >= real {
            State::Overwritten
        } else if lost > 0 || e.flags & ef::INCOMPLETE != 0 {
            State::Partial
        } else if bm.is_none() {
            State::Unknown
        } else {
            State::Good
        };
        cat.entries[i].lost = lost;
        cat.entries[i].state = state;
    }
}

/// Named data streams as entries `file:stream` next to their files. Appended last: their
/// ids are above every other kind.
fn add_streams(cat: &mut Catalog, aux: &Aux, cs: u64) {
    append_streams(cat, &aux.ads, &aux.runs, aux.bitmap.as_deref(), &|id| aux.entry_of(id), cs);
}

pub(crate) fn append_streams(
    cat: &mut Catalog,
    ads: &[Ads],
    input: &[Run],
    bitmap: Option<&[u8]>,
    lookup: &impl Fn(u64) -> Option<u32>,
    cs: u64,
) {
    for (k, a) in ads.iter().enumerate() {
        let Some(base) = lookup(a.base) else { continue };
        let f = cat.entries[base as usize];
        if f.seq != a.seq {
            continue;
        }
        let stream = String::from_utf8_lossy(&cat.names[a.name_off as usize..a.name_off as usize + a.name_len as usize]).into_owned();
        let name = format!("{}:{stream}", cat.name(base));
        let (name_off, name_len) = cat.add_name(&name);
        let mut e = Entry {
            id: fr_core::model::STREAM_ID_BASE + k as u64,
            seq: a.seq,
            flags: (f.flags & (ef::DELETED | ef::RECYCLED | ef::ORPHAN | ef::SYSTEM | ef::FOUND)) | ef::STREAM,
            parent: f.parent,
            name_off,
            name_len,
            size: a.size,
            valid: a.valid.min(a.size),
            created: f.created,
            modified: f.modified,
            accessed: f.accessed,
            changed: f.changed,
            deleted_at: f.deleted_at,
            ..Default::default()
        };
        match a.resident {
            Some((off, len)) => e.data = Data::Resident { off, len },
            None => {
                let start = cat.runs.len() as u32;
                let runs = &input[a.start as usize..(a.start + a.count) as usize];
                cat.runs.extend_from_slice(runs);
                let have: u64 = runs.iter().map(|r| r.len).sum();
                let need = a.size.div_ceil(cs);
                if have < need {
                    cat.runs.push(Run { lcn: Run::MISSING, len: need - have });
                    e.flags |= ef::INCOMPLETE;
                }
                e.data = Data::Runs { start, count: cat.runs.len() as u32 - start };
                let real: u64 = runs.iter().filter(|r| r.is_real()).map(|r| r.len).sum();
                let lost: u64 = bitmap.map_or(0, |bm| runs.iter().filter(|r| r.is_real()).map(|r| count_set(bm, r.lcn, r.len)).sum());
                e.lost = lost;
                e.state = if !f.is_deleted() {
                    State::Good
                } else if real > 0 && lost >= real {
                    State::Overwritten
                } else if lost > 0 || e.flags & ef::INCOMPLETE != 0 {
                    State::Partial
                } else if bitmap.is_none() {
                    State::Unknown
                } else {
                    State::Good
                };
            }
        }
        cat.entries.push(e);
    }
}

/// Files in use that own clusters of `lcn..lcn+len`.
pub fn owners_of(owners: &[Owner], lcn: u64, len: u64) -> Vec<u64> {
    let end = lcn + len;
    let hi = owners.partition_point(|o| o.lcn < end);
    let mut out: Vec<u64> = owners[..hi].iter().rev().take(4096).filter(|o| o.lcn + o.len > lcn).map(|o| o.id).collect();
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::synth::RecordBuilder;

    fn opts() -> ScanOptions {
        ScanOptions { threads: 1, ..Default::default() }
    }

    #[test]
    fn bit_counting() {
        let mut bm = vec![0u8; 32];
        bm[1] = 0b1111_0000; // clusters 12..16
        bm[10] = 0xFF; // 80..88
        assert_eq!(count_set(&bm, 0, 256), 12);
        assert_eq!(count_set(&bm, 13, 2), 2);
        assert_eq!(count_set(&bm, 250, 10), 4, "past the end counts as used");
        assert_eq!(count_set(&bm, 16, 64), 0);
        assert_eq!(count_set(&bm, 24, 64), 8, "word-sized step over 80..88");
        assert_eq!(count_set(&bm, 3, 200), 12);
    }

    #[test]
    fn missing_allocation_bitmap_does_not_mean_good() {
        let fs = FsInfo {
            kind: fr_core::FsKind::Ntfs,
            offset: 0,
            size: 8192,
            cluster_size: 512,
            record_size: 1024,
            mft: vec![],
            serial: 0,
            root: 5,
            data_offset: 0,
        };
        let file =
            Entry { id: 40, flags: ef::DELETED, size: 512, valid: 512, data: Data::Runs { start: 0, count: 1 }, ..Default::default() };
        let resident =
            Entry { id: 41, flags: ef::DELETED, size: 16, valid: 16, data: Data::Resident { off: 64, len: 16 }, ..Default::default() };
        let mut cat = Catalog::new(fs, vec![file, resident], vec![], vec![Run { lcn: 4, len: 1 }]);
        assign_states(&mut cat, None);
        assert_eq!(cat.entries[0].state, State::Unknown);
        assert_eq!(cat.entries[1].state, State::Good);
        assign_states(&mut cat, Some(&[0]));
        assert_eq!(cat.entries[0].state, State::Good);
    }

    #[test]
    fn deleted_record_becomes_an_entry() {
        let runs = [Extent { vcn: 0, len: 3, lcn: Some(1000) }];
        let mut raw = RecordBuilder::new(1024)
            .unused()
            .seq(8)
            .std_info(132_000_000_000_000_000, 0x20)
            .file_name(40, 2, "holiday.jpg", ns::WIN32)
            .file_name(40, 2, "HOLIDA~1.JPG", ns::DOS)
            .non_resident(attr::DATA, "", 0, 0, 3 * 4096, 10_000, None, &runs)
            .build(7);
        let mut w = Worker::default();
        parse(&mut w, 77, &mut raw, &opts());
        assert_eq!(w.raws.len(), 1);
        let r = w.raws[0];
        assert!(!r.in_use);
        assert_eq!(&w.names[r.name_off as usize..(r.name_off + r.name_len as u32) as usize], b"holiday.jpg");
        assert_eq!(r.parent & 0xFFFF_FFFF_FFFF, 40);
        assert_eq!(w.segs.len(), 1);
        assert_eq!(w.segs[0].head.unwrap().0, 10_000);
    }

    #[test]
    fn files_in_use_are_skipped_unless_asked() {
        let raw = RecordBuilder::new(1024).std_info(0, 0).file_name(5, 5, "live.txt", ns::WIN32).build(1);
        let mut w = Worker::default();
        parse(&mut w, 30, &mut raw.clone(), &opts());
        assert!(w.raws.is_empty());
        parse(&mut w, 30, &mut raw.clone(), &ScanOptions { existing: true, ..opts() });
        assert_eq!(w.raws.len(), 1);
    }

    #[test]
    fn recycle_bin_items_in_use_are_kept() {
        let mut w = Worker::default();
        let raw = RecordBuilder::new(1024).std_info(0, 0).file_name(70, 1, "$RXK3P2A.jpg", ns::WIN32).build(1);
        parse(&mut w, 31, &mut raw.clone(), &opts());
        assert_eq!(w.raws.len(), 1);
        let raw = RecordBuilder::new(1024).std_info(0, 0).file_name(EXTEND as u32, 11, "$UsnJrnl", ns::WIN32).build(1);
        parse(&mut w, 64, &mut raw.clone(), &opts());
        assert_eq!(w.usn_jrnl, Some((64, 1)));
    }

    fn raw(id: u64, seq: u16, in_use: bool, dir: bool, parent: u64, pseq: u16) -> Raw {
        Raw { id, seq, in_use, dir, parent: parent | (pseq as u64) << 48, ..Default::default() }
    }

    fn seg(base: u64, base_seq: u16, own: bool, lowest_vcn: u64, start: u32, head: Option<(u64, u64, u8, u16)>) -> Seg {
        Seg { kind: K_DATA, base, base_seq, own, in_use: false, lowest_vcn, start, count: 1, head }
    }

    #[test]
    fn extension_segments_are_joined_by_sequence() {
        let runs_in = vec![Run { lcn: 100, len: 4 }, Run { lcn: 900, len: 4 }, Run { lcn: 555, len: 4 }];
        let mut segs = vec![
            seg(50, 3, true, 0, 0, Some((32768, 32768, 0, 0))),
            // The record's previous life (sequence 1): must not be used.
            seg(50, 1, false, 4, 2, None),
            seg(50, 3, false, 4, 1, None),
        ];
        segs.sort_unstable_by_key(|g| (g.base, g.kind, g.lowest_vcn, !g.own));
        let r = raw(50, 4, false, false, 5, 5);
        let mut e = Entry::default();
        let mut runs = Vec::new();
        build_data(&mut e, &r, &segs, &runs_in, &mut runs, 4096, 1 << 20);
        assert_eq!(runs, vec![Run { lcn: 100, len: 4 }, Run { lcn: 900, len: 4 }]);
        assert_eq!(e.size, 32768);
        assert_eq!(e.flags & ef::INCOMPLETE, 0);
    }

    #[test]
    fn missing_tail_is_marked() {
        let runs_in = vec![Run { lcn: 100, len: 4 }];
        let segs = vec![seg(50, 3, true, 0, 0, Some((10 * 4096, 10 * 4096, 0, 0)))];
        let mut e = Entry::default();
        let mut runs = Vec::new();
        build_data(&mut e, &raw(50, 4, false, false, 5, 5), &segs, &runs_in, &mut runs, 4096, 1 << 20);
        assert_eq!(runs.last(), Some(&Run { lcn: Run::MISSING, len: 6 }));
        assert!(e.flags & ef::INCOMPLETE != 0);
    }

    #[test]
    fn owners_lookup() {
        let mut o = vec![Owner { lcn: 0, len: 100, id: 1 }, Owner { lcn: 150, len: 10, id: 2 }, Owner { lcn: 200, len: 5, id: 3 }];
        o.sort_unstable_by_key(|x| x.lcn);
        assert_eq!(owners_of(&o, 50, 120), vec![1, 2]);
        assert_eq!(owners_of(&o, 100, 50), Vec::<u64>::new());
    }
}
