//! A complete offline scan/save/load/recover path without mounting a disk.
use fr_core::{Source, ef};
use std::path::Path;

mod common;

#[test]
fn scan_session_and_recovery_preserve_source_and_contents() {
    let dir = std::env::temp_dir().join(format!("fr-workflow-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("disk.img");
    let session = dir.join("scan.frs");
    let output = dir.join("files");
    let original = common::fat12(1);
    std::fs::write(&input, &original).unwrap();
    let src = Source::open(input.to_str().unwrap()).unwrap();
    let part = fr_scan::pick_part(&src, None, None, false).unwrap();
    let scan = fr_scan::scan(&src, &part, &Default::default(), &Default::default()).unwrap();
    let files: Vec<_> =
        scan.catalog.entries.iter().enumerate().filter(|(_, e)| e.flags & ef::DELETED != 0 && !e.is_dir()).map(|(i, _)| i as u32).collect();
    assert_eq!(files.len(), 1);
    fr_core::session::save_catalog(&session, input.to_str().unwrap(), &part, &original[..512], &[], &scan.catalog).unwrap();
    let (source, part, loaded) = fr_scan::load_session(&session).unwrap();
    let target = fr_core::recover::check_target(&source, part.offset, part.size, &output).unwrap();
    let mut outcomes = vec![];
    fr_core::recover::recover(&source, &loaded.catalog, &files, &target, &fr_core::recover::Options { structure: false }, &mut |r| {
        outcomes.push((r.path.clone(), r.outcome.clone()))
    });
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].1, fr_core::recover::Outcome::Ok);
    assert_eq!(std::fs::read(&outcomes[0].0).unwrap(), vec![b'R'; 1024]);
    assert_eq!(std::fs::read(&input).unwrap(), original);
    let mut reader = fr_core::data::Reader::new(&source, &loaded.catalog).unwrap();
    let mut prefix = vec![];
    let stats = reader
        .read_limit(files[0], 17, &mut |b| {
            prefix.extend_from_slice(b);
            Ok(())
        })
        .unwrap();
    assert_eq!(prefix, vec![b'R'; 17]);
    assert_eq!(stats.bad, 0);
    drop(reader);
    drop(target);
    drop(source);
    drop(src);
    std::fs::remove_dir_all(Path::new(&dir)).unwrap();
}
