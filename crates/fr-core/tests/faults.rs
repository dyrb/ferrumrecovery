//! Recovery failure paths on disposable images and an explicitly supplied test VHD.
use fr_core::{Catalog, Data, Entry, FsInfo, Run, Source, ef};
use fr_core::{control::Control, data::Reader, image, recover};
use std::{fs, io, path::PathBuf, sync::Arc};

fn directory(tag: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/validation/faults");
    let dir = root.join(format!("{}-{tag}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn catalog(size: u64) -> Catalog {
    let fs = FsInfo {
        kind: fr_core::FsKind::Ntfs,
        offset: 0,
        size,
        cluster_size: 512,
        record_size: 1024,
        mft: vec![],
        serial: 0,
        root: 5,
        data_offset: 0,
    };
    let entry = Entry {
        id: 40,
        flags: ef::DELETED,
        parent: fr_core::NONE,
        size,
        valid: size,
        name_len: 8,
        data: Data::Runs { start: 0, count: 1 },
        ..Default::default()
    };
    Catalog::new(fs, vec![entry], b"data.bin".to_vec(), vec![Run { lcn: 0, len: size.div_ceil(512) }])
}

#[test]
fn cancel_reader_during_a_file_stops_delivery() {
    let dir = directory("cancel");
    let path = dir.join("source.img");
    fs::write(&path, vec![17; 8 << 20]).unwrap();
    let control = Arc::new(Control::default());
    let source = Source::open(path.to_str().unwrap()).unwrap().with_control(control.clone());
    let cat = catalog(8 << 20);
    let mut reader = Reader::new(&source, &cat).unwrap();
    let mut count = 0;
    let result = reader.read(0, &mut |b| {
        count += b.len();
        control.cancel();
        Ok(())
    });
    assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
    assert!(count < 8 << 20);
    drop(reader);
    drop(source);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn source_truncation_is_reported_as_unreadable_data() {
    let dir = directory("truncate");
    let path = dir.join("source.img");
    fs::write(&path, vec![23; 8 << 20]).unwrap();
    let source = Source::open(path.to_str().unwrap()).unwrap();
    let cat = catalog(8 << 20);
    let mut reader = Reader::new(&source, &cat).unwrap();
    let mut delivered = 0;
    let stats = reader
        .read(0, &mut |b| {
            if delivered == 0 {
                assert!(b.iter().all(|&v| v == 23));
                fs::OpenOptions::new().write(true).open(&path)?.set_len(4 << 20)?;
            } else {
                assert!(b.iter().all(|&v| v == 0));
            }
            delivered += b.len();
            Ok(())
        })
        .unwrap();
    assert_eq!(delivered, 8 << 20);
    assert_eq!(stats.bad, 4 << 20);
    drop(reader);
    drop(source);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn sink_failure_stops_delivery_and_imaging_resumes_after_cancellation() {
    let dir = directory("sink-image");
    let path = dir.join("source.img");
    let bytes = vec![29; 8 << 20];
    fs::write(&path, &bytes).unwrap();
    let source = Source::open(path.to_str().unwrap()).unwrap();
    let cat = catalog(8 << 20);
    let mut reader = Reader::new(&source, &cat).unwrap();
    let mut calls = 0;
    let error = reader
        .read(0, &mut |_| {
            calls += 1;
            Err(io::Error::from_raw_os_error(112))
        })
        .unwrap_err();
    assert_eq!(error.raw_os_error(), Some(112));
    assert_eq!(calls, 1);
    let stop = std::cell::Cell::new(false);
    let out = dir.join("copy.img");
    let map = dir.join("copy.map");
    let partial = image::image(
        &source,
        0,
        bytes.len() as u64,
        &out,
        &map,
        &mut |p| {
            if p.copied > 0 {
                stop.set(true);
            }
        },
        &|| stop.get(),
    )
    .unwrap();
    assert!(partial.bytes(image::Status::Copied) < bytes.len() as u64);
    image::image(&source, 0, bytes.len() as u64, &out, &map, &mut |_| {}, &|| false).unwrap();
    assert_eq!(fs::read(out).unwrap(), bytes);
    drop(reader);
    drop(source);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "requires FR_FULL_TARGET on the verified disposable destination VHD"]
fn destination_filling_after_preflight_reports_failure_and_removes_partial_file() {
    let target_dir = PathBuf::from(std::env::var("FR_FULL_TARGET").expect("explicit test target"));
    assert_eq!(target_dir, PathBuf::from(r"S:\recovered"));
    let fixture = PathBuf::from(std::env::var("FR_FULL_IMAGE").expect("explicit disposable VHD"));
    let guard = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/assert-validation-drive.ps1");
    let valid = std::process::Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(guard)
        .arg("-Image")
        .arg(fixture)
        .args(["-Letter", "S", "-Label", "FRFAULT"])
        .output()
        .unwrap();
    assert!(valid.status.success(), "{}", String::from_utf8_lossy(&valid.stderr));
    let dir = directory("disk-full");
    let input = dir.join("source.img");
    fs::write(&input, vec![31; 8 << 20]).unwrap();
    let source = Source::open(input.to_str().unwrap()).unwrap();
    fs::create_dir_all(&target_dir).unwrap();
    let target = recover::check_target(&source, 0, source.size, &target_dir).unwrap();
    assert!(target.free > 8 << 20);
    // SetEndOfFile alone may reserve no clusters: write bytes to consume real space.
    use io::Write;
    let mut filler = fs::File::create_new(r"S:\filler.bin").unwrap();
    let block = [1u8; 64 << 10];
    loop {
        match filler.write_all(&block) {
            Ok(()) => {}
            Err(e) => {
                assert_eq!(e.raw_os_error(), Some(112));
                break;
            }
        }
    }
    // Leave enough room to create a file, but not enough to write its contents.
    filler.set_len(filler.metadata().unwrap().len() - (128 << 10)).unwrap();
    drop(filler);
    let remaining = recover::check_target(&source, 0, source.size, &target_dir).unwrap();
    assert!(remaining.free > 0 && remaining.free < 8 << 20);
    let cat = catalog(8 << 20);
    let mut results = Vec::new();
    recover::recover(&source, &cat, &[0], &target, &recover::Options { structure: false }, &mut |r| {
        results.push((r.outcome.clone(), r.path.clone()))
    });
    assert_eq!(results.len(), 1);
    assert!(matches!(results[0].0, recover::Outcome::Failed(_)));
    assert!(!results[0].1.exists());
    drop(target);
    drop(source);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
#[ignore = "requires FR_DISCONNECT_IMAGE mounted read-only as R: by the fixture script"]
fn detached_virtual_source_returns_an_error_without_hanging() {
    let path = PathBuf::from(std::env::var("FR_DISCONNECT_IMAGE").expect("explicit fixture image"));
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/validation").canonicalize().unwrap();
    assert!(path.canonicalize().unwrap().starts_with(root));
    let source = Source::open("R:").unwrap();
    assert!(source.read_bytes(0, 512).is_ok());
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/detach-validation.ps1");
    let result = std::process::Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(script)
        .arg("-Image")
        .arg(path)
        .output()
        .unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert!(source.read_bytes(0, 512).is_err());
}
