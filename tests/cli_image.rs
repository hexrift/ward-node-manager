#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::Command;

const TOOLCHAIN_IMAGE: &str =
    "registry.example/toolchain@sha256:1111111111111111111111111111111111111111111111111111111111111111";

fn fake_docker(name: &str) -> (PathBuf, PathBuf) {
    let directory =
        std::env::temp_dir().join(format!("wardnm-cli-image-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).expect("create fake docker directory");
    let log = directory.join("docker.log");
    let docker = directory.join("docker");
    fs::write(
        &docker,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 0\n",
            log.display()
        ),
    )
    .expect("write fake docker");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&docker, fs::Permissions::from_mode(0o755))
            .expect("make fake docker executable");
    }
    (directory, log)
}

fn wardnm(path: &PathBuf, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_wardnm"))
        .env("PATH", path)
        .args(args)
        .output()
        .expect("run wardnm")
}

#[test]
fn runs_the_task_in_an_operator_chosen_image_pinned_by_digest() {
    let (directory, log) = fake_docker("pinned");
    wardnm(
        &directory,
        &[
            "run",
            "--task-id",
            "task-1",
            "--image",
            TOOLCHAIN_IMAGE,
            "--",
            "true",
        ],
    );

    let calls = fs::read_to_string(&log).expect("docker was called");
    let run = calls
        .lines()
        .find(|line| line.contains(" run --rm "))
        .expect("docker run was called");
    assert!(run.split(' ').any(|argument| argument == TOOLCHAIN_IMAGE));
    let _ = fs::remove_dir_all(directory);
}

#[test]
fn refuses_an_image_without_a_digest_before_docker_is_called() {
    let (directory, log) = fake_docker("mutable");
    let output = wardnm(
        &directory,
        &[
            "run",
            "--task-id",
            "task-1",
            "--image",
            "registry.example/toolchain:latest",
            "--",
            "true",
        ],
    );

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "{\"status\":\"failed\",\"code\":\"image_must_be_pinned_by_digest\"}"
    );
    assert!(!log.exists());
    let _ = fs::remove_dir_all(directory);
}
