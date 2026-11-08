//! Full-range search must preserve independent evidence and source-relative geometry.
mod common;
use common::*;
use fr_core::{Data, FsKind, State, ef};

fn full(image: &Image) -> (fr_core::Part, fr_scan::Scanned) {
    let part = fr_scan::pick_part_mode(&image.src, None, None, false, true).unwrap();
    let scanned =
        fr_scan::scan(&image.src, &part, &fr_scan::Request { full: true, threads: Some(1), ..Default::default() }, &Default::default())
            .unwrap();
    (part, scanned)
}

#[test]
fn parallel_pipeline_matches_serial_catalog_and_recovered_bytes() {
    use fr_ntfs::record::{attr, ns, synth::RecordBuilder};
    const BLOCK: usize = 8 << 20;
    let mut bytes = fat12(1);
    bytes.resize(3 * BLOCK + 1100, 0);
    let old = ntfs();
    bytes[2 << 20..(2 << 20) + old.len()].copy_from_slice(&old);
    let mut record = RecordBuilder::new(4096)
        .unused()
        .file_name(5, 1, "boundary.txt", ns::WIN32)
        .resident(attr::DATA, "", b"resident bytes crossing the block boundary")
        .resident(attr::DATA, "notes", b"named stream at the same boundary")
        .build(0x4567);
    put32(&mut record, 44, 12345);
    bytes[BLOCK - 512..BLOCK - 512 + record.len()].copy_from_slice(&record);
    let text = "A document with Unicode текст and emoji 😀.\n".repeat(190000);
    bytes[BLOCK + 4096..BLOCK + 4096 + text.len()].copy_from_slice(text.as_bytes());
    bytes[2 * BLOCK + (1 << 20)..2 * BLOCK + (1 << 20) + 9].copy_from_slice(b"%PDF-1.7\n");
    bytes[3 * BLOCK - 2..3 * BLOCK + 4].copy_from_slice(b"%%EOF\n");
    bytes[3 * BLOCK + 1024..].copy_from_slice(&wave());
    let mut image = Image::new("parallel-equivalence", &bytes);
    image.src.sector = 4096;
    let part = fr_scan::pick_part_mode(&image.src, None, None, true, true).unwrap();
    let mut previous: Option<fr_scan::Scanned> = None;
    for threads in [1, 2] {
        let scan = fr_scan::scan(
            &image.src,
            &part,
            &fr_scan::Request { full: true, check: true, streams: true, threads: Some(threads), ..Default::default() },
            &Default::default(),
        )
        .unwrap();
        let path = image.dir.join(format!("{threads}.frs"));
        fr_core::session::save_catalog(&path, "immutable test image", &part, &bytes[..512], &[], &scan.catalog).unwrap();
        if let Some(serial) = previous {
            assert_eq!(std::fs::read(image.dir.join("1.frs")).unwrap(), std::fs::read(path).unwrap());
            for i in 0..scan.catalog.entries.len() as u32 {
                let e = scan.catalog.entries[i as usize];
                if !e.is_dir() && e.data != Data::None {
                    assert_eq!(contents(&image.src, &scan.catalog, i), contents(&image.src, &serial.catalog, i));
                }
            }
            for name in ["boundary.txt", "boundary.txt:notes", "small.txt", "large.bin"] {
                assert!((0..scan.catalog.entries.len() as u32).any(|i| scan.catalog.name(i) == name), "missing {name}");
            }
            assert!((0..scan.catalog.entries.len() as u32).any(|i| scan.catalog.name(i).ends_with(".pdf")));
            assert!((0..scan.catalog.entries.len() as u32).any(|i| scan.catalog.name(i).ends_with(".wav")));
            break;
        }
        previous = Some(scan);
    }
    assert_eq!(std::fs::read(&image.path).unwrap(), bytes);
    image.close();
}

#[test]
fn headerless_text_crosses_windows_and_remains_a_fragment_after_loading() {
    let text = b"#include <stdio.h>\nA document line whose original file boundary is unknown.\n".repeat(120000);
    let mut bytes = vec![0; 512];
    bytes.extend_from_slice(&text);
    bytes.resize(bytes.len().div_ceil(512) * 512 + 512, 0);
    let image = Image::new("text-content", &bytes);
    let (part, scan) = full(&image);
    assert!(matches!(&scan.report, fr_scan::Report::Full { scan: fr_scan::FullReport { text_fragments: 1, .. }, .. }));
    let cat = image.roundtrip(&scan.catalog, &part);
    let i = (0..cat.entries.len() as u32).find(|&i| cat.name(i).ends_with(".c")).unwrap();
    assert_eq!(cat.entries[i as usize].state, State::Partial);
    assert_ne!(cat.entries[i as usize].flags & ef::INCOMPLETE, 0);
    assert!(cat.path(i).contains(fr_core::carve::TEXT_ROOT));
    assert_eq!(contents(&image.src, &cat, i), text);
    assert_eq!(std::fs::read(&image.path).unwrap(), bytes);
    image.close();
}

#[test]
fn full_sweep_checks_equal_a_separate_check_and_do_not_reread_the_samples() {
    let bytes = fat12(1);
    let image = Image::new("sweep-checks", &bytes);
    let (part, mut direct) = full(&image);
    let ids = (0..direct.catalog.entries.len() as u32).collect::<Vec<_>>();
    fr_core::check::check(&image.src, &mut direct.catalog, &ids, 1);
    let scan = fr_scan::scan(
        &image.src,
        &part,
        &fr_scan::Request { full: true, check: true, threads: Some(1), ..Default::default() },
        &Default::default(),
    )
    .unwrap();
    assert!(scan.checks_complete());
    assert_eq!(direct.catalog.entries.len(), scan.catalog.entries.len());
    for (a, b) in direct.catalog.entries.iter().zip(&scan.catalog.entries) {
        assert_eq!((a.id, a.size, a.state, a.flags, a.data), (b.id, b.size, b.state, b.flags, b.data));
    }
    if let fr_scan::Report::Full { scan: report, .. } = &scan.report {
        assert!(report.checks.unwrap().reused > 0);
        assert_eq!(report.timings.checking.io.calls, 0);
        assert!(report.io.calls > 0);
    } else {
        panic!("full report expected");
    }
    let notes = fr_scan::english(&scan);
    assert!(notes.iter().any(|n| n.contains("Total I/O")));
    assert!(notes.iter().any(|n| n.contains("reused from sweep")));
    image.close();
}

#[test]
fn zip_end_crossing_a_sweep_window_restores_the_entire_archive() {
    let at = 512;
    let name = b"sample.bin";
    let mut archive = vec![0; 30];
    archive[..4].copy_from_slice(b"PK\x03\x04");
    put16(&mut archive, 4, 20);
    put16(&mut archive, 6, 8); // Size follows the payload, not the local header.
    put16(&mut archive, 26, name.len() as u16);
    archive.extend_from_slice(name);
    let eocd = (8 << 20) - at - 2;
    let cd = eocd - 46 - name.len();
    let payload_len = cd - 12 - archive.len();
    archive.resize(cd - 12, 0x55);
    archive.extend_from_slice(&0u32.to_le_bytes());
    archive.extend_from_slice(&(payload_len as u32).to_le_bytes());
    archive.extend_from_slice(&(payload_len as u32).to_le_bytes());
    let mut central = vec![0; 46];
    central[..4].copy_from_slice(b"PK\x01\x02");
    put16(&mut central, 6, 20);
    put16(&mut central, 8, 8);
    put32(&mut central, 20, payload_len as u32);
    put32(&mut central, 24, payload_len as u32);
    put16(&mut central, 28, name.len() as u16);
    archive.extend_from_slice(&central);
    archive.extend_from_slice(name);
    let mut end = vec![0; 22];
    end[..4].copy_from_slice(b"PK\x05\x06");
    put16(&mut end, 8, 1);
    put16(&mut end, 10, 1);
    put32(&mut end, 12, (46 + name.len()) as u32);
    put32(&mut end, 16, cd as u32);
    archive.extend_from_slice(&end);
    let mut bytes = vec![0; at];
    bytes.extend_from_slice(&archive);
    let image = Image::new("zip-window", &bytes);
    let (_, scan) = full(&image);
    let cat = scan.catalog;
    let i = (0..cat.entries.len() as u32).find(|&i| cat.name(i).ends_with(".zip")).unwrap();
    assert_eq!(cat.entries[i as usize].flags & ef::INCOMPLETE, 0);
    assert_eq!(contents(&image.src, &cat, i), archive);
    assert_eq!(std::fs::read(&image.path).unwrap(), bytes);
    image.close();
}

#[test]
fn allocated_sector_inside_a_cluster_is_searched_without_losing_metadata() {
    let mut bytes = fat12(8);
    let root = 19 * 512;
    bytes[root..root + 11].copy_from_slice(b"HOST    BIN");
    put32(&mut bytes, root + 28, 4096);
    for off in [512, 5120] {
        bytes[off + 3..off + 5].copy_from_slice(&[0xff, 0x0f]);
    }
    let wav = wave();
    bytes[34 * 512..34 * 512 + wav.len()].copy_from_slice(&wav);
    let image = Image::new("allocated", &bytes);
    let part = fr_scan::pick_part(&image.src, None, None, false).unwrap();
    let deep = fr_scan::scan(&image.src, &part, &fr_scan::Request { deep: true, ..Default::default() }, &Default::default()).unwrap();
    assert!(!deep.catalog.entries.iter().any(|e| e.flags & ef::SIGNATURE != 0));
    let (part, scan) = full(&image);
    let cat = image.roundtrip(&scan.catalog, &part);
    let host = (0..cat.entries.len() as u32).find(|&i| cat.name(i) == "HOST.BIN").unwrap();
    assert!(!cat.entries[host as usize].is_gone());
    let found: Vec<_> = cat.entries.iter().enumerate().filter(|(_, e)| !e.is_dir() && e.flags & ef::SIGNATURE != 0).collect();
    assert_eq!(found.len(), 1);
    let (i, e) = found[0];
    assert!(e.is_gone() && !e.is_deleted());
    assert_eq!(e.state, State::Unknown);
    assert_eq!(contents(&image.src, &cat, i as u32), wav);
    assert_eq!(std::fs::read(&image.path).unwrap(), bytes);
    image.close();
}

#[test]
fn historical_fat_uses_its_own_cluster_size_and_offset() {
    let mut bytes = fat12(8);
    bytes.resize(8192 * 512, 0);
    put16(&mut bytes, 19, 8192);
    let old = fat12(1);
    let base = 512 << 10;
    bytes[base..base + old.len()].copy_from_slice(&old);
    let image = Image::new("old-fat", &bytes);
    let (part, scan) = full(&image);
    let cat = image.roundtrip(&scan.catalog, &part);
    assert_eq!(cat.fs.cluster_size, 4096);
    let i = (0..cat.entries.len() as u32)
        .find(|&i| !cat.entries[i as usize].is_dir() && cat.entries[i as usize].flags & ef::HISTORICAL != 0)
        .unwrap();
    assert_eq!(cat.entries[i as usize].state, State::Unknown);
    assert_eq!(contents(&image.src, &cat, i), vec![b'R'; 1024]);
    assert!(matches!(scan.report, fr_scan::Report::Full { scan: fr_scan::FullReport { volumes: 1, .. }, .. }));
    image.close();
}

#[test]
fn exact_signature_match_merges_but_an_overlapping_claim_does_not_hide_it() {
    for (case, declared, expected) in [("exact", 76, 0), ("overlap", 1024, 1)] {
        let mut bytes = fat12(1);
        let root = 19 * 512;
        bytes[root..root + 11].copy_from_slice(b"\xe5OUND   WAV");
        put32(&mut bytes, root + 28, declared);
        let wav = wave();
        bytes[33 * 512..33 * 512 + wav.len()].copy_from_slice(&wav);
        let image = Image::new(case, &bytes);
        let (part, scan) = full(&image);
        let cat = image.roundtrip(&scan.catalog, &part);
        assert!(matches!(scan.report, fr_scan::Report::Full { scan: fr_scan::FullReport { carved, .. }, .. } if carved == expected));
        let i = (0..cat.entries.len() as u32)
            .find(|&i| {
                !cat.entries[i as usize].is_dir()
                    && cat.entries[i as usize].size == wav.len() as u64
                    && cat.entries[i as usize].flags & ef::SIGNATURE != 0
            })
            .unwrap();
        assert_eq!(contents(&image.src, &cat, i), wav);
        image.close();
    }
}

#[test]
fn loose_resident_record_preserves_record_size_across_sweep_windows_and_sessions() {
    use fr_ntfs::record::{attr, ns, synth::RecordBuilder};
    let mut bytes = vec![0; (8 << 20) + 8192];
    let at = (8 << 20) - 512;
    let mut record = RecordBuilder::new(4096)
        .unused()
        .seq(7)
        .file_name(5, 1, "resident.txt", ns::WIN32)
        .resident(attr::DATA, "", b"surviving resident contents")
        .resident(attr::DATA, "notes", b"surviving named stream")
        .build(0x1234);
    put32(&mut record, 44, 10024);
    bytes[at..at + record.len()].copy_from_slice(&record);
    let mut image = Image::new("resident", &bytes);
    image.src.sector = 4096;
    let part = fr_scan::pick_part_mode(&image.src, None, None, false, true).unwrap();
    let scan = fr_scan::scan(&image.src, &part, &fr_scan::Request { full: true, streams: true, ..Default::default() }, &Default::default())
        .unwrap();
    assert_eq!(part.fs, FsKind::Unknown);
    let cat = image.roundtrip(&scan.catalog, &part);
    let i = (0..cat.entries.len() as u32).find(|&i| cat.name(i) == "resident.txt").unwrap();
    assert!(matches!(cat.entries[i as usize].data, Data::Inline { record_size: 4096, .. }));
    assert_eq!(contents(&image.src, &cat, i), b"surviving resident contents");
    let stream = (0..cat.entries.len() as u32).find(|&i| cat.name(i) == "resident.txt:notes").unwrap();
    assert_eq!(contents(&image.src, &cat, stream), b"surviving named stream");
    image.close();
}

#[test]
fn overlapping_signatures_and_4kn_unaligned_starts_are_retained() {
    let mut bytes = vec![0; 2 << 20];
    let at = 512;
    bytes[at..at + 2].copy_from_slice(b"BM");
    put32(&mut bytes, at + 2, 2048);
    put32(&mut bytes, at + 10, 54);
    put32(&mut bytes, at + 14, 40);
    let wav = wave();
    bytes[at + 512..at + 512 + wav.len()].copy_from_slice(&wav);
    let mut image = Image::new("nested", &bytes);
    image.src.sector = 4096;
    let (part, scan) = full(&image);
    let cat = image.roundtrip(&scan.catalog, &part);
    let files: Vec<_> = cat.entries.iter().enumerate().filter(|(_, e)| !e.is_dir()).collect();
    assert_eq!(files.len(), 2);
    for (i, e) in files {
        let expected = if e.size == 2048 { &bytes[at..at + 2048] } else { &wav };
        assert_eq!(contents(&image.src, &cat, i as u32), expected);
    }
    image.close();
}

#[test]
fn cancellation_stops_full_search_and_explicit_offsets_stay_bounded() {
    let image = Image::new("cancel", &vec![0; 2 << 20]);
    let part = fr_scan::pick_part_mode(&image.src, None, Some(512), false, true).unwrap();
    assert_eq!(part.offset, 512);
    assert_eq!(part.size, (2 << 20) - 512);
    let control = std::sync::Arc::new(fr_core::control::Control::default());
    control.cancel();
    let src = image.src.try_clone().unwrap().with_control(control);
    assert!(fr_scan::scan(&src, &part, &fr_scan::Request { full: true, ..Default::default() }, &Default::default()).is_err());
    drop(src);
    image.close();
}

#[test]
fn old_ntfs_backup_boot_and_fragmented_resident_record_restore_with_own_geometry() {
    let mut bytes = fat12(8);
    bytes.resize(8192 * 512, 0);
    put16(&mut bytes, 19, 8192);
    let base = 512 << 10;
    let mut old = ntfs();
    old[..512].fill(0); // Only the backup boot sector survives the format.
    bytes[base..base + old.len()].copy_from_slice(&old);
    let image = Image::new("old-ntfs", &bytes);
    let (part, scan) = full(&image);
    let cat = image.roundtrip(&scan.catalog, &part);
    let mut checked = 0;
    for i in 0..cat.entries.len() as u32 {
        if cat.name(i) == "small.txt" {
            assert!(matches!(cat.entries[i as usize].data, Data::RecordRuns { record_size: 4096, .. }));
            assert_eq!(contents(&image.src, &cat, i), b"fragmented MFT resident bytes");
            checked += 1;
        } else if cat.name(i) == "large.bin" {
            assert_eq!(contents(&image.src, &cat, i), vec![0x5a; 2000]);
            checked += 1;
        }
    }
    assert_eq!(checked, 2);
    assert!(matches!(scan.report, fr_scan::Report::Full { scan: fr_scan::FullReport { volumes: 1, .. }, .. }));
    image.close();
}

#[test]
fn a_current_volume_imported_from_raw_retains_its_deletion_flags() {
    let bytes = fat12(1);
    let image = Image::new("current-raw", &bytes);
    let part = fr_scan::pick_part_mode(&image.src, None, None, true, true).unwrap();
    let scan = fr_scan::scan(&image.src, &part, &fr_scan::Request { full: true, ..Default::default() }, &Default::default()).unwrap();
    let cat = image.roundtrip(&scan.catalog, &part);
    let i = (0..cat.entries.len() as u32).find(|&i| !cat.entries[i as usize].is_dir()).unwrap();
    assert!(cat.entries[i as usize].is_deleted());
    assert_eq!(cat.entries[i as usize].flags & ef::HISTORICAL, 0);
    assert_eq!(contents(&image.src, &cat, i), vec![b'R'; 1024]);
    image.close();
}

#[test]
fn partial_last_sector_is_recovered_without_reading_beyond_the_selected_range() {
    let mut bytes = vec![0; 1024];
    let wav = wave();
    bytes.extend_from_slice(&wav);
    let mut image = Image::new("partial-sector", &bytes);
    image.src.sector = 4096;
    let (part, scan) = full(&image);
    let cat = image.roundtrip(&scan.catalog, &part);
    let i = cat.entries.iter().position(|e| !e.is_dir()).unwrap() as u32;
    assert_eq!(contents(&image.src, &cat, i), wav);
    assert!(matches!(scan.report, fr_scan::Report::Full { scan: fr_scan::FullReport { bad_bytes: 0, bytes: 1100, .. }, .. }));
    image.close();
}

#[test]
fn a_different_record_geometry_inside_the_current_mft_is_not_suppressed() {
    use fr_ntfs::record::{attr, ns, synth::RecordBuilder};
    let mut bytes = ntfs();
    let at = 512 * 1024 + (29 * 4096 + 512 - 97 * 1024);
    let mut rec = RecordBuilder::new(1024)
        .unused()
        .file_name(5, 1, "embedded.txt", ns::WIN32)
        .resident(attr::DATA, "", b"old record inside current MFT")
        .build(0x7890);
    put32(&mut rec, 44, 90324);
    bytes[at..at + rec.len()].copy_from_slice(&rec);
    let image = Image::new("old-record-in-mft", &bytes);
    let (part, scan) = full(&image);
    let cat = image.roundtrip(&scan.catalog, &part);
    let i = (0..cat.entries.len() as u32).find(|&i| cat.name(i) == "embedded.txt").unwrap();
    assert!(matches!(cat.entries[i as usize].data, Data::Inline { record_size: 1024, .. }));
    assert_eq!(contents(&image.src, &cat, i), b"old record inside current MFT");
    image.close();
}

#[test]
fn mirrored_system_records_obey_the_system_file_filter() {
    let mut bytes = ntfs();
    let mirror = bytes[32 * 1024..36 * 1024].to_vec();
    bytes[4 * 1024..8 * 1024].copy_from_slice(&mirror);
    let image = Image::new("mirror", &bytes);
    let (_, scan) = full(&image);
    let cat = scan.catalog;
    let mirror =
        (0..cat.entries.len() as u32).find(|&i| cat.name(i) == "$MFT" && cat.entries[i as usize].flags & ef::HISTORICAL != 0).unwrap();
    assert!(cat.entries[mirror as usize].flags & ef::SYSTEM != 0);
    assert!(!fr_core::filter::Filter::default().accepts(&cat, mirror));
    assert!(fr_core::filter::Filter { system: true, ..Default::default() }.accepts(&cat, mirror));
    image.close();
}

#[test]
fn valid_boot_geometry_is_used_when_the_mft_runlist_cannot_be_read() {
    let mut bytes = ntfs();
    bytes[32 * 1024..36 * 1024].fill(0);
    let image = Image::new("missing-mft-zero", &bytes);
    let (part, scan) = full(&image);
    assert!(!scan.warnings().is_empty());
    let cat = image.roundtrip(&scan.catalog, &part);
    assert_eq!(cat.fs.cluster_size, 1024);
    let i = (0..cat.entries.len() as u32).find(|&i| cat.name(i) == "large.bin").unwrap();
    assert_eq!(cat.entries[i as usize].state, State::Unknown);
    assert_eq!(contents(&image.src, &cat, i), vec![0x5a; 2000]);
    image.close();
}

#[test]
fn forged_mft_size_is_rejected_before_planning_reads() {
    use fr_ntfs::{
        record::{attr, ns, synth::RecordBuilder},
        runlist::Extent,
    };
    let mut bytes = ntfs();
    let record = RecordBuilder::new(4096)
        .file_name(5, 1, "$MFT", ns::WIN32)
        .non_resident(attr::DATA, "", 0, 0, 128 << 10, u64::MAX, None, &[Extent { vcn: 0, len: 128, lcn: Some(32) }])
        .build(3);
    bytes[32 * 1024..36 * 1024].copy_from_slice(&record);
    let image = Image::new("forged-mft-size", &bytes);
    let (_, scan) = full(&image);
    assert!(scan.warnings().iter().any(|e| e.to_string().contains("$MFT data lies outside the volume")));
    assert!(scan.catalog.entries.iter().any(|e| e.flags & ef::HISTORICAL != 0));
    image.close();
}
