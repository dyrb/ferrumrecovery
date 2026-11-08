//! What NTFS remembers outside the file records, read after the catalog is built.
//!
//! * Recycle Bin: an item moved there becomes `$Rxxxxxx.ext`; its original path, size
//!   and deletion time are in `$Ixxxxxx.ext` next to it. `$I` files are a few hundred
//!   bytes, stored inside their file records, so they survive emptying the bin.
//! * Change journal (`$Extend\$UsnJrnl:$J`): every deletion is logged with the file
//!   name, folder and time. It dates deleted files, and names those whose records are
//!   already reused.
//! * Folder index slack (`$I30`): index buffers keep stale copies of entries that were
//!   removed, with name, size and times, after the live entries.
//!
//! The last two only name files: without a file record there is nothing to recover, so
//! they become trace entries (state no-data), shown on request.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use fr_core::data::Reader;
use fr_core::model::{Catalog, Data, Entry, NONE, State, TRACE_ID_BASE, ef};
use fr_core::source::ReadSlot;

use crate::bytes::*;
use crate::mft::Volume;
use crate::record::apply_fixup;
use crate::scan::{Aux, K_I30, K_USN, ScanOptions, ScanReport, name_key, resolve_parent};

const USN_REASON_FILE_DELETE: u32 = 0x200;
/// FILETIME of 1980-01-01 and 2107-01-01: anything outside is not a timestamp.
const T_MIN: u64 = 119_600_064_000_000_000;
const T_MAX: u64 = 159_670_656_000_000_000;

pub(crate) fn run(vol: &Volume, cat: &mut Catalog, aux: &Aux, opts: &ScanOptions, report: &mut ScanReport) {
    report.recycled = recycle_bin(vol, cat);
    let mut traces = Vec::new();
    match journal(vol, cat, aux) {
        Some((dated, lost)) => {
            report.dated = dated;
            if opts.traces {
                traces.extend(lost);
            }
        }
        None => report.no_journal = true,
    }
    if opts.traces {
        traces.extend(index_slack(vol, cat, aux, opts.threads));
        // A name that exists again in the same folder is an older version of a live file
        // (programs often save by deleting and recreating): not what anyone looks for.
        traces.retain(|c| c.parent == NONE || !aux.live_names.contains(&(cat.entries[c.parent as usize].id, name_key(&c.name))));
        report.traces = add_traces(cat, traces);
    }
}

fn utf16(b: &[u8]) -> String {
    let units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&u| u != 0).collect();
    String::from_utf16_lossy(&units)
}

/// `$I` file: (original path, size, deletion time). Version 1 (Vista to 8.1) has a fixed
/// 260-character path, version 2 (Windows 10 and later) a length-prefixed one.
pub fn parse_recycle_info(b: &[u8]) -> Option<(String, u64, u64)> {
    if b.len() < 28 {
        return None;
    }
    let (ver, size, time) = (u64le(b, 0), u64le(b, 8), u64le(b, 16));
    let path = match ver {
        1 => utf16(&b[24..b.len().min(24 + 520)]),
        2 => {
            let n = u32le(b, 24) as usize;
            utf16(&b[28..b.len().min(28 + 2 * n)])
        }
        _ => return None,
    };
    if path.is_empty() || !(T_MIN..T_MAX).contains(&time) {
        return None;
    }
    Some((path, size, time))
}

/// `C:\Users\Me\a.txt` -> `\Users\Me\a.txt`.
fn strip_drive(p: &str) -> String {
    let b = p.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        let rest = &p[2..];
        if rest.starts_with('\\') { rest.to_string() } else { format!("\\{rest}") }
    } else if p.starts_with('\\') {
        p.to_string()
    } else {
        format!("\\{p}")
    }
}

/// Give Recycle Bin items their original paths and deletion times. Returns how many.
fn recycle_bin(vol: &Volume, cat: &mut Catalog) -> u64 {
    let is_bin = |cat: &Catalog, i: u32| cat.entries[i as usize].is_dir() && cat.name(i).eq_ignore_ascii_case("$Recycle.Bin");
    // Per-user folders inside a bin.
    let user_dirs: HashSet<u32> = (0..cat.entries.len() as u32)
        .filter(|&i| {
            let e = &cat.entries[i as usize];
            e.is_dir() && e.parent != NONE && is_bin(cat, e.parent)
        })
        .collect();
    if user_dirs.is_empty() {
        return 0;
    }
    let mut infos = Vec::new();
    let mut items: HashMap<(u32, String), Vec<u32>> = HashMap::new();
    for i in 0..cat.entries.len() as u32 {
        let e = &cat.entries[i as usize];
        if !user_dirs.contains(&e.parent) {
            continue;
        }
        let name = cat.name(i);
        if name.len() < 3 || !name.is_char_boundary(2) {
            continue;
        }
        let suffix = name[2..].to_uppercase();
        if name.starts_with("$I") && !e.is_dir() && e.size <= 64 << 10 {
            infos.push((i, suffix));
        } else if name.starts_with("$R") {
            items.entry((e.parent, suffix)).or_default().push(i);
        }
    }
    let Ok(mut reader) = Reader::new(vol.src, cat) else { return 0 };
    let mut found = Vec::new();
    for (i, suffix) in infos {
        let mut bytes = Vec::new();
        let mut sink = |b: &[u8]| {
            bytes.extend_from_slice(b);
            Ok(())
        };
        if reader.read(i, &mut sink).is_err() {
            continue;
        }
        if let Some(info) = parse_recycle_info(&bytes) {
            found.push((i, suffix, info));
        }
    }
    drop(reader);
    let mut n = 0;
    for (i, suffix, (path, _size, time)) in found {
        cat.entries[i as usize].flags |= ef::SYSTEM;
        let parent = cat.entries[i as usize].parent;
        let Some(targets) = items.get(&(parent, suffix)) else { continue };
        for &r in targets {
            let e = &mut cat.entries[r as usize];
            if e.deleted_at == 0 {
                e.deleted_at = time;
            }
            if !e.is_deleted() {
                e.flags |= ef::RECYCLED;
            }
            cat.alias.insert(r, strip_drive(&path));
            n += 1;
        }
    }
    // Everything inside a folder sent to the bin went there at the same moment.
    if !cat.alias.is_empty() {
        for i in 0..cat.entries.len() as u32 {
            if cat.entries[i as usize].deleted_at != 0 || cat.alias.contains_key(&i) {
                continue;
            }
            let chain = cat.chain(i);
            if let Some(&k) = chain.iter().rev().skip(1).find(|k| cat.alias.contains_key(k)) {
                cat.entries[i as usize].deleted_at = cat.entries[k as usize].deleted_at;
            }
        }
    }
    n
}

/// A name of a deleted file without its record.
pub(crate) struct TraceCand {
    /// Folder entry index, or NONE.
    parent: u32,
    name: String,
    dir: bool,
    size: u64,
    times: [u64; 4],
    deleted_at: u64,
}

/// One deletion from the change journal.
struct Deletion {
    frn: u64,
    parent: u64,
    time: u64,
    dir: bool,
    name: String,
}

/// Parse change journal records in `b`. Returns the bytes consumed (a record cut by the
/// end of `b` is left for the next call).
fn parse_usn(b: &[u8], out: &mut Vec<Deletion>) -> usize {
    let mut p = 0usize;
    while p + 8 <= b.len() {
        let len = u32le(b, p) as usize;
        if len == 0 {
            p += 8;
            continue;
        }
        if !len.is_multiple_of(8) || !(0x40..=0x1000).contains(&len) {
            p += 8;
            continue;
        }
        if p + len > b.len() {
            return p;
        }
        let r = &b[p..p + len];
        let major = u16le(r, 4);
        let fields = match major {
            2 => Some((u64le(r, 8), u64le(r, 16), u64le(r, 32), u32le(r, 40), u32le(r, 52), u16le(r, 56), u16le(r, 58))),
            3 if len >= 0x50 => Some((u64le(r, 8), u64le(r, 24), u64le(r, 48), u32le(r, 56), u32le(r, 68), u16le(r, 72), u16le(r, 74))),
            _ => None,
        };
        if let Some((frn, parent, time, reason, attrs, nlen, noff)) = fields
            && reason & USN_REASON_FILE_DELETE != 0
            && (noff as usize) + (nlen as usize) <= len
            && nlen > 0
            && nlen.is_multiple_of(2)
        {
            out.push(Deletion { frn, parent, time, dir: attrs & 0x10 != 0, name: utf16(&r[noff as usize..noff as usize + nlen as usize]) });
        }
        p += if major == 2 || major == 3 || major == 4 { len } else { 8 };
    }
    p
}

/// Read the change journal: date deleted entries, and return deletions whose records are
/// gone. `None` if there is no readable journal.
fn journal(vol: &Volume, cat: &mut Catalog, aux: &Aux) -> Option<(u64, Vec<TraceCand>)> {
    let (id, seq) = aux.usn_jrnl?;
    let j = aux.join(id, K_USN, seq, true);
    let (size, ..) = j.head?;
    let cs = vol.boot.cluster_size as u64;
    let mut dels = Vec::new();
    let mut slot = ReadSlot::new().ok()?;
    let mut buf = vol.src.buffer(4 << 20);
    let mut carry: Vec<u8> = Vec::new();
    let mut vcn = 0u64;
    for r in &j.runs {
        let start = vcn * cs;
        vcn += r.len;
        if !r.is_real() || start >= size {
            carry.clear();
            continue;
        }
        let bytes = (r.len * cs).min(size - start);
        let mut k = 0u64;
        while k < bytes {
            let n = (bytes - k).min(buf.len() as u64) as usize;
            let rd = n.div_ceil(vol.src.sector as usize) * vol.src.sector as usize;
            vol.src.read_robust(&mut slot, vol.base + r.lcn * cs + k, &mut buf[..rd]);
            carry.extend_from_slice(&buf[..n]);
            let used = parse_usn(&carry, &mut dels);
            carry.drain(..used);
            k += n as u64;
        }
    }
    // Latest deletion per (record, sequence).
    let mut by_rec: HashMap<u64, Vec<usize>> = HashMap::new();
    for (k, d) in dels.iter().enumerate() {
        by_rec.entry(d.frn & 0xFFFF_FFFF_FFFF).or_default().push(k);
    }
    let mut dated = 0;
    let mut used = vec![false; dels.len()];
    for e in cat.entries.iter_mut() {
        if !e.is_deleted() {
            continue;
        }
        let Some(ks) = by_rec.get(&e.id) else { continue };
        let mut best = 0u64;
        for &k in ks {
            let s = (dels[k].frn >> 48) as u16;
            if s == e.seq || s.wrapping_add(1) == e.seq {
                best = best.max(dels[k].time);
                used[k] = true;
            }
        }
        if best != 0 {
            e.deleted_at = best;
            dated += 1;
        }
    }
    let lost = dels
        .into_iter()
        .zip(used)
        .filter(|(_, u)| !u)
        .map(|(d, _)| TraceCand {
            parent: resolve_parent(&cat.entries, &aux.index, d.parent).unwrap_or(NONE),
            name: d.name,
            dir: d.dir,
            size: 0,
            times: [0; 4],
            deleted_at: d.time,
        })
        .collect();
    Some((dated, lost))
}

/// Stale `$FILE_NAME` keys in one index buffer of folder `dir_id`. `whole`: look at the
/// live entries too (the folder itself is deleted).
fn slack_keys(buf: &mut [u8], dir_id: u64, whole: bool, out: &mut Vec<(u64, TraceCand)>, parent: u32) {
    if buf.len() < 0x40 || &buf[..4] != b"INDX" || !apply_fixup(buf) {
        return;
    }
    let hdr = 0x18usize;
    let first = hdr + u32le(buf, hdr) as usize;
    let used = hdr + u32le(buf, hdr + 4) as usize;
    let alloc = (hdr + u32le(buf, hdr + 8) as usize).min(buf.len());
    let from = if whole { first } else { used }.div_ceil(8) * 8;
    let mut q = from.max(16);
    while q + 0x42 <= alloc {
        if u64le(buf, q) & 0xFFFF_FFFF_FFFF == dir_id {
            let nlen = buf[q + 0x40] as usize;
            let nsp = buf[q + 0x41];
            let end = q + 0x42 + 2 * nlen;
            let times = [u64le(buf, q + 8), u64le(buf, q + 16), u64le(buf, q + 32), u64le(buf, q + 24)];
            let ok = nlen > 0
                && nsp <= 3
                && nsp != 2
                && end <= alloc
                && (T_MIN..T_MAX).contains(&times[0])
                && (T_MIN..T_MAX).contains(&times[1])
                && buf[q + 0x42..end].as_chunks::<2>().0.iter().all(|c| {
                    let u = u16::from_le_bytes([c[0], c[1]]);
                    u >= 0x20 && !(u < 0x80 && br#"/\:*?"<>|"#.contains(&(u as u8)))
                });
            // The entry header before the key holds the file reference; trust it only
            // when the key length it gives fits the key found.
            let key_len = u16le(buf, q - 6) as usize;
            if ok && key_len == 0x42 + 2 * nlen {
                let frn = u64le(buf, q - 16);
                out.push((
                    frn,
                    TraceCand {
                        parent,
                        name: utf16(&buf[q + 0x42..end]),
                        dir: u32le(buf, q + 0x38) & 0x1000_0000 != 0,
                        size: u64le(buf, q + 0x30),
                        times,
                        deleted_at: 0,
                    },
                ));
                q = end.div_ceil(8) * 8;
                continue;
            }
        }
        q += 8;
    }
}

/// Names of removed files from the slack of every folder index.
fn index_slack(vol: &Volume, cat: &Catalog, aux: &Aux, threads: usize) -> Vec<TraceCand> {
    let cs = vol.boot.cluster_size as u64;
    let ib = vol.boot.index_record_size as usize;
    let dirs: Vec<u32> = (0..cat.entries.len() as u32)
        .filter(|&i| cat.entries[i as usize].is_dir() && cat.entries[i as usize].flags & ef::TRACE == 0)
        .collect();
    let next = AtomicUsize::new(0);
    let found: Vec<(u64, TraceCand)> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..threads.clamp(1, 8))
            .map(|_| {
                s.spawn(|| {
                    let mut out = Vec::new();
                    let Ok(mut slot) = ReadSlot::new() else { return out };
                    let mut buf = vol.src.buffer(1 << 20);
                    loop {
                        let k = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&i) = dirs.get(k) else { break };
                        let e = &cat.entries[i as usize];
                        let j = aux.join(e.id, K_I30, e.seq, !e.is_deleted());
                        let Some((size, ..)) = j.head else { continue };
                        let mut vcn = 0u64;
                        for r in &j.runs {
                            let start = vcn * cs;
                            vcn += r.len;
                            if !r.is_real() || start >= size {
                                continue;
                            }
                            let bytes = (r.len * cs).min(size - start) as usize;
                            let mut k = 0usize;
                            while k < bytes {
                                // Whole index buffers per read.
                                let n = (bytes - k).min(buf.len()) / ib * ib;
                                if n == 0 {
                                    break;
                                }
                                vol.src.read_robust(&mut slot, vol.base + r.lcn * cs + k as u64, &mut buf[..n]);
                                for b in buf[..n].chunks_exact_mut(ib) {
                                    slack_keys(b, e.id, e.is_deleted(), &mut out, i);
                                }
                                k += n;
                            }
                        }
                    }
                    out
                })
            })
            .collect();
        hs.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
    });
    found
        .into_iter()
        .filter(|(frn, _)| {
            let (rec, seq) = (frn & 0xFFFF_FFFF_FFFF, (frn >> 48) as u16);
            // Still the same file in use: a stale copy of a live entry.
            if aux.live.get(rec as usize).is_some_and(|&s| s != 0 && s == seq) {
                return false;
            }
            // A deleted record the catalog already has.
            !aux.entry_of(rec).is_some_and(|k| {
                let e = &cat.entries[k as usize];
                e.is_deleted() && (e.seq == seq || e.seq == seq.wrapping_add(1))
            })
        })
        .map(|(_, c)| c)
        .collect()
}

/// Add trace entries, one per (folder, name), skipping names the catalog already has.
fn add_traces(cat: &mut Catalog, cands: Vec<TraceCand>) -> u64 {
    if cands.is_empty() {
        return 0;
    }
    let known: HashSet<(u32, String)> = (0..cat.entries.len() as u32)
        .filter(|&i| cat.entries[i as usize].parent != NONE)
        .map(|i| (cat.entries[i as usize].parent, cat.name(i).to_lowercase()))
        .collect();
    let mut added: HashMap<(u32, String), u32> = HashMap::new();
    let mut n = 0u64;
    for c in cands {
        let key = (c.parent, c.name.to_lowercase());
        if c.parent != NONE && known.contains(&key) {
            continue;
        }
        if let Some(&k) = added.get(&key) {
            let e = &mut cat.entries[k as usize];
            e.deleted_at = e.deleted_at.max(c.deleted_at);
            if e.size == 0 {
                e.size = c.size;
            }
            if e.modified == 0 {
                [e.created, e.modified, e.accessed, e.changed] = c.times;
            }
            continue;
        }
        let (name_off, name_len) = cat.add_name(&c.name);
        let mut flags = ef::DELETED | ef::TRACE;
        if c.dir {
            flags |= ef::DIR;
        }
        if c.parent == NONE {
            flags |= ef::ORPHAN;
        } else if cat.entries[c.parent as usize].flags & ef::SYSTEM != 0 {
            flags |= ef::SYSTEM;
        }
        let k = cat.entries.len() as u32;
        cat.entries.push(Entry {
            id: TRACE_ID_BASE + n,
            flags,
            state: State::NoData,
            parent: c.parent,
            name_off,
            name_len,
            size: c.size,
            created: c.times[0],
            modified: c.times[1],
            accessed: c.times[2],
            changed: c.times[3],
            deleted_at: c.deleted_at,
            data: Data::None,
            ..Default::default()
        });
        added.insert(key, k);
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le16(v: &mut Vec<u8>, s: &str) {
        for u in s.encode_utf16() {
            v.extend_from_slice(&u.to_le_bytes());
        }
    }

    #[test]
    fn recycle_info_versions() {
        let t = 133_000_000_000_000_000u64;
        let mut v2 = Vec::new();
        v2.extend_from_slice(&2u64.to_le_bytes());
        v2.extend_from_slice(&1234u64.to_le_bytes());
        v2.extend_from_slice(&t.to_le_bytes());
        let p = r"C:\Users\Me\Desktop\report.docx";
        v2.extend_from_slice(&((p.encode_utf16().count() + 1) as u32).to_le_bytes());
        le16(&mut v2, p);
        v2.extend_from_slice(&[0, 0]);
        assert_eq!(parse_recycle_info(&v2), Some((p.to_string(), 1234, t)));

        let mut v1 = vec![0u8; 24 + 520];
        v1[..8].copy_from_slice(&1u64.to_le_bytes());
        v1[16..24].copy_from_slice(&t.to_le_bytes());
        let mut name = Vec::new();
        le16(&mut name, r"D:\old.txt");
        v1[24..24 + name.len()].copy_from_slice(&name);
        assert_eq!(parse_recycle_info(&v1).unwrap().0, r"D:\old.txt");
        assert_eq!(strip_drive(r"D:\old.txt"), r"\old.txt");
        assert_eq!(parse_recycle_info(&[0u8; 40]), None);
    }

    fn usn_v2(frn: u64, parent: u64, reason: u32, name: &str) -> Vec<u8> {
        let mut r = vec![0u8; 60];
        let mut n = Vec::new();
        le16(&mut n, name);
        let len = (60 + n.len()).div_ceil(8) * 8;
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&2u16.to_le_bytes());
        r[8..16].copy_from_slice(&frn.to_le_bytes());
        r[16..24].copy_from_slice(&parent.to_le_bytes());
        r[32..40].copy_from_slice(&133_000_000_000_000_000u64.to_le_bytes());
        r[40..44].copy_from_slice(&reason.to_le_bytes());
        r[56..58].copy_from_slice(&(n.len() as u16).to_le_bytes());
        r[58..60].copy_from_slice(&60u16.to_le_bytes());
        r.extend_from_slice(&n);
        r.resize(len, 0);
        r
    }

    #[test]
    fn journal_records_and_cut_ends() {
        let mut b = Vec::new();
        b.extend(usn_v2(100 | 3 << 48, 5, 0x100, "created.txt"));
        b.extend(usn_v2(101 | 7 << 48, 5, 0x8000_0200, "gone.txt"));
        b.extend(vec![0u8; 24]);
        let cut = usn_v2(102, 5, 0x200, "second.txt");
        b.extend_from_slice(&cut[..20]);
        let mut out = Vec::new();
        let used = parse_usn(&b, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "gone.txt");
        assert_eq!(out[0].frn, 101 | 7 << 48);
        assert_eq!(used, b.len() - 20, "the cut record waits for more bytes");
    }

    #[test]
    fn slack_finds_removed_keys() {
        // One 4096-byte INDX buffer of folder 40: one live entry, then a removed one.
        let mut b = vec![0u8; 4096];
        b[..4].copy_from_slice(b"INDX");
        b[4..6].copy_from_slice(&0x28u16.to_le_bytes());
        b[6..8].copy_from_slice(&9u16.to_le_bytes());
        let entry = |b: &mut Vec<u8>, at: usize, frn: u64, name: &str| {
            let mut n = Vec::new();
            le16(&mut n, name);
            let key_len = 0x42 + n.len();
            b[at..at + 8].copy_from_slice(&frn.to_le_bytes());
            b[at + 8..at + 10].copy_from_slice(&((16 + key_len).div_ceil(8) as u16 * 8).to_le_bytes());
            b[at + 10..at + 12].copy_from_slice(&(key_len as u16).to_le_bytes());
            let k = at + 16;
            b[k..k + 8].copy_from_slice(&(40u64 | 2 << 48).to_le_bytes());
            for t in 0..4 {
                b[k + 8 + 8 * t..k + 16 + 8 * t].copy_from_slice(&133_000_000_000_000_000u64.to_le_bytes());
            }
            b[k + 0x30..k + 0x38].copy_from_slice(&777u64.to_le_bytes());
            b[k + 0x40] = (n.len() / 2) as u8;
            b[k + 0x41] = 1;
            b[k + 0x42..k + 0x42 + n.len()].copy_from_slice(&n);
        };
        entry(&mut b, 0x40, 200 | 1 << 48, "live.txt");
        entry(&mut b, 0x100, 300 | 4 << 48, "removed.txt");
        // Index header: entries from 0x40, live part ends at 0x100, allocated to 0x1000.
        b[0x18..0x1C].copy_from_slice(&0x28u32.to_le_bytes());
        b[0x1C..0x20].copy_from_slice(&0xE8u32.to_le_bytes());
        b[0x20..0x24].copy_from_slice(&(0x1000u32 - 0x18).to_le_bytes());
        // Update sequence: value 1 at the end of every sector, originals in the array.
        b[0x28..0x2A].copy_from_slice(&1u16.to_le_bytes());
        for i in 1..9 {
            let end = i * 512 - 2;
            let orig = [b[end], b[end + 1]];
            b[0x28 + 2 * i..0x2A + 2 * i].copy_from_slice(&orig);
            b[end..end + 2].copy_from_slice(&1u16.to_le_bytes());
        }
        let mut out = Vec::new();
        slack_keys(&mut b.clone(), 40, false, &mut out, 7);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1.name, "removed.txt");
        assert_eq!(out[0].0, 300 | 4 << 48);
        assert_eq!(out[0].1.size, 777);
        let mut all = Vec::new();
        slack_keys(&mut b, 40, true, &mut all, 7);
        assert_eq!(all.len(), 2, "a deleted folder: live entries count too");
    }
}
