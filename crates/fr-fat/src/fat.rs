//! FAT12, FAT16 and FAT32.
//!
//! Deleting a file replaces the first byte of its directory entry with `0xE5` and frees
//! its cluster chain in the FAT; the rest of the entry (name, size, first cluster, dates)
//! stays. Long names live in LFN entries before it, also marked `0xE5`, still in reverse
//! order. The lost first character of the short name comes back through the LFN checksum,
//! which covers all 11 bytes of the short name.
//!
//! Since the chain is gone, a deleted file is assumed to lie in consecutive clusters from
//! its first one, as FAT writes files when the disk is not fragmented. A deleted folder
//! is read the same way, accepted only when its first cluster still starts with its own
//! `.` entry.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use fr_core::carve::{Carver, carve_into};
use fr_core::model::{Builder, Entry, FOUND_ID_BASE, FsInfo, NONE, Run, State, ef};
use fr_core::part::FsKind;
use fr_core::source::Source;
use fr_core::sweep::{SweepProgress, sweep};
use fr_core::time::from_dos;

use crate::io::{Vol, runs_of};
use crate::{FatReport, FatScan, ScanOptions};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Boot {
    pub bits: u8,
    pub bps: u32,
    pub cluster_size: u32,
    pub fat_off: u64,
    pub fat_bytes: u64,
    pub nfats: u32,
    /// FAT12/16: fixed root directory area.
    pub root_off: u64,
    pub root_bytes: u64,
    pub data_off: u64,
    /// Clusters in the data area (numbered from 2 on disk).
    pub clusters: u64,
    pub root_cluster: u32,
    pub total_bytes: u64,
    pub serial: u32,
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

pub fn parse_boot(b: &[u8]) -> Option<Boot> {
    if b.len() < 512 {
        return None;
    }
    let bps = u16le(b, 0x0B) as u32;
    let spc = b[0x0D] as u32;
    let reserved = u16le(b, 0x0E) as u64;
    let nfats = b[0x10] as u32;
    let root_entries = u16le(b, 0x11) as u64;
    let total = if u16le(b, 0x13) != 0 { u16le(b, 0x13) as u64 } else { u32le(b, 0x20) as u64 };
    let fatsz = if u16le(b, 0x16) != 0 { u16le(b, 0x16) as u64 } else { u32le(b, 0x24) as u64 };
    if !bps.is_power_of_two() || !(512..=4096).contains(&bps) || !spc.is_power_of_two() || reserved == 0 || !(1..=4).contains(&nfats) {
        return None;
    }
    let b64 = bps as u64;
    let root_secs = (root_entries * 32).div_ceil(b64);
    let data_sec = reserved + nfats as u64 * fatsz + root_secs;
    if fatsz == 0 || total <= data_sec {
        return None;
    }
    let clusters = (total - data_sec) / spc as u64;
    let bits = if clusters < 4085 {
        12
    } else if clusters < 65525 {
        16
    } else {
        32
    };
    let needed = ((clusters + 2) * bits as u64).div_ceil(8);
    let root_cluster = if bits == 32 { u32le(b, 0x2C) } else { 0 };
    if fatsz * b64 < needed || bits == 32 && !(2..clusters + 2).contains(&(root_cluster as u64)) {
        return None;
    }
    let serial = if bits == 32 { u32le(b, 0x43) } else { u32le(b, 0x27) };
    Some(Boot {
        bits,
        bps,
        cluster_size: bps * spc,
        fat_off: reserved * b64,
        fat_bytes: fatsz * b64,
        nfats,
        root_off: (reserved + nfats as u64 * fatsz) * b64,
        root_bytes: root_secs * b64,
        data_off: data_sec * b64,
        clusters,
        root_cluster,
        total_bytes: total * b64,
        serial,
    })
}

/// The file allocation table.
pub struct Table {
    bits: u8,
    data: Vec<u8>,
    clusters: u64,
}

impl Table {
    /// Next cluster of `n` (0 = free).
    pub fn get(&self, n: u64) -> u32 {
        let d = &self.data;
        match self.bits {
            12 => {
                let i = (n + n / 2) as usize;
                if i + 1 >= d.len() {
                    return u32::MAX;
                }
                let v = u16le(d, i) as u32;
                if n & 1 == 1 { v >> 4 } else { v & 0xFFF }
            }
            16 => d.get(2 * n as usize..2 * n as usize + 2).map_or(u32::MAX, |b| u16le(b, 0) as u32),
            _ => d.get(4 * n as usize..4 * n as usize + 4).map_or(u32::MAX, |b| u32le(b, 0) & 0x0FFF_FFFF),
        }
    }

    pub fn is_free(&self, n: u64) -> bool {
        self.get(n) == 0
    }

    /// Clusters of the chain from `first` (on-disk numbers), at most `max`.
    pub fn chain(&self, first: u64, max: u64) -> Vec<u64> {
        let mut out = Vec::new();
        let mut c = first;
        let last = self.clusters + 1;
        while (2..=last).contains(&c) && (out.len() as u64) < max {
            out.push(c);
            let next = self.get(c) as u64;
            let eoc = match self.bits {
                12 => next >= 0xFF7,
                16 => next >= 0xFFF7,
                _ => next >= 0x0FFF_FFF7,
            };
            if eoc || next < 2 {
                break;
            }
            c = next;
        }
        out
    }
}

/// LFN checksum of an 11-byte short name.
pub fn lfn_checksum(sfn: &[u8]) -> u8 {
    sfn.iter().fold(0u8, |s, &c| ((s & 1) << 7).wrapping_add(s >> 1).wrapping_add(c))
}

/// OEM code page bytes to text.
fn oem(b: &[u8]) -> String {
    if b.is_ascii() {
        return String::from_utf8_lossy(b).into_owned();
    }
    let mut w = [0u16; 16];
    // SAFETY: input and output buffers with their lengths.
    let n = unsafe {
        windows_sys::Win32::Globalization::MultiByteToWideChar(
            windows_sys::Win32::Globalization::CP_OEMCP,
            0,
            b.as_ptr(),
            b.len() as i32,
            w.as_mut_ptr(),
            w.len() as i32,
        )
    };
    String::from_utf16_lossy(&w[..n.max(0) as usize])
}

/// `NAME    EXT` to `name.ext`, honouring the lower-case flags of Windows NT.
fn short_name(sfn: &[u8], nt: u8) -> String {
    let mut base = sfn[..8].to_vec();
    if base[0] == 0x05 {
        base[0] = 0xE5;
    }
    let base = oem(base.trim_ascii_end());
    let ext = oem(sfn[8..11].trim_ascii_end());
    let base = if nt & 0x08 != 0 { base.to_lowercase() } else { base };
    let ext = if nt & 0x10 != 0 { ext.to_lowercase() } else { ext };
    if ext.is_empty() { base } else { format!("{base}.{ext}") }
}

/// One parsed directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// Byte offset of the short entry inside the directory data.
    pub at: usize,
    pub name: String,
    pub deleted: bool,
    pub dir: bool,
    pub attrs: u8,
    pub first: u64,
    pub size: u64,
    pub created: u64,
    pub modified: u64,
    pub accessed: u64,
}

/// Directory entries of `d`. `fat32`: the high word of the first cluster is valid.
pub fn parse_dir(d: &[u8], fat32: bool) -> Vec<DirEntry> {
    let mut out = Vec::new();
    // LFN entries seen since the last short entry, in physical order.
    let mut lfn: Vec<&[u8]> = Vec::new();
    for (k, e) in d.as_chunks::<32>().0.iter().enumerate() {
        let b0 = e[0];
        let attr = e[11];
        if b0 == 0 {
            // End of the directory, unless deleted entries follow (rare, but cheap to see).
            lfn.clear();
            continue;
        }
        if attr & 0x3F == 0x0F {
            lfn.push(e);
            continue;
        }
        let pending = std::mem::take(&mut lfn);
        if b0 == b'.' || attr & 0x08 != 0 {
            continue;
        }
        // Reject entries that are not plausible (random data in a reused cluster).
        if attr & 0xC0 != 0 || e[1..11].iter().any(|&c| c < 0x20 && c != 0x05) {
            continue;
        }
        let deleted = b0 == 0xE5;
        let mut sfn = [0u8; 11];
        sfn.copy_from_slice(&e[..11]);
        let long = long_name(&pending, &mut sfn, deleted);
        let name = match long {
            Some(n) => n,
            None => {
                if deleted {
                    sfn[0] = b'_';
                }
                short_name(&sfn, e[12])
            }
        };
        if name.is_empty() {
            continue;
        }
        let hi = if fat32 { u16le(e, 20) as u64 } else { 0 };
        out.push(DirEntry {
            at: k * 32,
            name,
            deleted,
            dir: attr & 0x10 != 0,
            attrs: attr,
            first: hi << 16 | u16le(e, 26) as u64,
            size: u32le(e, 28) as u64,
            created: from_dos(u16le(e, 16), u16le(e, 14), e[13], None),
            modified: from_dos(u16le(e, 24), u16le(e, 22), 0, None),
            accessed: from_dos(u16le(e, 18), 0, 0, None),
        });
    }
    out
}

/// The long name from LFN entries `lfn` (physical order) that belong to short name `sfn`.
/// For a deleted entry the first byte of `sfn` is `0xE5`: the checksum finds the
/// original one, written back into `sfn`.
fn long_name(lfn: &[&[u8]], sfn: &mut [u8; 11], deleted: bool) -> Option<String> {
    if lfn.is_empty() {
        return None;
    }
    let sum = lfn[0][13];
    if lfn.iter().any(|e| e[13] != sum) {
        return None;
    }
    if !deleted {
        // Ordinals descend to 1, the first physical entry carries the "last" flag.
        let n = lfn.len();
        let ok = lfn.iter().enumerate().all(|(i, e)| (e[0] & 0x1F) as usize == n - i) && lfn[0][0] & 0x40 != 0;
        if !ok || lfn_checksum(sfn) != sum {
            return None;
        }
    }
    let mut units = Vec::with_capacity(13 * lfn.len());
    for e in lfn.iter().rev() {
        for r in [1..11, 14..26, 28..32] {
            units.extend(e[r].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])));
        }
    }
    let end = units.iter().position(|&u| u == 0 || u == 0xFFFF).unwrap_or(units.len());
    let name = String::from_utf16_lossy(&units[..end]);
    if name.is_empty() {
        return None;
    }
    if deleted {
        // Most short names start with the upper-case first letter of the long name.
        let guess = name.chars().next().filter(char::is_ascii).map(|c| c.to_ascii_uppercase() as u8);
        let hit = guess.into_iter().chain(0x20..=0xFFu8).find(|&c| {
            sfn[0] = c;
            lfn_checksum(sfn) == sum
        });
        hit?;
    }
    Some(name)
}

/// Where a directory's entries are.
enum Loc {
    /// FAT12/16 root area.
    Fixed,
    /// Live directory: follow the FAT.
    Chain(u64),
    /// Deleted directory: consecutive clusters from here, the first one with its `.` entry.
    Guess(u64),
    /// A piece of a folder found in free space: consecutive clusters from here, no `.`.
    Part(u64),
}

/// Scanner state shared by the walk from the root and the walks from lost folders.
struct Ctx<'a> {
    vol: Vol<'a>,
    boot: Boot,
    fat: Table,
    base: u64,
    existing: bool,
    b: Builder,
    /// First clusters of folders already walked.
    seen: HashSet<u64>,
    /// Clusters read as folder data.
    covered: HashSet<u64>,
    /// Ids of entries added (the same entry can be reached twice).
    ids: HashSet<u64>,
    /// First cluster -> builder index of folders added.
    dir_of: HashMap<u64, u32>,
    carver: Carver,
    report: FatReport,
}

impl Ctx<'_> {
    fn cs(&self) -> u64 {
        self.boot.cluster_size as u64
    }

    fn cluster_off(&self, c: u64) -> u64 {
        self.boot.data_off + (c - 2) * self.cs()
    }

    /// Bytes of a folder and the volume offset of each cluster of it.
    fn read_dir(&mut self, loc: &Loc) -> (Vec<u8>, Vec<u64>, usize) {
        let cs = self.cs();
        match *loc {
            Loc::Fixed => {
                let d = self.vol.read(self.boot.root_off, self.boot.root_bytes as usize);
                let n = d.len();
                (d, vec![self.boot.root_off], n.max(1))
            }
            Loc::Chain(c) => {
                let cl = self.fat.chain(c, 65536);
                let mut d = Vec::with_capacity(cl.len() * cs as usize);
                for &c in &cl {
                    d.extend(self.vol.read(self.cluster_off(c), cs as usize));
                    self.covered.insert(c);
                }
                let offs = cl.iter().map(|&c| self.cluster_off(c)).collect();
                (d, offs, cs as usize)
            }
            Loc::Guess(c) | Loc::Part(c) => {
                let dotted = matches!(loc, Loc::Guess(_));
                let mut d = Vec::new();
                let mut offs = Vec::new();
                let mut k = c;
                while k <= self.boot.clusters + 1 && offs.len() < 256 {
                    if k != c && (!self.fat.is_free(k) || self.covered.contains(&k)) {
                        break;
                    }
                    let chunk = self.vol.read(self.cluster_off(k), cs as usize);
                    if k == c && dotted && !is_dir_start(&chunk) {
                        break;
                    }
                    // The next folder often follows right after: its "." entry ends this one.
                    if k != c && (chunk[0] == b'.' || !looks_like_dir(&chunk)) {
                        break;
                    }
                    // A cluster with free entries is the last one of its folder.
                    let last = has_end(&chunk);
                    d.extend(chunk);
                    offs.push(self.cluster_off(k));
                    self.covered.insert(k);
                    if last {
                        break;
                    }
                    k += 1;
                }
                (d, offs, cs as usize)
            }
        }
    }

    /// Walk the folder tree from `start`.
    fn walk(&mut self, start: (u32, Loc, bool)) {
        let fat32 = self.boot.bits == 32;
        let cs = self.cs();
        let mut queue = vec![start];
        while let Some((parent, loc, parent_deleted)) = queue.pop() {
            let (data, pieces, piece_len) = self.read_dir(&loc);
            self.report.dirs += 1;
            for de in parse_dir(&data, fat32) {
                let deleted = de.deleted || parent_deleted;
                if !deleted && !de.dir && !self.existing {
                    continue;
                }
                let at = pieces.get(de.at / piece_len).copied().unwrap_or(0) + (de.at % piece_len) as u64;
                let id = (self.base + at) / 32 + 1;
                if !self.ids.insert(id) {
                    continue;
                }
                let mut e = Entry {
                    id,
                    parent,
                    attrs: de.attrs as u32,
                    size: if de.dir { 0 } else { de.size },
                    valid: if de.dir { 0 } else { de.size },
                    created: de.created,
                    modified: de.modified,
                    accessed: de.accessed,
                    ..Default::default()
                };
                if deleted {
                    e.flags |= ef::DELETED;
                }
                if de.dir {
                    e.flags |= ef::DIR;
                }
                let valid_first = (2..=self.boot.clusters + 1).contains(&de.first);
                let mut runs = Vec::new();
                if !de.dir && de.size > 0 && valid_first {
                    let need = de.size.div_ceil(cs);
                    if deleted {
                        let n = need.min(self.boot.clusters + 2 - de.first);
                        runs.push(Run { lcn: de.first - 2, len: n });
                        if n < need {
                            runs.push(Run { lcn: Run::MISSING, len: need - n });
                            e.flags |= ef::INCOMPLETE;
                        }
                    } else {
                        let cl = self.fat.chain(de.first, need);
                        runs = runs_of(&cl.iter().map(|c| c - 2).collect::<Vec<_>>());
                        if (cl.len() as u64) < need {
                            runs.push(Run { lcn: Run::MISSING, len: need - cl.len() as u64 });
                            e.flags |= ef::INCOMPLETE;
                        }
                    }
                }
                if deleted && !de.dir {
                    let fat = &self.fat;
                    e.lost = runs.iter().filter(|r| r.is_real()).flat_map(|r| r.lcn..r.lcn + r.len).filter(|&c| !fat.is_free(c + 2)).count()
                        as u64;
                    if de.size > 0 && !valid_first {
                        e.state = State::NoData;
                    }
                    self.report.deleted_files += 1;
                } else if deleted {
                    self.report.deleted_dirs += 1;
                }
                let idx = self.b.add(e, &de.name, &runs);
                if de.dir && valid_first && self.seen.insert(de.first) {
                    self.dir_of.insert(de.first, idx);
                    queue.push((idx, if deleted { Loc::Guess(de.first) } else { Loc::Chain(de.first) }, deleted));
                }
            }
        }
    }

    /// Lost folders: free clusters that start like a folder (`.` then `..`) and were not
    /// reached from the root. Each comes back under its parent when that is known (the
    /// `..` entry), otherwise under `<Lost files>` as `Folder <cluster>`.
    fn lost_dirs(&mut self, progress: Option<&SweepProgress>) {
        let mut found: Vec<(u64, u64)> = Vec::new();
        let mut parts: Vec<u64> = Vec::new();
        let base = self.base + self.boot.data_off;
        let fat = &self.fat;
        let want = |c: u64| fat.is_free(c + 2);
        let carver = &mut self.carver;
        let bad = sweep(self.vol.src, base, self.boot.cluster_size, self.boot.clusters, &want, progress, &mut |c, b| {
            carver.observe(c, b);
            if is_dir_start(b) && fat_first(b, 0) == c + 2 {
                found.push((c + 2, fat_first(b, 32)));
            } else if looks_like_dir_part(b) {
                parts.push(c + 2);
            }
        });
        self.vol.bad += bad;
        let parent_of: HashMap<u64, u64> = found.iter().copied().collect();
        for &(c, _) in &found {
            if self.seen.contains(&c) {
                continue;
            }
            // Walk from the highest lost ancestor so names come from its entries.
            let mut top = c;
            let mut guard = 0;
            while let Some(&p) = parent_of.get(&top) {
                if p == 0 || self.seen.contains(&p) || !parent_of.contains_key(&p) || guard > 64 {
                    break;
                }
                top = p;
                guard += 1;
            }
            if !self.seen.insert(top) {
                continue;
            }
            let pc = parent_of.get(&top).copied().unwrap_or(0);
            let parent = if pc == 0 || pc == self.boot.root_cluster as u64 { Some(0) } else { self.dir_of.get(&pc).copied() };
            let mut e = Entry { id: FOUND_ID_BASE + top, flags: ef::DIR | ef::DELETED | ef::FOUND, ..Default::default() };
            match parent {
                Some(p) => e.parent = p,
                None => {
                    e.parent = NONE;
                    e.flags |= ef::ORPHAN;
                }
            }
            if !self.ids.insert(e.id) {
                continue;
            }
            let idx = self.b.add(e, &format!("Folder {top}"), &[]);
            self.dir_of.insert(top, idx);
            self.report.lost_dirs += 1;
            self.walk((idx, Loc::Guess(top), true));
        }
        // Pieces of folders that grew past their first cluster (FAT puts the next cluster
        // of a folder wherever the disk is free then): their files keep their names, the
        // folder they belonged to is unknown.
        for c in parts {
            if self.covered.contains(&c) || !self.seen.insert(c) {
                continue;
            }
            let e =
                Entry { id: FOUND_ID_BASE + c, parent: NONE, flags: ef::DIR | ef::DELETED | ef::FOUND | ef::ORPHAN, ..Default::default() };
            if !self.ids.insert(e.id) {
                continue;
            }
            let idx = self.b.add(e, &format!("Folder {c}"), &[]);
            self.report.lost_dirs += 1;
            self.walk((idx, Loc::Part(c), true));
        }
    }
}

/// First cluster field of the directory entry at byte `at`.
fn fat_first(b: &[u8], at: usize) -> u64 {
    (u16le(b, at + 20) as u64) << 16 | u16le(b, at + 26) as u64
}

/// A folder's first cluster: a `.` entry and a `..` entry, both folders.
fn is_dir_start(b: &[u8]) -> bool {
    b.len() >= 64 && b[..11] == *b".          " && b[11] & 0x10 != 0 && b[32..43] == *b"..         " && b[43] & 0x10 != 0
}

pub fn scan(src: &Source, base: u64, opts: &ScanOptions) -> Result<FatScan, fr_core::error::Error> {
    let mut vol = Vol::new(src, base)?;
    let sec = src.sector.max(512) as usize;
    let boot_sec = vol.read(0, sec);
    let boot = parse_boot(&boot_sec).ok_or("not a FAT boot sector")?;
    scan_with_boot(src, base, boot, opts)
}

/// Scan using a validated primary or backup boot sector.
pub fn scan_with_boot(src: &Source, base: u64, boot: Boot, opts: &ScanOptions) -> Result<FatScan, fr_core::error::Error> {
    let t0 = Instant::now();
    let mut vol = Vol::new(src, base)?;
    if base.checked_add(boot.total_bytes).is_none_or(|end| end > src.size) {
        return Err("scan range is outside the source".into());
    }
    vol.limit = boot.total_bytes;
    // First FAT; the second copy when the first is damaged.
    let mut fat = Table { bits: boot.bits, data: Vec::new(), clusters: boot.clusters };
    let mut allocation_known = false;
    for copy in 0..boot.nfats as u64 {
        let before = vol.bad;
        fat.data = vol.read(boot.fat_off + copy * boot.fat_bytes, boot.fat_bytes as usize);
        if vol.bad == before {
            allocation_known = true;
            break;
        }
    }
    let kind = if boot.bits == 32 { FsKind::Fat32 } else { FsKind::Fat16 };
    let mut b = Builder::default();
    let root = b.add(Entry { id: 0, parent: NONE, flags: ef::DIR, ..Default::default() }, "", &[]);
    let root_loc = if boot.bits == 32 { Loc::Chain(boot.root_cluster as u64) } else { Loc::Fixed };
    let mut cx = Ctx {
        vol,
        fat,
        base,
        existing: opts.existing,
        b,
        seen: HashSet::new(),
        covered: HashSet::new(),
        ids: HashSet::new(),
        dir_of: HashMap::new(),
        carver: Carver::default(),
        report: FatReport::default(),
        boot,
    };
    if cx.boot.bits == 32 {
        cx.seen.insert(cx.boot.root_cluster as u64);
    }
    cx.walk((root, root_loc, false));
    if opts.deep {
        cx.lost_dirs(opts.progress.as_deref());
    }
    let fs = FsInfo {
        kind,
        offset: base,
        size: cx.boot.total_bytes,
        cluster_size: cx.boot.cluster_size,
        record_size: 0,
        mft: Vec::new(),
        serial: cx.boot.serial as u64,
        root: 0,
        data_offset: cx.boot.data_off,
    };
    let mut cat = cx.b.finish(fs);
    cx.report.no_allocation = !allocation_known;
    let fat = &cx.fat;
    cx.report.carved = carve_into(&mut cat, src, &cx.carver, &|c| fat.is_free(c + 2));
    let mut report = cx.report;
    crate::assign_states(&mut cat, allocation_known);
    report.bad_bytes = cx.vol.bad;
    report.t = t0.elapsed();
    Ok(FatScan { catalog: cat, report })
}

/// Some entry of the cluster is unused: the folder ends in it.
fn has_end(c: &[u8]) -> bool {
    c.as_chunks::<32>().0.iter().any(|e| e[0] == 0)
}

/// A cluster in the middle of a folder: no `.` entry, every used entry plausible, short
/// entries with valid dates, and enough of them that random data does not pass.
fn looks_like_dir_part(c: &[u8]) -> bool {
    if c[0] == b'.' || c[0] == 0 || !looks_like_dir(c) {
        return false;
    }
    let mut short = 0;
    for e in c.as_chunks::<32>().0.iter().filter(|e| e[0] != 0 && e[11] & 0x3F != 0x0F) {
        let date = u16le(e, 24);
        let (m, d) = ((date >> 5) & 15, date & 31);
        if date != 0 && (!(1..=12).contains(&m) || d == 0) {
            return false;
        }
        if e[..11].iter().all(|&b| b == b' ') {
            return false;
        }
        short += 1;
    }
    short >= 2
}

/// A cluster of a directory: every used entry has a sane attribute byte and name.
fn looks_like_dir(c: &[u8]) -> bool {
    let mut used = 0;
    for e in c.as_chunks::<32>().0.iter() {
        if e[0] == 0 {
            continue;
        }
        let attr = e[11];
        let lfn = attr & 0x3F == 0x0F;
        if !lfn && (attr & 0xC0 != 0 || e[1..11].iter().any(|&b| b < 0x20)) {
            return false;
        }
        used += 1;
    }
    used > 0
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A short entry.
    pub fn sfn_entry(name: &[u8; 11], attr: u8, first: u32, size: u32) -> [u8; 32] {
        let mut e = [0u8; 32];
        e[..11].copy_from_slice(name);
        e[11] = attr;
        e[20..22].copy_from_slice(&((first >> 16) as u16).to_le_bytes());
        e[26..28].copy_from_slice(&(first as u16).to_le_bytes());
        e[28..32].copy_from_slice(&size.to_le_bytes());
        // 2026-03-15 12:00
        e[24..26].copy_from_slice(&(((2026u16 - 1980) << 9) | (3 << 5) | 15).to_le_bytes());
        e[22..24].copy_from_slice(&(12u16 << 11).to_le_bytes());
        e
    }

    /// LFN entries for `long` (physical order) with the checksum of `sfn`.
    pub fn lfn_entries(long: &str, sfn: &[u8; 11]) -> Vec<[u8; 32]> {
        let mut units: Vec<u16> = long.encode_utf16().collect();
        units.push(0);
        while !units.len().is_multiple_of(13) {
            units.push(0xFFFF);
        }
        let n = units.len() / 13;
        let sum = lfn_checksum(sfn);
        (0..n)
            .rev()
            .map(|k| {
                let mut e = [0u8; 32];
                e[0] = (k + 1) as u8 | if k == n - 1 { 0x40 } else { 0 };
                e[11] = 0x0F;
                e[13] = sum;
                let part = &units[k * 13..k * 13 + 13];
                let mut i = 0;
                for r in [1..11, 14..26, 28..32] {
                    for c in e[r].as_chunks_mut::<2>().0.iter_mut() {
                        c.copy_from_slice(&part[i].to_le_bytes());
                        i += 1;
                    }
                }
                e
            })
            .collect()
    }

    #[test]
    fn checksum_known_value() {
        // Checksum is a rotate-right-and-add over the 11 bytes.
        let mut s = 0u8;
        for &c in b"README  TXT" {
            s = (s >> 1 | (s & 1) << 7).wrapping_add(c);
        }
        assert_eq!(lfn_checksum(b"README  TXT"), s);
    }

    #[test]
    fn live_and_deleted_entries_with_long_names() {
        let mut d = Vec::new();
        let sfn = *b"HOLIDA~1JPG";
        for e in lfn_entries("Holiday photo.jpg", &sfn) {
            d.extend_from_slice(&e);
        }
        d.extend_from_slice(&sfn_entry(&sfn, 0x20, 0x0001_0005, 123_456));
        // Deleted: first bytes of all entries become 0xE5.
        let sfn2 = *b"REPORT~1DOC";
        let mut del: Vec<[u8; 32]> = lfn_entries("Report final.docx", &sfn2);
        del.push(sfn_entry(&sfn2, 0x20, 77, 5000));
        for e in &mut del {
            e[0] = 0xE5;
            d.extend_from_slice(e);
        }
        // Deleted without a long name: the first letter is unknown.
        let mut lone = sfn_entry(b"NOTES   TXT", 0x20, 90, 10);
        lone[0] = 0xE5;
        d.extend_from_slice(&lone);
        d.extend_from_slice(&[0u8; 64]);
        let v = parse_dir(&d, true);
        assert_eq!(v.len(), 3);
        assert_eq!((v[0].name.as_str(), v[0].deleted, v[0].first, v[0].size), ("Holiday photo.jpg", false, 0x0001_0005, 123_456));
        assert_eq!((v[1].name.as_str(), v[1].deleted, v[1].first), ("Report final.docx", true, 77));
        assert_eq!((v[2].name.as_str(), v[2].deleted), ("_OTES.TXT", true));
        assert!(v[0].modified > 0);
    }

    #[test]
    fn lfn_of_another_entry_is_ignored() {
        let mut d = Vec::new();
        for e in lfn_entries("Other name.txt", b"OTHER~1 TXT") {
            d.extend_from_slice(&e);
        }
        d.extend_from_slice(&sfn_entry(b"PLAIN   TXT", 0x20, 9, 1));
        let v = parse_dir(&d, true);
        assert_eq!(v[0].name, "PLAIN.TXT");
    }

    #[test]
    fn fat12_and_fat32_tables() {
        // FAT12: entries 0..3 = FF8, FFF, 003, FFF packed in 6 bytes.
        let t = Table { bits: 12, data: vec![0xF8, 0xFF, 0xFF, 0x03, 0xF0, 0xFF], clusters: 10 };
        assert_eq!(t.get(2), 0x003);
        assert_eq!(t.get(3), 0xFFF);
        assert_eq!(t.chain(2, 10), vec![2, 3]);
        let mut d = vec![0u8; 4 * 8];
        for (n, v) in [(2u32, 5u32), (5, 6), (6, 0x0FFF_FFFF)] {
            d[4 * n as usize..4 * n as usize + 4].copy_from_slice(&v.to_le_bytes());
        }
        let t = Table { bits: 32, data: d, clusters: 6 };
        assert_eq!(t.chain(2, 100), vec![2, 5, 6]);
        assert!(t.is_free(3));
    }
}
