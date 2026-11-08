//! exFAT.
//!
//! A file is a set of directory entries: a file entry (type 0x85: attributes, times),
//! a stream extension (0xC0: first cluster, sizes, "no FAT chain" flag) and file name
//! entries (0xC1, 15 characters each). Deleting it only clears bit 7 of each type byte
//! (0x05, 0x40, 0x41) and frees its clusters in the allocation bitmap. With the bit put
//! back, the set checksum of the file entry verifies the whole set.
//!
//! Files written in one piece carry the "no FAT chain" flag: their location is exact.
//! Others keep their chain in the FAT, which deletion does not clear, so it is followed
//! as long as it stays in free clusters.

use std::collections::HashSet;
use std::time::Instant;

use fr_core::carve::{Carver, carve_into};
use fr_core::model::{Builder, Entry, FOUND_ID_BASE, FsInfo, NONE, Run, ef};
use fr_core::part::FsKind;
use fr_core::source::Source;
use fr_core::sweep::{SweepProgress, sweep};
use fr_core::time::from_dos;

use crate::io::{Vol, count_set, runs_of};
use crate::{FatReport, FatScan, ScanOptions};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Boot {
    pub bps: u32,
    pub cluster_size: u32,
    pub fat_off: u64,
    pub fat_bytes: u64,
    pub heap_off: u64,
    pub clusters: u64,
    pub root_cluster: u32,
    pub serial: u32,
    pub total_bytes: u64,
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

pub fn parse_boot(b: &[u8]) -> Option<Boot> {
    if b.len() < 512 || &b[3..11] != b"EXFAT   " {
        return None;
    }
    let bps_shift = b[0x6C] as u32;
    let spc_shift = b[0x6D] as u32;
    if !(9..=12).contains(&bps_shift) || bps_shift + spc_shift > 25 {
        return None;
    }
    let bps = 1u64 << bps_shift;
    let clusters = u32le(b, 0x5C) as u64;
    let fat_len = u32le(b, 0x54) as u64;
    let fat_off = u32le(b, 0x50) as u64 * bps;
    let heap_off = u32le(b, 0x58) as u64 * bps;
    let cluster_size = 1u64 << (bps_shift + spc_shift);
    let total_bytes = u64le(b, 0x48).checked_mul(bps)?;
    let root_cluster = u32le(b, 0x60);
    let nfats = b[0x6E] as u64;
    if clusters == 0
        || fat_len * bps < (clusters + 2) * 4
        || !(1..=2).contains(&nfats)
        || fat_off == 0
        || fat_off + fat_len * bps * nfats > heap_off
        || heap_off + clusters * cluster_size > total_bytes
        || !(2..clusters + 2).contains(&(root_cluster as u64))
    {
        return None;
    }
    Some(Boot {
        bps: bps as u32,
        cluster_size: cluster_size as u32,
        fat_off,
        fat_bytes: fat_len * bps,
        heap_off,
        clusters,
        root_cluster,
        serial: u32le(b, 0x64),
        total_bytes,
    })
}

/// Set checksum of an entry set (the checksum field itself, bytes 2-3, is skipped).
pub fn set_checksum(set: &[u8]) -> u16 {
    set.iter().enumerate().filter(|(i, _)| *i != 2 && *i != 3).fold(0u16, |s, (_, &b)| s.rotate_right(1).wrapping_add(b as u16))
}

/// Timestamp, 10 ms increment and UTC offset fields to FILETIME.
fn stamp(ts: u32, ms10: u8, tz: u8) -> u64 {
    let off = (tz & 0x80 != 0).then(|| ((((tz & 0x7F) << 1) as i8) >> 1) as i32 * 15);
    from_dos((ts >> 16) as u16, ts as u16, ms10, off)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// Byte offset of the file entry inside the directory data.
    pub at: usize,
    pub name: String,
    pub deleted: bool,
    pub dir: bool,
    pub attrs: u16,
    pub first: u64,
    pub size: u64,
    pub valid: u64,
    pub no_chain: bool,
    pub created: u64,
    pub modified: u64,
    pub accessed: u64,
}

/// Special entries of a root directory.
#[derive(Debug, Default)]
pub struct RootInfo {
    pub bitmap: Option<(u64, u64)>,
    pub label: String,
}

/// Entry sets of `d`; for the root, also the bitmap location and the volume label.
pub fn parse_dir(d: &[u8], root: &mut RootInfo) -> Vec<DirEntry> {
    let mut out = Vec::new();
    let n = d.len() / 32;
    let mut k = 0;
    while k < n {
        let e = &d[k * 32..k * 32 + 32];
        let t = e[0];
        if t == 0 {
            break;
        }
        match t {
            0x81 => root.bitmap = Some((u32le(e, 20) as u64, u64le(e, 24))),
            0x83 => {
                let c = (e[1] as usize).min(11);
                let units: Vec<u16> = e[2..2 + 2 * c].chunks_exact(2).map(|x| u16::from_le_bytes([x[0], x[1]])).collect();
                root.label = String::from_utf16_lossy(&units);
            }
            _ => {}
        }
        if t & 0x7F != 0x05 {
            k += 1;
            continue;
        }
        let sc = e[1] as usize;
        if !(2..=18).contains(&sc) || k + sc >= n {
            k += 1;
            continue;
        }
        let in_use = t & 0x80 != 0;
        let set = &d[k * 32..(k + 1 + sc) * 32];
        let stream = &set[32..64];
        // A stream extension, then the name entries the name length needs (vendor entries
        // may follow), all with the same "in use" bit as the file entry.
        let name_len = stream[3] as usize;
        let needed = 1 + name_len.div_ceil(15);
        let bit = if in_use { 0x80 } else { 0 };
        let ok = sc >= needed
            && set
                .chunks_exact(32)
                .skip(1)
                .take(needed)
                .enumerate()
                .all(|(j, x)| x[0] & 0x7F == if j == 0 { 0x40 } else { 0x41 } && x[0] & 0x80 == bit);
        if !ok {
            k += 1;
            continue;
        }
        let mut fixed = set.to_vec();
        for x in fixed.chunks_exact_mut(32) {
            x[0] |= 0x80;
        }
        if set_checksum(&fixed) != u16le(e, 2) || name_len == 0 {
            k += 1;
            continue;
        }
        let mut units = Vec::with_capacity(name_len);
        for x in set.chunks_exact(32).skip(2).take(needed - 1) {
            units.extend(x[2..32].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])));
        }
        units.truncate(name_len);
        let attrs = u16le(e, 4);
        out.push(DirEntry {
            at: k * 32,
            name: String::from_utf16_lossy(&units),
            deleted: !in_use,
            dir: attrs & 0x10 != 0,
            attrs,
            first: u32le(stream, 20) as u64,
            size: u64le(stream, 24),
            valid: u64le(stream, 8),
            no_chain: stream[1] & 2 != 0,
            created: stamp(u32le(e, 8), e[20], e[22]),
            modified: stamp(u32le(e, 12), e[21], e[23]),
            accessed: stamp(u32le(e, 16), 0, e[24]),
        });
        k += 1 + sc;
    }
    out
}

struct Fat {
    data: Vec<u8>,
    clusters: u64,
}

impl Fat {
    fn chain(&self, first: u64, max: u64) -> Vec<u64> {
        let mut out = Vec::new();
        let mut c = first;
        while (2..=self.clusters + 1).contains(&c) && (out.len() as u64) < max {
            out.push(c);
            let Some(b) = self.data.get(4 * c as usize..4 * c as usize + 4) else { break };
            let next = u32le(b, 0) as u64;
            if !(2..0xFFFF_FFF7).contains(&next) {
                break;
            }
            c = next;
        }
        out
    }
}

/// Clusters (0-based) of a stream: contiguous with "no FAT chain", otherwise the chain.
/// For a deleted stream the chain is used only if it stays in clusters that are free now.
fn stream_clusters(fat: &Fat, bm: &[u8], first: u64, need: u64, no_chain: bool, deleted: bool) -> Vec<u64> {
    if need == 0 || !(2..=fat.clusters + 1).contains(&first) {
        return Vec::new();
    }
    let contiguous = || (first - 2..(first - 2 + need).min(fat.clusters)).collect::<Vec<_>>();
    if no_chain {
        return contiguous();
    }
    let chain: Vec<u64> = fat.chain(first, need).iter().map(|c| c - 2).collect();
    if !deleted {
        return chain;
    }
    let free = chain.iter().all(|&c| count_set(bm, c, 1) == 0);
    if chain.len() as u64 == need && free { chain } else { contiguous() }
}

/// Scanner state shared by the walk from the root and the walks from lost folders.
struct Ctx<'a> {
    vol: Vol<'a>,
    boot: Boot,
    fat: Fat,
    bm: Vec<u8>,
    base: u64,
    existing: bool,
    b: Builder,
    seen: HashSet<u64>,
    /// Clusters (0-based) read as folder data.
    covered: HashSet<u64>,
    ids: HashSet<u64>,
    carver: Carver,
    report: FatReport,
}

impl Ctx<'_> {
    fn cs(&self) -> u64 {
        self.boot.cluster_size as u64
    }

    fn read_clusters(&mut self, cl: &[u64]) -> Vec<u8> {
        let cs = self.cs();
        let mut d = Vec::with_capacity(cl.len() * cs as usize);
        for r in runs_of(cl) {
            d.extend(self.vol.read(self.boot.heap_off + r.lcn * cs, (r.len * cs) as usize));
        }
        d
    }

    /// Walk the folder tree from a folder whose entries are already parsed.
    fn walk(&mut self, start: (u32, Vec<DirEntry>, Vec<u64>, bool)) {
        let cs = self.cs();
        let mut queue = vec![start];
        while let Some((parent, entries, dir_cl, parent_deleted)) = queue.pop() {
            self.report.dirs += 1;
            self.covered.extend(dir_cl.iter().copied());
            for de in entries {
                let deleted = de.deleted || parent_deleted;
                if !deleted && !de.dir && !self.existing {
                    continue;
                }
                let at = dir_cl.get(de.at / cs as usize).map_or(0, |c| self.boot.heap_off + c * cs) + (de.at as u64 % cs);
                let id = (self.base + at) / 32 + 1;
                if !self.ids.insert(id) {
                    continue;
                }
                let need = de.size.div_ceil(cs);
                let cl = stream_clusters(&self.fat, &self.bm, de.first, need, de.no_chain, deleted);
                let mut runs = runs_of(&cl);
                let mut e = Entry {
                    id,
                    parent,
                    attrs: de.attrs as u32,
                    size: if de.dir { 0 } else { de.size },
                    valid: if de.dir { 0 } else { de.valid.min(de.size) },
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
                if !de.dir && (cl.len() as u64) < need && !cl.is_empty() {
                    runs.push(Run { lcn: Run::MISSING, len: need - cl.len() as u64 });
                    e.flags |= ef::INCOMPLETE;
                }
                if deleted && !de.dir {
                    e.lost = cl.iter().map(|&c| count_set(&self.bm, c, 1)).sum();
                    self.report.deleted_files += 1;
                } else if deleted && de.dir {
                    self.report.deleted_dirs += 1;
                }
                let file_runs = if de.dir { Vec::new() } else { runs };
                let idx = self.b.add(e, &de.name, &file_runs);
                if de.dir && !cl.is_empty() && self.seen.insert(de.first) {
                    let data = self.read_clusters(&cl);
                    let children = parse_dir(&data, &mut RootInfo::default());
                    queue.push((idx, children, cl, deleted));
                }
            }
        }
    }

    /// Lost folders: free clusters whose first entries form a valid entry set, not reached
    /// from the root. exFAT folders do not name their parent, so each comes back under
    /// `<Lost files>` as `Folder <cluster>`; folders inside it keep their names.
    fn lost_dirs(&mut self, progress: Option<&SweepProgress>) {
        let mut found: Vec<u64> = Vec::new();
        let base = self.base + self.boot.heap_off;
        let bm = &self.bm;
        let want = |c: u64| count_set(bm, c, 1) == 0;
        // A folder cluster holds entry sets; one may have started in the cluster before,
        // so the first set is looked for among the first 20 entries.
        let carver = &mut self.carver;
        let bad = sweep(self.vol.src, base, self.boot.cluster_size, self.boot.clusters, &want, progress, &mut |c, b| {
            carver.observe(c, b);
            let head = &b[..b.len().min(40 * 32)];
            if head.chunks_exact(32).take(20).any(|e| e[0] & 0x7F == 0x05) && !parse_dir(head, &mut RootInfo::default()).is_empty() {
                found.push(c);
            }
        });
        self.vol.bad += bad;
        let cs = self.cs();
        for c in found {
            if self.covered.contains(&c) {
                continue;
            }
            // A folder spanning several clusters: a cluster with no end-of-folder entry
            // (type 0) continues in the next one.
            let mut cl = vec![c];
            let mut data = self.read_clusters(&cl);
            while cl.len() < 256 && !data[data.len() - cs as usize..].chunks_exact(32).any(|e| e[0] == 0) {
                let n = c + cl.len() as u64;
                if n >= self.boot.clusters || count_set(&self.bm, n, 1) != 0 || self.covered.contains(&n) {
                    break;
                }
                data.extend(self.read_clusters(&[n]));
                cl.push(n);
            }
            let children = parse_dir(&data, &mut RootInfo::default());
            if children.is_empty() {
                continue;
            }
            // Its own id range: the position of its first entry belongs to that entry.
            let e =
                Entry { id: FOUND_ID_BASE + c, parent: NONE, flags: ef::DIR | ef::DELETED | ef::FOUND | ef::ORPHAN, ..Default::default() };
            if !self.ids.insert(e.id) {
                continue;
            }
            let idx = self.b.add(e, &format!("Folder {}", c + 2), &[]);
            self.report.lost_dirs += 1;
            self.walk((idx, children, cl, true));
        }
    }
}

pub fn scan(src: &Source, base: u64, opts: &ScanOptions) -> Result<FatScan, fr_core::error::Error> {
    let mut vol = Vol::new(src, base)?;
    let sec = src.sector.max(512) as usize;
    let boot = parse_boot(&vol.read(0, sec)).ok_or("not an exFAT boot sector")?;
    scan_with_boot(src, base, boot, opts)
}

/// Scan using a validated primary or backup boot sector.
pub fn scan_with_boot(src: &Source, base: u64, boot: Boot, opts: &ScanOptions) -> Result<FatScan, fr_core::error::Error> {
    let t0 = Instant::now();
    let mut vol = Vol::new(src, base)?;
    let sec = src.sector.max(512) as usize;
    if base.checked_add(boot.total_bytes).is_none_or(|end| end > src.size) {
        return Err("scan range is outside the source".into());
    }
    vol.limit = boot.total_bytes;
    let cs = boot.cluster_size as u64;
    let fat = Fat { data: vol.read(boot.fat_off, ((boot.clusters + 2) * 4).div_ceil(sec as u64) as usize * sec), clusters: boot.clusters };
    let mut b = Builder::default();
    let root = b.add(Entry { id: 0, parent: NONE, flags: ef::DIR, ..Default::default() }, "", &[]);
    let mut cx = Ctx {
        vol,
        fat,
        bm: Vec::new(),
        base,
        existing: opts.existing,
        b,
        seen: HashSet::new(),
        covered: HashSet::new(),
        ids: HashSet::new(),
        carver: Carver::default(),
        report: FatReport::default(),
        boot,
    };
    // Root directory, then the allocation bitmap it points to.
    let root_cl: Vec<u64> = cx.fat.chain(cx.boot.root_cluster as u64, 1 << 16).iter().map(|c| c - 2).collect();
    let root_data = cx.read_clusters(&root_cl);
    let mut info = RootInfo::default();
    let root_entries = parse_dir(&root_data, &mut info);
    let mut allocation_known = false;
    if let Some((first, len)) = info.bitmap {
        let before = cx.vol.bad;
        let cl: Vec<u64> = cx.fat.chain(first, len.div_ceil(cs)).iter().map(|c| c - 2).collect();
        let mut d = cx.read_clusters(&cl);
        d.truncate(len as usize);
        allocation_known = cx.vol.bad == before && d.len() as u64 >= cx.boot.clusters.div_ceil(8);
        cx.bm = d;
    }
    cx.seen.insert(cx.boot.root_cluster as u64);
    cx.walk((root, root_entries, root_cl, false));
    if opts.deep {
        cx.lost_dirs(opts.progress.as_deref());
    }
    let fs = FsInfo {
        kind: FsKind::ExFat,
        offset: base,
        size: cx.boot.total_bytes,
        cluster_size: cx.boot.cluster_size,
        record_size: 0,
        mft: Vec::new(),
        serial: cx.boot.serial as u64,
        root: 0,
        data_offset: cx.boot.heap_off,
    };
    let mut cat = cx.b.finish(fs);
    cx.report.no_allocation = !allocation_known;
    let bm = &cx.bm;
    cx.report.carved = carve_into(&mut cat, src, &cx.carver, &|c| count_set(bm, c, 1) == 0);
    let mut report = cx.report;
    crate::assign_states(&mut cat, allocation_known);
    report.bad_bytes = cx.vol.bad;
    report.t = t0.elapsed();
    Ok(FatScan { catalog: cat, report })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file entry set named `name`, deleted or not.
    fn set(name: &str, first: u32, size: u64, no_chain: bool, deleted: bool) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let names = units.len().div_ceil(15);
        let mut s = vec![0u8; 32 * (2 + names)];
        s[0] = 0x85;
        s[1] = (1 + names) as u8;
        s[4..6].copy_from_slice(&0x20u16.to_le_bytes());
        let ts = (((2026u32 - 1980) << 9 | 3 << 5 | 15) << 16) | (12 << 11);
        s[12..16].copy_from_slice(&ts.to_le_bytes());
        s[23] = 0x80 | 12; // UTC+3
        s[32] = 0xC0;
        s[33] = 1 | if no_chain { 2 } else { 0 };
        s[35] = units.len() as u8;
        s[40..48].copy_from_slice(&size.to_le_bytes());
        s[52..56].copy_from_slice(&first.to_le_bytes());
        s[56..64].copy_from_slice(&size.to_le_bytes());
        for (j, chunk) in units.chunks(15).enumerate() {
            let o = 64 + 32 * j;
            s[o] = 0xC1;
            for (i, u) in chunk.iter().enumerate() {
                s[o + 2 + 2 * i..o + 4 + 2 * i].copy_from_slice(&u.to_le_bytes());
            }
        }
        let sum = set_checksum(&s);
        s[2..4].copy_from_slice(&sum.to_le_bytes());
        if deleted {
            for x in s.chunks_exact_mut(32) {
                x[0] &= 0x7F;
            }
        }
        s
    }

    #[test]
    fn live_and_deleted_sets() {
        let mut d = Vec::new();
        d.extend(set("A rather long file name.txt", 10, 5000, true, false));
        d.extend(set("deleted photo.jpg", 20, 70000, true, true));
        // Damaged deleted set: a byte of the name changed, the checksum no longer fits.
        let mut bad = set("broken.bin", 30, 10, true, true);
        bad[70] ^= 1;
        d.extend(bad);
        d.extend([0u8; 32]);
        let v = parse_dir(&d, &mut RootInfo::default());
        assert_eq!(v.len(), 2);
        assert_eq!(
            (v[0].name.as_str(), v[0].deleted, v[0].first, v[0].size, v[0].no_chain),
            ("A rather long file name.txt", false, 10, 5000, true)
        );
        assert_eq!((v[1].name.as_str(), v[1].deleted), ("deleted photo.jpg", true));
        assert!(v[0].modified > 0);
    }

    #[test]
    fn utc_offsets() {
        let ts = (((2026u32 - 1980) << 9 | 3 << 5 | 15) << 16) | (12 << 11);
        let utc3 = stamp(ts, 0, 0x80 | 12);
        let utc0 = stamp(ts, 0, 0x80);
        assert_eq!(utc0 - utc3, 3 * 3600 * 10_000_000);
        let west = stamp(ts, 0, 0x80 | 0x7C); // -4 * 15 min
        assert_eq!(west - utc0, 3600 * 10_000_000);
    }

    #[test]
    fn deleted_chain_used_only_when_free() {
        let mut data = vec![0u8; 4 * 12];
        for (n, v) in [(2u32, 7u32), (7, 9), (9, 0xFFFF_FFFF)] {
            data[4 * n as usize..4 * n as usize + 4].copy_from_slice(&v.to_le_bytes());
        }
        let fat = Fat { data, clusters: 10 };
        let free = vec![0u8; 2];
        assert_eq!(stream_clusters(&fat, &free, 2, 3, false, true), vec![0, 5, 7]);
        let mut used = vec![0u8; 2];
        used[0] = 1 << 5; // cluster 7 (0-based 5) taken
        assert_eq!(stream_clusters(&fat, &used, 2, 3, false, true), vec![0, 1, 2], "falls back to contiguous");
        assert_eq!(stream_clusters(&fat, &free, 2, 3, true, true), vec![0, 1, 2]);
    }
}
