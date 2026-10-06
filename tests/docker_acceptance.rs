use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use ward_node_manager::PINNED_IMAGE;

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

fn test_directory() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "wardnm-docker-acceptance-{}-{}",
        std::process::id(),
        NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).expect("create test directory");
    path
}

#[test]
#[ignore = "requires Docker and the pinned task image to be available"]
fn packs_transfers_runs_and_removes_a_workspace_task() {
    let directory = test_directory();
    let source = directory.join("source");
    let snapshot = directory.join("workspace.wnm");
    let marker = "wardnm-docker-acceptance-ok";
    fs::create_dir(&source).expect("create source directory");
    fs::write(source.join("marker.txt"), marker).expect("write marker");

    let image = Command::new("docker")
        .args(["image", "inspect", PINNED_IMAGE])
        .output()
        .expect("inspect Docker image");
    assert!(
        image.status.success(),
        "pinned image is not available locally"
    );

    let pack = Command::new(env!("CARGO_BIN_EXE_wardnm"))
        .args(["snapshot", "pack"])
        .arg(&source)
        .arg(&snapshot)
        .output()
        .expect("pack workspace");
    assert!(pack.status.success());

    let run_id = format!(
        "acceptance-{}-{}",
        std::process::id(),
        NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed)
    );
    let task_id = "workspace";
    let result = Command::new(env!("CARGO_BIN_EXE_wardnm"))
        .args([
            "run",
            "--run-id",
            &run_id,
            "--task-id",
            task_id,
            "--snapshot",
        ])
        .arg(&snapshot)
        .args(["--", "cat", "marker.txt"])
        .output()
        .expect("run workspace task");
    assert!(
        result.status.success(),
        "task failed: {}",
        String::from_utf8_lossy(&result.stdout)
    );
    let result = String::from_utf8_lossy(&result.stdout);
    assert!(result.contains("\"status\":\"completed\""));
    assert!(result.contains(marker));

    let run_filter = format!("label=com.portable-node-manager.run={run_id}");
    let task_filter = format!("label=com.portable-node-manager.task={task_id}");
    let container = Command::new("docker")
        .args([
            "container",
            "ls",
            "--all",
            "--quiet",
            "--filter",
            &run_filter,
            "--filter",
            &task_filter,
        ])
        .output()
        .expect("list task containers");
    assert!(
        container.status.success(),
        "Docker container listing failed"
    );
    assert!(
        container.stdout.is_empty(),
        "task container was not removed"
    );
    fs::remove_dir_all(directory).expect("remove test directory");
}
