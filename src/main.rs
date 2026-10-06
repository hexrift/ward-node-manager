#![forbid(unsafe_code)]

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use ward_node_manager::workspace_snapshot::{encode_directory, MAX_SNAPSHOT_BYTES};
use ward_node_manager::{Limits, ManagerConfig, NodeManager, TaskSpec, PINNED_IMAGE};

fn main() {
    let code = match run() {
        Ok(code) => code,
        Err(message) => {
            println!(
                "{{\"status\":\"failed\",\"code\":{}}}",
                json_string(&message)
            );
            2
        }
    };
    std::process::exit(code);
}

fn run() -> Result<i32, String> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("snapshot") => return pack_snapshot(args),
        Some("run") => {}
        _ => return Err("usage_invalid".into()),
    }

    let mut run_id = format!("run-{}-{}", std::process::id(), timestamp_nanos());
    let mut task_id = None;
    let mut workspace_snapshot = None;
    let mut workspace_snapshot_output = None;
    let mut limits = Limits {
        wall_seconds: 30,
        memory_bytes: 256 * 1024 * 1024,
        cpu_millis: 1000,
        pids: 32,
        output_bytes: 4096,
    };
    let mut command = Vec::new();
    while let Some(arg) = args.next() {
        if arg == "--" {
            command.extend(args);
            break;
        }
        let value = args.next().ok_or("usage_invalid")?;
        match arg.as_str() {
            "--run-id" => run_id = value,
            "--task-id" => task_id = Some(value),
            "--snapshot" => workspace_snapshot = Some(read_snapshot(&value)?),
            "--snapshot-output" => workspace_snapshot_output = Some(value),
            "--wall-seconds" => limits.wall_seconds = parse(&value)?,
            "--memory-bytes" => limits.memory_bytes = parse(&value)?,
            "--cpu-millis" => limits.cpu_millis = parse(&value)?,
            "--pids" => limits.pids = parse(&value)?,
            "--output-bytes" => limits.output_bytes = parse(&value)?,
            _ => return Err("option_invalid".into()),
        }
    }
    if command.is_empty() {
        return Err("command_required".into());
    }
    let task = TaskSpec {
        id: task_id.ok_or("task_id_required")?,
        argv: command,
        workspace_snapshot,
    };
    let config = ManagerConfig {
        image: PINNED_IMAGE.into(),
        run_id,
        limits,
    };
    let manager = NodeManager {
        config,
        docker_binary: "docker".into(),
        cancelled: Default::default(),
    };
    if workspace_snapshot_output.is_some() && task.workspace_snapshot.is_none() {
        return Err("workspace_snapshot_invalid".into());
    }
    let result = if workspace_snapshot_output.is_some() {
        manager.run_with_workspace_snapshot(&task)
    } else {
        manager.run(&task)
    };
    match result {
        Ok(result) => {
            let stdout = json_string(&String::from_utf8_lossy(&result.stdout));
            let stderr = json_string(&String::from_utf8_lossy(&result.stderr));
            let workspace_snapshot_bytes = match (
                workspace_snapshot_output.as_deref(),
                result.workspace_snapshot.as_deref(),
            ) {
                (Some(path), Some(snapshot)) => {
                    write_snapshot_output(path, snapshot)?;
                    snapshot.len().to_string()
                }
                _ => "null".into(),
            };
            println!(
                "{{\"status\":\"{}\",\"taskId\":{},\"exitCode\":{},\"timedOut\":{},\"outputTruncated\":{},\"stdout\":{},\"stderr\":{},\"workspaceSnapshotBytes\":{}}}",
                if result.exit_code == Some(0) && !result.timed_out { "completed" } else { "failed" },
                json_string(&result.task_id),
                result.exit_code.map_or("null".into(), |value| value.to_string()),
                result.timed_out,
                result.output_truncated,
                stdout,
                stderr,
                workspace_snapshot_bytes,
            );
            Ok(if result.exit_code == Some(0) && !result.timed_out {
                0
            } else {
                1
            })
        }
        Err(error) => Err(error.to_string()),
    }
}

fn write_snapshot_output(path: &str, snapshot: &[u8]) -> Result<(), String> {
    let mut file = create_private_snapshot(path).map_err(|_| "snapshot_output_unavailable")?;
    if file.write_all(snapshot).is_err() {
        drop(file);
        let _ = fs::remove_file(path);
        return Err("snapshot_output_unavailable".into());
    }
    Ok(())
}

fn pack_snapshot(mut args: impl Iterator<Item = String>) -> Result<i32, String> {
    if args.next().as_deref() != Some("pack") {
        return Err("usage_invalid".into());
    }
    let source = args.next().ok_or("usage_invalid")?;
    let output = args.next().ok_or("usage_invalid")?;
    if args.next().is_some() {
        return Err("usage_invalid".into());
    }

    let snapshot =
        encode_directory(Path::new(&source)).map_err(|_| "workspace_snapshot_invalid")?;
    let mut file = create_private_snapshot(&output).map_err(|_| "snapshot_output_unavailable")?;
    if file.write_all(&snapshot).is_err() {
        drop(file);
        let _ = fs::remove_file(output);
        return Err("snapshot_output_unavailable".into());
    }

    println!("{{\"status\":\"packed\",\"bytes\":{}}}", snapshot.len());
    Ok(0)
}

fn create_private_snapshot(path: impl AsRef<Path>) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn read_snapshot(path: &str) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|_| "snapshot_unavailable")?;
    let mut encoded = Vec::new();
    file.take((MAX_SNAPSHOT_BYTES + 1) as u64)
        .read_to_end(&mut encoded)
        .map_err(|_| "snapshot_unavailable")?;
    if encoded.len() > MAX_SNAPSHOT_BYTES {
        return Err("workspace_snapshot_invalid".into());
    }
    Ok(encoded)
}

fn parse<T: std::str::FromStr>(value: &str) -> Result<T, String> {
    value.parse().map_err(|_| "option_invalid".to_owned())
}

fn timestamp_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos())
}

fn json_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            value if value.is_control() => escaped.push(' '),
            value => escaped.push(value),
        }
    }
    escaped.push('"');
    escaped
}
