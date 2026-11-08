//! What a source holds: one file system starting at offset 0 (a volume or a volume image),
//! or a partitioned disk (MBR with extended partitions, or GPT).

use std::io;

use crate::source::Source;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsKind {
    Ntfs,
    Fat32,
    Fat16,
    ExFat,
    Unknown,
}

impl FsKind {
    pub fn name(self) -> &'static str {
        match self {
            FsKind::Ntfs => "NTFS",
            FsKind::Fat32 => "FAT32",
            FsKind::Fat16 => "FAT16",
            FsKind::ExFat => "exFAT",
            FsKind::Unknown => "unknown",
        }
    }
}

/// File system of a boot sector (first sector of a volume).
pub fn detect_fs(b: &[u8]) -> FsKind {
    if b.len() < 512 {
        return FsKind::Unknown;
    }
    if &b[3..11] == b"NTFS    " {
        return FsKind::Ntfs;
    }
    if &b[3..11] == b"EXFAT   " {
        return FsKind::ExFat;
    }
    if b[510] != 0x55 || b[511] != 0xAA {
        return FsKind::Unknown;
    }
    if &b[0x52..0x5A] == b"FAT32   " {
        return FsKind::Fat32;
    }
    if &b[0x36..0x3B] == b"FAT16" || &b[0x36..0x3B] == b"FAT12" {
        return FsKind::Fat16;
    }
    FsKind::Unknown
}

/// Size in bytes a boot sector gives its volume, if it is one of the known kinds.
pub fn boot_size(b: &[u8]) -> Option<(FsKind, u64)> {
    if b.len() < 512 {
        return None;
    }
    let fs = detect_fs(b);
    let bps = u16::from_le_bytes([b[0x0B], b[0x0C]]) as u64;
    let size = match fs {
        FsKind::Ntfs => u64le(b, 0x28).checked_mul(bps)?,
        FsKind::ExFat => u64le(b, 0x48).checked_mul(1 << b[0x6C].min(12))?,
        FsKind::Fat32 | FsKind::Fat16 => {
            let t16 = u16::from_le_bytes([b[0x13], b[0x14]]) as u64;
            let t = if t16 != 0 { t16 } else { u32le(b, 0x20) as u64 };
            t.checked_mul(bps)?
        }
        FsKind::Unknown => return None,
    };
    (size >= 1 << 20).then_some((fs, size))
}

/// A file system found by [`search`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundFs {
    pub offset: u64,
    pub size: u64,
    pub fs: FsKind,
    /// Found through its backup boot sector (the first one is damaged).
    pub from_backup: bool,
    /// Also listed in the partition table.
    pub in_table: bool,
}

/// Look for file systems anywhere on `src` by their boot sectors. Candidates: partition
/// table entries, every 1 MiB boundary (current partitioning), every cylinder start (old
/// partitioning: 255 heads x 63 sectors, plus the 63-sector gap), every 64 KiB boundary on
/// disks up to 16 GiB (Windows aligns small disks so), and every sector of the 4 MiB after
/// the end of each file system found (the next partition usually starts right there).
/// FAT32 and exFAT also keep a backup boot sector 6 and 12 sectors later.
/// `progress(done, total)` is called now and then.
pub fn search(src: &Source, progress: &mut dyn FnMut(u64, u64)) -> Vec<FoundFs> {
    use std::collections::BTreeSet;
    const MIB: u64 = 1 << 20;
    const CYL: u64 = 255 * 63 * 512;
    let size = src.size;
    let table: Vec<Part> = find(src).unwrap_or_default();
    let mut todo: BTreeSet<u64> = (0..size / MIB).map(|k| k * MIB).collect();
    todo.extend((0..size / CYL).flat_map(|k| [k * CYL, k * CYL + 63 * 512]));
    if size <= 16 << 30 {
        todo.extend((0..size / (64 << 10)).map(|k| k * (64 << 10)));
    }
    todo.extend(table.iter().map(|p| p.offset));
    let mut out: Vec<FoundFs> = Vec::new();
    // (end, serial) of NTFS volumes found, to tell their backup boot sectors apart.
    let mut serials: Vec<(u64, u64)> = Vec::new();
    let total = todo.len() as u64;
    let mut done = 0u64;
    while let Some(off) = todo.pop_first() {
        if src.checkpoint().is_err() {
            break;
        }
        done += 1;
        if done.is_multiple_of(4096) {
            progress(done.min(total), total);
        }
        if out.iter().any(|f| off > f.offset && off < f.offset + f.size) {
            continue; // inside a file system already found
        }
        let mut hit = None;
        for (delta, backup) in [(0u64, false), (6 * 512, true), (12 * 512, true)] {
            let Ok(b) = src.read_bytes(off + delta, 512) else { break };
            // NTFS keeps a copy of its boot sector in the sector right after the size it
            // declares: not a volume of its own.
            if delta == 0 && detect_fs(&b) == FsKind::Ntfs && serials.iter().any(|&(end, s)| end == off && s == u64le(&b, 0x48)) {
                break;
            }
            if let Some((fs, fsize)) = boot_size(&b) {
                if fs == FsKind::Ntfs && delta == 0 {
                    serials.push((off + fsize, u64le(&b, 0x48)));
                }
                let backup_ok = !backup || matches!((fs, delta), (FsKind::Fat32, 3072) | (FsKind::ExFat, 6144));
                if backup_ok && off + fsize <= size + 512 {
                    hit = Some(FoundFs { offset: off, size: fsize, fs, from_backup: backup, in_table: false });
                    break;
                }
            }
        }
        if let Some(mut f) = hit {
            f.in_table = table.iter().any(|p| p.offset == f.offset);
            // Sectors right after it. The NTFS size leaves out its backup boot sector.
            let end = f.offset + f.size;
            if let Ok(tail) = src.read_bytes(end, (4 * MIB).min(size.saturating_sub(end)) as usize) {
                for (k, sec) in tail.chunks_exact(512).enumerate() {
                    if boot_size(sec).is_some() {
                        todo.insert(end + 512 * k as u64);
                    }
                }
            }
            out.push(f);
        }
    }
    progress(total, total);
    out
}

/// One file system found on a source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    /// 0 for a source that is a single volume, otherwise the partition number from 1.
    pub index: u32,
    pub offset: u64,
    pub size: u64,
    pub fs: FsKind,
    /// GPT partition name, if any.
    pub label: String,
}

const GPT_SIG: &[u8] = b"EFI PART";

fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// A partition table entry before its file system is known.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Slot {
    offset: u64,
    size: u64,
    label: String,
}

/// MBR entries of one table sector: (type, first LBA, sector count).
fn mbr_entries(sec: &[u8]) -> Vec<(u8, u64, u64)> {
    (0..4)
        .map(|i| {
            let e = &sec[446 + 16 * i..446 + 16 * (i + 1)];
            (e[4], u32le(e, 8) as u64, u32le(e, 12) as u64)
        })
        .filter(|&(t, _, n)| t != 0 && n != 0)
        .collect()
}

fn is_extended(t: u8) -> bool {
    matches!(t, 0x05 | 0x0F | 0x85)
}

/// Partitions of a disk read through `read(lba_bytes_offset, len)`.
fn table_slots(read: &mut dyn FnMut(u64, usize) -> io::Result<Vec<u8>>, disk_size: u64) -> io::Result<Vec<Slot>> {
    let mbr = read(0, 512)?;
    let valid_mbr = mbr.len() >= 512 && mbr[510] == 0x55 && mbr[511] == 0xAA;
    let entries = if valid_mbr { mbr_entries(&mbr) } else { Vec::new() };
    if !valid_mbr || entries.iter().any(|&(t, _, _)| t == 0xEE) {
        let mut damaged = None;
        for sector in [512u64, 4096] {
            if disk_size < 2 * sector {
                continue;
            }
            for at in [sector, disk_size / sector * sector - sector] {
                let Ok(h) = read(at, 512) else { continue };
                if h.get(..8) == Some(GPT_SIG) {
                    match gpt_slots(read, &h, at, sector, disk_size) {
                        Ok(slots) => return Ok(slots),
                        Err(e) => damaged = Some(e),
                    }
                }
            }
        }
        return damaged.map_or_else(|| Ok(Vec::new()), Err);
    }
    let mut out = Vec::new();
    for &(t, lba, n) in &entries {
        if is_extended(t) {
            // Chain of extended boot records; each holds one logical partition and a link
            // to the next EBR, both relative to the start of the extended partition.
            let base = lba;
            let mut next = base;
            let mut visited = std::collections::HashSet::new();
            while next < disk_size / 512 && visited.insert(next) {
                let ebr = read(next * 512, 512)?;
                if ebr[510] != 0x55 || ebr[511] != 0xAA {
                    break;
                }
                let es = mbr_entries(&ebr);
                if let Some(&(_, l, c)) = es.iter().find(|e| !is_extended(e.0)) {
                    out.push(Slot { offset: (next + l) * 512, size: c * 512, label: String::new() });
                }
                match es.iter().find(|e| is_extended(e.0)) {
                    Some(&(_, l, _)) if base + l != next => next = base + l,
                    _ => break,
                }
            }
        } else {
            out.push(Slot { offset: lba * 512, size: n * 512, label: String::new() });
        }
    }
    Ok(out)
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB88320u32.wrapping_mul(crc & 1));
        }
    }
    !crc
}

fn gpt_slots(
    read: &mut dyn FnMut(u64, usize) -> io::Result<Vec<u8>>,
    h: &[u8],
    at: u64,
    sector: u64,
    disk_size: u64,
) -> io::Result<Vec<Slot>> {
    let bad = || crate::error::Error::new("invalid GPT partition table").into_io(io::ErrorKind::InvalidData);
    if h.len() < 512 {
        return Err(bad());
    }
    let header_size = u32le(h, 0x0C) as usize;
    if !(92..=512).contains(&header_size) || u64le(h, 0x18).checked_mul(sector) != Some(at) {
        return Err(bad());
    }
    let mut header = h[..header_size].to_vec();
    header[0x10..0x14].fill(0);
    if crc32(&header) != u32le(h, 0x10) {
        return Err(bad());
    }
    let first = u64le(h, 0x48).checked_mul(sector).ok_or_else(bad)?;
    let count = u32le(h, 0x50) as usize;
    let esize = u32le(h, 0x54) as usize;
    let bytes = count.checked_mul(esize).ok_or_else(bad)?;
    // A malformed header must not allocate an unbounded table or silently truncate it.
    if count == 0 || !(128..=4096).contains(&esize) || !esize.is_multiple_of(128) || bytes > 64 << 20 {
        return Err(bad());
    }
    let read_len = bytes.div_ceil(sector as usize) * sector as usize;
    if first.checked_add(read_len as u64).is_none_or(|end| end > disk_size) {
        return Err(bad());
    }
    let (first_usable, last_usable) = (u64le(h, 0x28), u64le(h, 0x30));
    if first_usable > last_usable || last_usable >= disk_size / sector {
        return Err(bad());
    }
    let table = read(first, read_len)?;
    let table = table.get(..bytes).ok_or_else(bad)?;
    if crc32(table) != u32le(h, 0x58) {
        return Err(bad());
    }
    let mut out = Vec::new();
    for e in table.chunks_exact(esize) {
        if e[..16].iter().all(|&b| b == 0) {
            continue;
        }
        let (lo, hi) = (u64le(e, 0x20), u64le(e, 0x28));
        if hi < lo || lo < first_usable || hi > last_usable {
            continue;
        }
        let offset = lo.checked_mul(sector).ok_or_else(bad)?;
        let size = (hi - lo + 1).checked_mul(sector).ok_or_else(bad)?;
        let units: Vec<u16> =
            e[0x38..0x38 + 72].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
        out.push(Slot { offset, size, label: String::from_utf16_lossy(&units) });
    }
    Ok(out)
}

/// File systems on `src`: the source itself when it starts with a boot sector, otherwise
/// every partition of its partition table.
pub fn find(src: &Source) -> io::Result<Vec<Part>> {
    let first = src.read_bytes(0, 512)?;
    let fs = detect_fs(&first);
    if fs != FsKind::Unknown {
        return Ok(vec![Part { index: 0, offset: 0, size: src.size, fs, label: String::new() }]);
    }
    let size = src.size;
    let mut read = |off: u64, len: usize| -> io::Result<Vec<u8>> {
        if off.checked_add(len as u64).is_none_or(|end| end > size) {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, crate::error::Error::new("partition table outside the source")));
        }
        src.read_bytes(off, len)
    };
    let slots = table_slots(&mut read, size)?;
    let mut out = Vec::new();
    for (i, s) in slots.into_iter().enumerate() {
        if s.offset >= size {
            continue;
        }
        let fs = read(s.offset, 512).map(|b| detect_fs(&b)).unwrap_or(FsKind::Unknown);
        out.push(Part { index: i as u32 + 1, offset: s.offset, size: s.size.min(size - s.offset), fs, label: s.label });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(size: usize) -> Vec<u8> {
        vec![0u8; size]
    }

    fn mbr_entry(d: &mut [u8], sector: usize, i: usize, t: u8, lba: u32, n: u32) {
        let o = sector * 512 + 446 + 16 * i;
        d[o + 4] = t;
        d[o + 8..o + 12].copy_from_slice(&lba.to_le_bytes());
        d[o + 12..o + 16].copy_from_slice(&n.to_le_bytes());
        d[sector * 512 + 510] = 0x55;
        d[sector * 512 + 511] = 0xAA;
    }

    fn slots(d: &[u8]) -> Vec<(u64, u64)> {
        let mut read = |off: u64, len: usize| Ok(d[off as usize..off as usize + len].to_vec());
        table_slots(&mut read, d.len() as u64).unwrap().iter().map(|s| (s.offset, s.size)).collect()
    }

    #[test]
    fn mbr_with_logical_partitions() {
        let mut d = disk(64 * 512);
        mbr_entry(&mut d, 0, 0, 0x07, 2, 10);
        mbr_entry(&mut d, 0, 1, 0x0F, 20, 40);
        // EBR at 20: logical at 20+1, next EBR at 20+10
        mbr_entry(&mut d, 20, 0, 0x07, 1, 5);
        mbr_entry(&mut d, 20, 1, 0x05, 10, 20);
        // EBR at 30: logical at 30+1, end of chain
        mbr_entry(&mut d, 30, 0, 0x0B, 1, 8);
        assert_eq!(slots(&d), vec![(1024, 5120), (21 * 512, 5 * 512), (31 * 512, 8 * 512)]);
    }

    #[test]
    fn gpt_entries_and_names() {
        let mut d = disk(64 * 512);
        mbr_entry(&mut d, 0, 0, 0xEE, 1, 63);
        let h = 512;
        d[h..h + 8].copy_from_slice(GPT_SIG);
        d[h + 0x48..h + 0x50].copy_from_slice(&2u64.to_le_bytes());
        d[h + 0x50..h + 0x54].copy_from_slice(&4u32.to_le_bytes());
        d[h + 0x54..h + 0x58].copy_from_slice(&128u32.to_le_bytes());
        let e = 2 * 512 + 128; // second entry; the first stays empty
        d[e] = 1;
        d[e + 0x20..e + 0x28].copy_from_slice(&34u64.to_le_bytes());
        d[e + 0x28..e + 0x30].copy_from_slice(&40u64.to_le_bytes());
        for (k, u) in "Data".encode_utf16().enumerate() {
            d[e + 0x38 + 2 * k..e + 0x3A + 2 * k].copy_from_slice(&u.to_le_bytes());
        }
        d[h + 0x0C..h + 0x10].copy_from_slice(&92u32.to_le_bytes());
        d[h + 0x18..h + 0x20].copy_from_slice(&1u64.to_le_bytes());
        d[h + 0x28..h + 0x30].copy_from_slice(&34u64.to_le_bytes());
        d[h + 0x30..h + 0x38].copy_from_slice(&61u64.to_le_bytes());
        let crc = crc32(&d[1024..1536]);
        d[h + 0x58..h + 0x5C].copy_from_slice(&crc.to_le_bytes());
        let crc = crc32(&d[h..h + 92]);
        d[h + 0x10..h + 0x14].copy_from_slice(&crc.to_le_bytes());
        let mut read = |off: u64, len: usize| Ok(d[off as usize..off as usize + len].to_vec());
        let s = table_slots(&mut read, d.len() as u64).unwrap();
        assert_eq!(s, vec![Slot { offset: 34 * 512, size: 7 * 512, label: "Data".into() }]);

        let table = d[1024..1536].to_vec();
        d[62 * 512..63 * 512].copy_from_slice(&table);
        let mut backup = d[h..h + 512].to_vec();
        backup[0x18..0x20].copy_from_slice(&63u64.to_le_bytes());
        backup[0x48..0x50].copy_from_slice(&62u64.to_le_bytes());
        backup[0x10..0x14].fill(0);
        let crc = crc32(&backup[..92]);
        backup[0x10..0x14].copy_from_slice(&crc.to_le_bytes());
        d[63 * 512..64 * 512].copy_from_slice(&backup);
        d[h + 0x10] ^= 1; // The primary header is damaged; the backup is independent.
        let mut read = |off: u64, len: usize| Ok(d[off as usize..off as usize + len].to_vec());
        assert_eq!(table_slots(&mut read, d.len() as u64).unwrap(), s);
    }

    #[test]
    fn gpt_checksums_and_offsets_reject_corrupt_geometry() {
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
        let mut h = vec![0u8; 512];
        h[..8].copy_from_slice(GPT_SIG);
        h[0x0C..0x10].copy_from_slice(&92u32.to_le_bytes());
        h[0x18..0x20].copy_from_slice(&1u64.to_le_bytes());
        h[0x48..0x50].copy_from_slice(&u64::MAX.to_le_bytes());
        let crc = crc32(&h[..92]);
        h[0x10..0x14].copy_from_slice(&crc.to_le_bytes());
        let mut read = |_, _| panic!("invalid table must not be read");
        assert!(gpt_slots(&mut read, &h, 512, 512, 32768).is_err());
        h[0x10] ^= 1;
        assert!(gpt_slots(&mut read, &h, 512, 512, 32768).is_err());
    }

    #[test]
    fn file_system_signatures() {
        let mut b = vec![0u8; 512];
        b[3..11].copy_from_slice(b"NTFS    ");
        assert_eq!(detect_fs(&b), FsKind::Ntfs);
        let mut b = vec![0u8; 512];
        b[0x52..0x5A].copy_from_slice(b"FAT32   ");
        assert_eq!(detect_fs(&b), FsKind::Unknown, "no boot signature");
        b[510] = 0x55;
        b[511] = 0xAA;
        assert_eq!(detect_fs(&b), FsKind::Fat32);
    }
}
