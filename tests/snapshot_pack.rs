use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use ward_node_manager::workspace_snapshot::{decode, WorkspaceFile};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

fn test_directory() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "wardnm-snapshot-pack-{}-{}",
        std::process::id(),
        NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).expect("create test directory");
    path
}

#[test]
fn packs_a_directory_into_a_wnm1_snapshot() {
    let directory = test_directory();
    let source = directory.join("source");
    let output = directory.join("workspace.wnm");
    fs::create_dir_all(source.join("src")).expect("create source directory");
    fs::write(source.join("src/main.rs"), b"fn main() {}").expect("write source file");

    let result = Command::new(env!("CARGO_BIN_EXE_wardnm"))
        .args(["snapshot", "pack"])
        .arg(&source)
        .arg(&output)
        .output()
        .expect("run wardnm");

    assert!(result.status.success());
    assert_eq!(
        decode(&fs::read(output).expect("read packed snapshot")).expect("decode snapshot"),
        [WorkspaceFile {
            path: "src/main.rs".into(),
            contents: b"fn main() {}".to_vec(),
        }]
    );
    fs::remove_dir_all(directory).expect("remove test directory");
}

#[test]
fn refuses_to_overwrite_an_existing_snapshot() {
    let directory = test_directory();
    let source = directory.join("source");
    let output = directory.join("workspace.wnm");
    fs::create_dir(&source).expect("create source directory");
    fs::write(&output, b"preserve").expect("write existing output");

    let result = Command::new(env!("CARGO_BIN_EXE_wardnm"))
        .args(["snapshot", "pack"])
        .arg(&source)
        .arg(&output)
        .output()
        .expect("run wardnm");

    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("snapshot_output_unavailable"));
    assert_eq!(
        fs::read(&output).expect("read existing output"),
        b"preserve"
    );
    fs::remove_dir_all(directory).expect("remove test directory");
}

#[test]
fn rejects_an_unavailable_source_without_creating_a_snapshot() {
    let directory = test_directory();
    let output = directory.join("workspace.wnm");

    let result = Command::new(env!("CARGO_BIN_EXE_wardnm"))
        .args(["snapshot", "pack"])
        .arg(directory.join("missing"))
        .arg(&output)
        .output()
        .expect("run wardnm");

    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stdout).contains("workspace_snapshot_invalid"));
    assert!(!output.exists());
    fs::remove_dir_all(directory).expect("remove test directory");
}
