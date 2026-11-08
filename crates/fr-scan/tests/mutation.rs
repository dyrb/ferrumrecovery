//! Bounded, reproducible mutation fuzzing. No disks are opened by this campaign.
use fr_core::{carve::formats, lznt1, part};

fn next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn boot_seeds() -> [Vec<u8>; 3] {
    let mut ntfs = vec![0; 512];
    ntfs[3..11].copy_from_slice(b"NTFS    ");
    ntfs[11..13].copy_from_slice(&512u16.to_le_bytes());
    ntfs[13] = 8;
    ntfs[0x28..0x30].copy_from_slice(&262144u64.to_le_bytes());
    ntfs[0x30..0x38].copy_from_slice(&4u64.to_le_bytes());
    ntfs[0x38..0x40].copy_from_slice(&8u64.to_le_bytes());
    ntfs[0x40] = (-10i8) as u8;
    ntfs[0x44] = (-12i8) as u8;
    let mut fat = vec![0; 512];
    fat[11..13].copy_from_slice(&512u16.to_le_bytes());
    fat[13] = 1;
    fat[14..16].copy_from_slice(&32u16.to_le_bytes());
    fat[16] = 2;
    fat[32..36].copy_from_slice(&262144u32.to_le_bytes());
    fat[36..40].copy_from_slice(&2048u32.to_le_bytes());
    fat[44..48].copy_from_slice(&2u32.to_le_bytes());
    let mut exfat = vec![0; 512];
    exfat[3..11].copy_from_slice(b"EXFAT   ");
    exfat[0x48..0x50].copy_from_slice(&263168u64.to_le_bytes());
    for (at, value) in [(0x50, 24u32), (0x54, 257), (0x58, 1024), (0x5c, 32768), (0x60, 2)] {
        exfat[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    exfat[0x6c] = 9;
    exfat[0x6d] = 3;
    exfat[0x6e] = 1;
    [ntfs, fat, exfat]
}

#[test]
fn reject_impossible_fat_geometry_before_allocating_tables() {
    let [_, mut fat, mut exfat] = boot_seeds();
    assert!(fr_fat::fat::parse_boot(&fat).is_some());
    assert!(fr_fat::exfat::parse_boot(&exfat).is_some());
    // A large table/heap cannot fit in the advertised small volume.
    exfat[0x5c..0x60].copy_from_slice(&0x7fff_ffffu32.to_le_bytes());
    exfat[0x54..0x58].copy_from_slice(&0x1000_0001u32.to_le_bytes());
    assert!(fr_fat::exfat::parse_boot(&exfat).is_none());
    // Conversely, this table cannot address the advertised data clusters.
    fat[36..40].copy_from_slice(&1u32.to_le_bytes());
    assert!(fr_fat::fat::parse_boot(&fat).is_none());
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../../target/validation/truncated-fat-{}.img", std::process::id()));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    for boot in boot_seeds().iter().skip(1) {
        std::fs::write(&path, boot).unwrap();
        let source = fr_core::Source::open(path.to_str().unwrap()).unwrap();
        let kind = if boot[3..11] == *b"EXFAT   " { fr_core::FsKind::ExFat } else { fr_core::FsKind::Fat32 };
        assert!(fr_fat::scan(&source, 0, kind, &Default::default()).is_err());
    }
    std::fs::remove_file(path).unwrap();
}

#[test]
fn malformed_signature_lists_never_panic() {
    for s in ["x €€", "x FFFF 18446744073709551615", "x 0000 18446744073709551614", "x abCD 0 0", "x 0000 512"] {
        assert!(std::panic::catch_unwind(|| formats::parse_user_signatures(s)).is_ok(), "{s:?}");
    }
}

#[test]
fn mutate_metadata_parsers() {
    let iterations = std::env::var("FR_FUZZ_CASES").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(10_000);
    let mut random = 0xB954_7682_238A_17CD;
    let mut seed = vec![0u8; 1024];
    seed[..4].copy_from_slice(b"FILE");
    seed[4..6].copy_from_slice(&48u16.to_le_bytes());
    seed[6..8].copy_from_slice(&3u16.to_le_bytes());
    seed[0x14..0x16].copy_from_slice(&56u16.to_le_bytes());
    seed[0x18..0x1c].copy_from_slice(&64u32.to_le_bytes());
    seed[48..50].copy_from_slice(&0x1234u16.to_le_bytes());
    seed[56..60].fill(255);
    seed[510..512].copy_from_slice(&0x1234u16.to_le_bytes());
    seed[1022..1024].copy_from_slice(&0x1234u16.to_le_bytes());
    let [ntfs, fat, exfat] = boot_seeds();
    let seeds = [seed, ntfs, fat, exfat];
    for case in 0..iterations {
        let n = next(&mut random) as usize % 4097;
        let mut b = if case % 3 == 0 { seeds[case / 3 % seeds.len()].clone() } else { vec![0; n] };
        if case % 3 == 1 {
            for byte in &mut b {
                *byte = next(&mut random) as u8;
            }
        }
        for _ in 0..1 + next(&mut random) % 16 {
            if !b.is_empty() {
                let at = next(&mut random) as usize % b.len();
                b[at] ^= next(&mut random) as u8;
            }
        }
        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = fr_ntfs::boot::BootSector::parse(&b);
            let _ = fr_fat::fat::parse_boot(&b);
            let _ = fr_fat::exfat::parse_boot(&b);
            let _ = part::boot_size(&b);
            let _ = fr_ntfs::runlist::decode(&b, next(&mut random), &mut Vec::new());
            let _ = fr_ntfs::runlist::allocated_clusters(&b);
            let _ = fr_ntfs::record::parse_attribute_list(&b);
            let _ = fr_fat::fat::parse_dir(&b, true);
            let _ = fr_fat::exfat::parse_dir(&b, &mut Default::default());
            let _ = formats::detect(&b);
            let _ = lznt1::decompress(&b, &mut [0; 8192]);
            if fr_ntfs::record::fixup_record(&mut b).is_ok() {
                let _ = fr_ntfs::record::attributes(&b);
            }
        }));
        assert!(run.is_ok(), "mutation case {case}, reproduce with FR_FUZZ_CASES={}", case + 1);
    }
    eprintln!("mutation fuzz: {iterations} inputs, 12 parser paths; seed B9547682238A17CD");
}

#[test]
fn mutate_carved_file_parsers() {
    use fr_core::{
        Source,
        carve::{Fmt, stream::Stream},
    };
    use std::io::{Seek, Write};
    let formats: &[(Fmt, &[u8])] = &[
        (Fmt::Jpeg, b"\xff\xd8\xff\xe1"),
        (Fmt::Png, b"\x89PNG\r\n\x1a\n"),
        (Fmt::Gif, b"GIF89a"),
        (Fmt::Bmp, b"BM"),
        (Fmt::Tiff, b"II\x2a\0"),
        (Fmt::Riff, b"RIFF"),
        (Fmt::Bmff, b"\0\0\0\x18ftypisom"),
        (Fmt::Ebml, b"\x1a\x45\xdf\xa3"),
        (Fmt::Mp3, b"ID3"),
        (Fmt::Flac, b"fLaC"),
        (Fmt::Ogg, b"OggS"),
        (Fmt::Pdf, b"%PDF-1.7"),
        (Fmt::Zip, b"PK\x03\x04"),
        (Fmt::Ole, b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1"),
        (Fmt::Rtf, b"{\\rtf1"),
        (Fmt::Rar, b"Rar!\x1a\x07\0"),
        (Fmt::SevenZip, b"7z\xbc\xaf\x27\x1c"),
        (Fmt::Gzip, b"\x1f\x8b\x08"),
        (Fmt::Sqlite, b"SQLite format 3\0"),
        (Fmt::Psd, b"8BPS"),
        (Fmt::Pe, b"MZ"),
        (Fmt::Xml, b"<?xml"),
        (Fmt::Html, b"<!DOCTYPE html>"),
        (Fmt::Cab, b"MSCF"),
        (Fmt::Pcx, b"\x0a\x05\x01\x08"),
        (Fmt::Dcx, b"\xb1\x68\xde\x3a"),
        (Fmt::Ico, b"\0\0\x01\0\x01\0"),
        (Fmt::Tga, b"\0\0\x02\0"),
        (Fmt::Midi, b"MThd"),
        (Fmt::Aiff, b"FORM"),
        (Fmt::Sfnt, b"\0\x01\0\0"),
        (Fmt::Swf, b"FWS"),
        (Fmt::Flv, b"FLV\x01"),
        (Fmt::EaVp6, b"MVhd"),
    ];
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/validation/fuzz");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("formats-{}.img", std::process::id()));
    let mut file = std::fs::File::create(&path).unwrap();
    file.set_len(16384).unwrap();
    let source = Source::open(path.to_str().unwrap()).unwrap();
    let mut random = 0xCA67_9874_0AB3_1191;
    for case in 0..2800 {
        let (fmt, magic) = formats[case % formats.len()];
        let mut bytes = vec![0; 16384];
        if case % 3 != 0 {
            for byte in &mut bytes {
                *byte = next(&mut random) as u8;
            }
        }
        bytes[..magic.len()].copy_from_slice(magic);
        for _ in 0..16 {
            let at = magic.len() + next(&mut random) as usize % (512 - magic.len());
            bytes[at] ^= next(&mut random) as u8;
        }
        file.rewind().unwrap();
        file.write_all(&bytes).unwrap();
        file.flush().unwrap();
        let run = std::panic::catch_unwind(|| {
            let mut stream = Stream::new(&source, 0, bytes.len() as u64).unwrap();
            let len = formats::length(fmt, &mut stream).filter(|&n| n <= bytes.len() as u64).unwrap_or(bytes.len() as u64);
            let _ = formats::extension(fmt, &mut stream);
            let _ = formats::meta_name(fmt, &mut stream, len);
        });
        assert!(run.is_ok(), "file-format mutation case {case}, {fmt:?}");
    }
    drop(source);
    drop(file);
    std::fs::remove_file(path).unwrap();
}
