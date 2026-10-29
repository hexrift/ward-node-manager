use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fake_docker(name: &str) -> (PathBuf, PathBuf) {
    let directory = std::env::temp_dir().join(format!(
        "wardnm-cli-workspace-{}-{name}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(directory.join("source")).expect("create fake docker directory");
    fs::write(directory.join("source/README.md"), "hello\n").expect("write workspace file");
    let log = directory.join("docker.log");
    let docker = directory.join("docker");
    fs::write(
        &docker,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\ncat > /dev/null\nexit 0\n",
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

fn wardnm(path: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_wardnm"))
        .env("PATH", path)
        .args(args)
        .output()
        .expect("run wardnm")
}

fn run_with_workspace(directory: &Path, options: &[&str]) -> std::process::Output {
    let snapshot = directory.join("workspace.wnm");
    let packed = wardnm(
        directory,
        &[
            "snapshot",
            "pack",
            directory.join("source").to_str().unwrap(),
            snapshot.to_str().unwrap(),
        ],
    );
    assert_eq!(packed.status.code(), Some(0));
    let mut args = vec![
        "run",
        "--task-id",
        "task-1",
        "--snapshot",
        snapshot.to_str().unwrap(),
    ];
    args.extend_from_slice(options);
    args.extend_from_slice(&["--", "true"]);
    wardnm(directory, &args)
}

#[test]
fn sizes_an_executable_workspace_from_the_command_line() {
    let (directory, log) = fake_docker("exec");
    run_with_workspace(
        &directory,
        &[
            "--memory-bytes",
            "1073741824",
            "--workspace-bytes",
            "536870912",
            "--workspace-mode",
            "exec",
        ],
    );

    let calls = fs::read_to_string(&log).expect("docker was called");
    assert!(
        calls.contains(" /workspace:rw,exec,nosuid,nodev,size=512m,mode=0700,uid=65534,gid=65534 ")
    );
    let _ = fs::remove_dir_all(directory);
}

#[test]
fn refuses_an_unknown_workspace_mode_before_docker_is_called() {
    let (directory, log) = fake_docker("mode");
    let output = run_with_workspace(&directory, &["--workspace-mode", "writable"]);

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "{\"status\":\"failed\",\"code\":\"workspace_mode_invalid\"}"
    );
    assert!(!log.exists());
    let _ = fs::remove_dir_all(directory);
}
