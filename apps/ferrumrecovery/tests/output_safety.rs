#[test]
fn csv_cannot_truncate_its_source_image() {
    let dir = std::env::temp_dir().join(format!("fr-cli-output-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("source.img");
    let bytes = vec![0u8; 512];
    std::fs::write(&source, &bytes).unwrap();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_ferrumrecovery"))
        .args(["scan", source.to_str().unwrap(), "--raw", "--csv", source.to_str().unwrap()])
        .output()
        .unwrap();
    let after = std::fs::read(&source).unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    assert!(!result.status.success());
    assert_eq!(after, bytes);
}

#[test]
fn imaging_accepts_an_explicit_destination_flag() {
    let dir = std::env::temp_dir().join(format!("fr-cli-image-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("source.img");
    let output = dir.join("copy.img");
    std::fs::write(&source, [19u8; 512]).unwrap();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_ferrumrecovery"))
        .args(["image", source.to_str().unwrap(), "--to", output.to_str().unwrap()])
        .output()
        .unwrap();
    let copied = std::fs::read(&output);
    std::fs::remove_dir_all(dir).unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert_eq!(copied.unwrap(), [19u8; 512]);
}
