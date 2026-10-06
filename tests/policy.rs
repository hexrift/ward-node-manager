use std::ffi::OsStr;

use ward_node_manager::workspace_snapshot::{encode, WorkspaceFile};
use ward_node_manager::{build_docker_args, Limits, ManagerConfig, TaskSpec, ValidationError};

fn config() -> ManagerConfig {
    ManagerConfig {
        image: "alpine@sha256:5291449c3df73caf6ed85e649dec1b9e818b39a5d8c871e97afc13e9cd5e8fa8"
            .into(),
        run_id: "run-123".into(),
        limits: Limits {
            wall_seconds: 10,
            memory_bytes: 134_217_728,
            cpu_millis: 500,
            pids: 32,
            output_bytes: 4096,
        },
    }
}

fn task(id: &str, argv: &[&str]) -> TaskSpec {
    TaskSpec {
        id: id.into(),
        argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
        workspace_snapshot: None,
    }
}

#[test]
fn rejects_task_ids_that_could_escape_container_name_scope() {
    let too_long = "a".repeat(65);
    for id in ["", "../other", "task/name", "UPPER", too_long.as_str()] {
        assert!(matches!(
            build_docker_args(&task(id, &["true"]), &config()),
            Err(ValidationError::TaskId)
        ));
    }
}

#[test]
fn rejects_empty_or_unbounded_commands_before_docker_is_called() {
    assert!(matches!(
        build_docker_args(&task("task-1", &[]), &config()),
        Err(ValidationError::Arguments)
    ));
    assert!(matches!(
        build_docker_args(&task("task-1", &["x\0y"]), &config()),
        Err(ValidationError::Arguments)
    ));
    assert!(matches!(
        build_docker_args(&task("task-1", &[&"x".repeat(4097)]), &config()),
        Err(ValidationError::Arguments)
    ));
}

#[test]
fn rejects_mutable_images_and_resource_limits_outside_safe_ceilings() {
    let mut mutable = config();
    mutable.image = "alpine:latest".into();
    assert!(matches!(
        build_docker_args(&task("task-1", &["true"]), &mutable),
        Err(ValidationError::Image)
    ));

    let mut excessive = config();
    excessive.limits.pids = 4097;
    assert!(matches!(
        build_docker_args(&task("task-1", &["true"]), &excessive),
        Err(ValidationError::Limits)
    ));
}

#[test]
fn pins_execution_to_a_restricted_non_root_offline_container() {
    let args = build_docker_args(&task("worker-a", &["/bin/sh", "-c", "echo ok"]), &config())
        .expect("valid request");
    let args = args
        .iter()
        .map(|arg| OsStr::new(arg).to_string_lossy())
        .collect::<Vec<_>>();

    for required in [
        "--rm",
        "--network",
        "none",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges:true",
        "--user",
        "65534:65534",
        "--memory",
        "--cpus",
        "--pids-limit",
        "--pull=never",
    ] {
        assert!(args.iter().any(|arg| arg == required), "missing {required}");
    }
    assert!(args.windows(2).any(|pair| pair == ["--network", "none"]));
    assert!(args.windows(2).any(|pair| pair == ["--cap-drop", "ALL"]));
    assert!(!args.iter().any(|arg| arg == "--privileged"));
    assert!(!args.iter().any(|arg| arg.starts_with("/Users/")));
    assert!(args
        .windows(3)
        .any(|triple| triple == ["/bin/sh", "-c", "echo ok"]));
}

#[test]
fn transfers_validated_workspace_without_host_mounts() {
    let snapshot = encode(&[WorkspaceFile {
        path: "src/main.rs".into(),
        contents: b"fn main() {}".to_vec(),
    }])
    .expect("snapshot");
    let task = TaskSpec {
        id: "worker-a".into(),
        argv: vec!["node".into(), "src/main.js".into()],
        workspace_snapshot: Some(snapshot),
    };
    let args = build_docker_args(&task, &config()).expect("valid snapshot task");

    assert!(args.iter().any(|arg| arg == "--interactive"));
    assert!(args.windows(2).any(|pair| {
        pair == [
            "--tmpfs",
            "/workspace:rw,noexec,nosuid,nodev,size=32m,mode=0700,uid=65534,gid=65534",
        ]
    }));
    assert!(args
        .windows(2)
        .any(|pair| pair == ["--workdir", "/workspace"]));
    assert!(!args.iter().any(|arg| arg == "--mount" || arg == "-v"));
    assert_eq!(
        &args[args.len() - 7..],
        [
            "sh",
            "-c",
            "tar -xf - -C /workspace && wall_seconds=\"$1\" && shift && exec /bin/busybox timeout -s KILL \"$wall_seconds\" \"$@\"",
            "wardnm",
            "10",
            "node",
            "src/main.js"
        ]
    );
}

#[test]
fn rejects_malformed_workspace_snapshots_before_docker_runs() {
    let task = TaskSpec {
        id: "worker-a".into(),
        argv: vec!["node".into()],
        workspace_snapshot: Some(b"invalid".to_vec()),
    };

    assert!(matches!(
        build_docker_args(&task, &config()),
        Err(ValidationError::WorkspaceSnapshot)
    ));
}
