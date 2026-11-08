//! Deep search of NTFS free space for file records outside the MFT.
//!
//! A quick format writes a new, small MFT and frees everything else: the records of the
//! old MFT stay where they were, in what is now free space. Every free cluster is read
//! and every record-aligned `FILE` block with a valid update sequence is parsed like an
//! MFT record. Each keeps its own record number (header offset 0x2C), so the old folder
//! tree is rebuilt from them; the old root becomes the folder `<Found MFT records>`.

use std::collections::HashMap;

use fr_core::carve::{Carver, carve_into};
use fr_core::model::{Catalog, Data, Entry, FOUND_ID_BASE, NONE, State, ef};
use fr_core::sweep::sweep;

use crate::bytes::*;
use crate::mft::Volume;
use crate::scan::{Aux, K_DATA, ROOT, Raw, ScanOptions, ScanReport, Seg, Worker, build_data, count_set, parse, resolve_parent};

/// Name of the folder that holds what was under the root of the old MFT.
pub use fr_core::model::FOUND_MFT_ROOT as FOUND_ROOT;

pub(crate) fn run(vol: &Volume, cat: &mut Catalog, aux: &Aux, opts: &ScanOptions, report: &mut ScanReport) {
    let Some(bm) = aux.bitmap.as_deref() else { return };
    let rs = vol.boot.record_size as usize;
    let cs = vol.boot.cluster_size as usize;
    if cs < rs {
        report.deep_skipped = true;
        return;
    }
    let cs64 = cs as u64;
    // Parse every record found, as `existing`: records of an old MFT were in use there.
    let popts = ScanOptions { existing: true, owners: false, traces: false, ..opts.clone() };
    let mut w = Worker::default();
    let mut where_at: Vec<u64> = Vec::new();
    let mut rec = vec![0u8; rs];
    let mut carver = Carver::default();
    let want = |c: u64| count_set(bm, c, 1) == 0;
    let bad = sweep(vol.src, vol.base, vol.boot.cluster_size, vol.clusters(), &want, opts.sweep_progress.as_deref(), &mut |c, b| {
        carver.observe(c, b);
        for k in (0..cs).step_by(rs) {
            if &b[k..k + 4] != b"FILE" || u16le(b, k + 4) != 0x30 {
                continue;
            }
            rec.copy_from_slice(&b[k..k + rs]);
            let no = u32le(&rec, 0x2C) as u64;
            let before = w.raws.len();
            parse(&mut w, no, &mut rec, &popts);
            if w.raws.len() > before {
                where_at.push(c * cs64 + k as u64);
            }
        }
    });
    report.bad_bytes += bad;
    if !w.raws.is_empty() {
        found_records(vol, cat, aux, w, &where_at, report);
    }
    report.carved = carve_into(cat, vol.src, &carver, &want);
}

/// Add the records found in free space to the catalog.
fn found_records(vol: &Volume, cat: &mut Catalog, aux: &Aux, w: Worker, where_at: &[u64], report: &mut ScanReport) {
    let Some(bm) = aux.bitmap.as_deref() else { return };
    // Keep one copy per (record, sequence), and none of the records the MFT already has.
    let mut keep: Vec<(Raw, u64)> = Vec::new();
    let mut seen: HashMap<(u64, u16), ()> = HashMap::new();
    for (r, &at) in w.raws.iter().zip(where_at) {
        if seen.insert((r.id, r.seq), ()).is_some() {
            continue;
        }
        let known = aux.entry_of(r.id).is_some_and(|k| cat.entries[k as usize].seq == r.seq);
        if !known {
            keep.push((*r, at));
        }
    }
    let mut segs: Vec<Seg> = w.segs.iter().copied().filter(|g| g.kind == K_DATA).collect();
    segs.sort_unstable_by_key(|g| (g.base, g.kind, g.lowest_vcn, !g.own));
    let clusters = vol.clusters();
    let name_base = cat.names.len() as u32;
    cat.names.extend_from_slice(&w.names);

    // Folder for what was under the old root.
    let root_idx = aux.entry_of(ROOT).unwrap_or(NONE);
    let first = cat.entries.len() as u32;
    let (no, nl) = cat.add_name(FOUND_ROOT);
    cat.entries.push(Entry {
        id: FOUND_ID_BASE,
        flags: ef::DIR | ef::DELETED | ef::FOUND,
        parent: root_idx,
        name_off: no,
        name_len: nl,
        ..Default::default()
    });
    // Old record number -> new entry index, for parents among the found records.
    let mut by_rec: HashMap<u64, u32> = HashMap::new();
    let mut parents = Vec::with_capacity(keep.len());
    for (k, (r, at)) in keep.iter().enumerate() {
        let mut runs = std::mem::take(&mut cat.runs);
        let mut e = Entry {
            id: FOUND_ID_BASE + 1 + k as u64,
            seq: r.seq,
            flags: r.flags | ef::DELETED | ef::FOUND,
            attrs: r.attrs,
            parent: NONE,
            name_off: name_base + r.name_off,
            name_len: r.name_len,
            created: r.times[0],
            modified: r.times[1],
            accessed: r.times[2],
            changed: r.times[3],
            ..Default::default()
        };
        if r.dir {
            e.flags |= ef::DIR;
        } else {
            build_data(&mut e, r, &segs, &w.runs, &mut runs, vol.boot.cluster_size as u64, clusters);
        }
        cat.runs = runs;
        if let Data::Resident { off, len } = e.data {
            e.data = Data::Inline { at: *at, off, len, record_size: vol.boot.record_size };
        }
        let idx = cat.entries.len() as u32;
        by_rec.entry(r.id).or_insert(idx);
        parents.push(r.parent);
        cat.entries.push(e);
    }
    for (k, pref) in parents.into_iter().enumerate() {
        let idx = first + 1 + k as u32;
        let (pr, ps) = (pref & 0xFFFF_FFFF_FFFF, (pref >> 48) as u16);
        let parent = if pr == ROOT {
            Some(first)
        } else if let Some(&j) = by_rec.get(&pr).filter(|&&j| {
            let p = &cat.entries[j as usize];
            p.is_dir() && (p.seq == ps || p.seq == ps.wrapping_add(1))
        }) {
            Some(j)
        } else {
            resolve_parent(&cat.entries, &aux.index, pref)
        };
        match parent {
            Some(p) if p != idx => cat.entries[idx as usize].parent = p,
            _ => cat.entries[idx as usize].flags |= ef::ORPHAN,
        }
    }
    // States from the current allocation bitmap.
    for i in first + 1..cat.entries.len() as u32 {
        let e = cat.entries[i as usize];
        if e.is_dir() {
            continue;
        }
        let real = cat.real_clusters(i);
        let lost: u64 = cat.runs_of(i).iter().filter(|r| r.is_real()).map(|r| count_set(bm, r.lcn, r.len)).sum();
        let state = if e.flags & ef::ENCRYPTED != 0 {
            State::Encrypted
        } else if e.size == 0 || matches!(e.data, Data::Inline { .. }) {
            State::Good
        } else if e.data == Data::None {
            State::NoData
        } else if real > 0 && lost >= real {
            State::Overwritten
        } else if lost > 0 || e.flags & ef::INCOMPLETE != 0 {
            State::Partial
        } else {
            State::Good
        };
        cat.entries[i as usize].lost = lost;
        cat.entries[i as usize].state = state;
    }
    report.found = keep.len() as u64;
}
