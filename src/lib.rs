#![forbid(unsafe_code)]

pub mod workspace_snapshot;

use std::ffi::OsString;
use std::io::{ErrorKind, Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub const PINNED_IMAGE: &str =
    "alpine@sha256:5291449c3df73caf6ed85e649dec1b9e818b39a5d8c871e97afc13e9cd5e8fa8";
const MAX_ARGUMENTS: usize = 64;
const MAX_ARGUMENT_BYTES: usize = 16 * 1024;
const MAX_ARGUMENT_LENGTH: usize = 4096;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const RESULT_FRAME_MAGIC: &[u8] = b"WNMOUT1\n";

#[derive(Clone, Debug)]
pub struct Limits {
    pub wall_seconds: u64,
    pub memory_bytes: u64,
    pub cpu_millis: u32,
    pub pids: u32,
    pub output_bytes: usize,
}

#[derive(Clone, Debug)]
pub struct ManagerConfig {
    pub image: String,
    pub run_id: String,
    pub limits: Limits,
}

#[derive(Clone, Debug)]
pub struct TaskSpec {
    pub id: String,
    pub argv: Vec<String>,
    pub workspace_snapshot: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValidationError {
    TaskId,
    RunId,
    Arguments,
    Image,
    Limits,
    WorkspaceSnapshot,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::TaskId => "task_id_invalid",
            Self::RunId => "run_id_invalid",
            Self::Arguments => "arguments_invalid",
            Self::Image => "image_must_be_pinned_by_digest",
            Self::Limits => "resource_limits_out_of_bounds",
            Self::WorkspaceSnapshot => "workspace_snapshot_invalid",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for ValidationError {}

#[derive(Debug)]
pub enum RunError {
    Invalid(ValidationError),
    EngineUnavailable,
    EngineFailed,
    OutputReadFailed,
    WorkspaceTransferFailed,
    WorkspaceOutputInvalid,
    CleanupFailed,
    Cancelled,
}

impl std::fmt::Display for RunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Invalid(error) => return write!(formatter, "{error}"),
            Self::EngineUnavailable => "container_engine_unavailable",
            Self::EngineFailed => "container_run_failed",
            Self::OutputReadFailed => "container_output_unavailable",
            Self::WorkspaceTransferFailed => "workspace_transfer_failed",
            Self::WorkspaceOutputInvalid => "workspace_output_invalid",
            Self::CleanupFailed => "container_cleanup_failed",
            Self::Cancelled => "task_cancelled",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for RunError {}

#[derive(Debug)]
pub struct RunResult {
    pub task_id: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub output_truncated: bool,
    pub workspace_snapshot: Option<Vec<u8>>,
}

struct WorkspaceResultFrame {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    output_truncated: bool,
    archive: Vec<u8>,
}

fn decode_workspace_result_frame(
    frame: &[u8],
    output_limit: usize,
) -> Result<WorkspaceResultFrame, RunError> {
    let mut cursor = 0;
    if frame.get(..RESULT_FRAME_MAGIC.len()) != Some(RESULT_FRAME_MAGIC) {
        return Err(RunError::WorkspaceOutputInvalid);
    }
    cursor += RESULT_FRAME_MAGIC.len();
    let stdout_length = read_frame_length(frame, &mut cursor, output_limit)?;
    let stderr_length = read_frame_length(frame, &mut cursor, output_limit)?;
    let stdout_truncated = read_frame_flag(frame, &mut cursor)?;
    let stderr_truncated = read_frame_flag(frame, &mut cursor)?;
    let stdout = read_frame_bytes(frame, &mut cursor, stdout_length)?.to_vec();
    let stderr = read_frame_bytes(frame, &mut cursor, stderr_length)?.to_vec();
    let archive = frame
        .get(cursor..)
        .ok_or(RunError::WorkspaceOutputInvalid)?
        .to_vec();
    if workspace_snapshot::decode_tar(&archive).is_err() {
        return Err(RunError::WorkspaceOutputInvalid);
    }
    Ok(WorkspaceResultFrame {
        stdout,
        stderr,
        output_truncated: stdout_truncated || stderr_truncated,
        archive,
    })
}

fn read_frame_length(frame: &[u8], cursor: &mut usize, maximum: usize) -> Result<usize, RunError> {
    let remaining = frame
        .get(*cursor..)
        .ok_or(RunError::WorkspaceOutputInvalid)?;
    let line_length = remaining
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or(RunError::WorkspaceOutputInvalid)?;
    let line = &remaining[..line_length];
    if line.is_empty() || line.iter().any(|byte| !byte.is_ascii_digit()) {
        return Err(RunError::WorkspaceOutputInvalid);
    }
    let length = std::str::from_utf8(line)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value <= maximum)
        .ok_or(RunError::WorkspaceOutputInvalid)?;
    *cursor += line_length + 1;
    Ok(length)
}

fn read_frame_flag(frame: &[u8], cursor: &mut usize) -> Result<bool, RunError> {
    match read_frame_bytes(frame, cursor, 2)? {
        [b'0', b'\n'] => Ok(false),
        [b'1', b'\n'] => Ok(true),
        _ => Err(RunError::WorkspaceOutputInvalid),
    }
}

fn read_frame_bytes<'a>(
    frame: &'a [u8],
    cursor: &mut usize,
    length: usize,
) -> Result<&'a [u8], RunError> {
    let end = cursor
        .checked_add(length)
        .ok_or(RunError::WorkspaceOutputInvalid)?;
    let bytes = frame
        .get(*cursor..end)
        .ok_or(RunError::WorkspaceOutputInvalid)?;
    *cursor = end;
    Ok(bytes)
}

pub fn validate(task: &TaskSpec, config: &ManagerConfig) -> Result<(), ValidationError> {
    if !safe_identifier(&task.id, 48) {
        return Err(ValidationError::TaskId);
    }
    if !safe_identifier(&config.run_id, 48) {
        return Err(ValidationError::RunId);
    }
    if task.argv.is_empty()
        || task.argv.len() > MAX_ARGUMENTS
        || task.argv.iter().any(|value| {
            value.is_empty() || value.len() > MAX_ARGUMENT_LENGTH || value.as_bytes().contains(&0)
        })
        || task.argv.iter().map(String::len).sum::<usize>() > MAX_ARGUMENT_BYTES
    {
        return Err(ValidationError::Arguments);
    }
    if !valid_pinned_image(&config.image) {
        return Err(ValidationError::Image);
    }
    let limits = &config.limits;
    if !(1..=3600).contains(&limits.wall_seconds)
        || !(64 * 1024 * 1024..=8 * 1024 * 1024 * 1024).contains(&limits.memory_bytes)
        || !(100..=8000).contains(&limits.cpu_millis)
        || !(1..=256).contains(&limits.pids)
        || !(1..=MAX_OUTPUT_BYTES).contains(&limits.output_bytes)
    {
        return Err(ValidationError::Limits);
    }
    Ok(())
}

pub fn build_docker_args(
    task: &TaskSpec,
    config: &ManagerConfig,
) -> Result<Vec<String>, ValidationError> {
    build_docker_command(task, config, false).map(|(args, _)| args)
}

fn build_docker_command(
    task: &TaskSpec,
    config: &ManagerConfig,
    capture_workspace_snapshot: bool,
) -> Result<(Vec<String>, Option<Vec<u8>>), ValidationError> {
    validate(task, config)?;
    let workspace_archive = task
        .workspace_snapshot
        .as_deref()
        .map(|snapshot| {
            let files = workspace_snapshot::decode(snapshot)
                .map_err(|_| ValidationError::WorkspaceSnapshot)?;
            workspace_snapshot::encode_tar(&files).map_err(|_| ValidationError::WorkspaceSnapshot)
        })
        .transpose()?;
    let has_workspace = workspace_archive.is_some();
    let name = container_name(&config.run_id, &task.id);
    let limits = &config.limits;
    let mut args = vec![
        "run".into(),
        "--rm".into(),
        "--init".into(),
        "--pull=never".into(),
        "--name".into(),
        name,
        "--label".into(),
        "com.portable-node-manager.managed=true".into(),
        "--label".into(),
        format!("com.portable-node-manager.run={}", config.run_id),
        "--label".into(),
        format!("com.portable-node-manager.task={}", task.id),
        "--network".into(),
        "none".into(),
        "--ipc".into(),
        "private".into(),
        "--read-only".into(),
        "--cap-drop".into(),
        "ALL".into(),
        "--security-opt".into(),
        "no-new-privileges:true".into(),
        "--user".into(),
        "65534:65534".into(),
        "--pids-limit".into(),
        limits.pids.to_string(),
        "--memory".into(),
        format!("{}b", limits.memory_bytes),
        "--cpus".into(),
        format!(
            "{}.{:03}",
            limits.cpu_millis / 1000,
            limits.cpu_millis % 1000
        ),
        "--tmpfs".into(),
        "/tmp:rw,noexec,nosuid,nodev,size=16m,mode=1777,uid=65534,gid=65534".into(),
    ];
    if workspace_archive.is_some() {
        args.push("--interactive".into());
        args.extend([
            "--tmpfs".into(),
            "/workspace:rw,noexec,nosuid,nodev,size=32m,mode=0700,uid=65534,gid=65534".into(),
        ]);
    }
    args.extend([
        "--workdir".into(),
        if has_workspace { "/workspace" } else { "/tmp" }.into(),
        "--entrypoint".into(),
        "/bin/busybox".into(),
        config.image.clone(),
    ]);
    if has_workspace {
        args.extend([
            "sh".into(),
            "-c".into(),
            workspace_task_command(capture_workspace_snapshot).into(),
            "wardnm".into(),
            limits.wall_seconds.to_string(),
        ]);
        if capture_workspace_snapshot {
            args.push(limits.output_bytes.to_string());
        }
    } else {
        args.extend([
            "timeout".into(),
            "-s".into(),
            "KILL".into(),
            limits.wall_seconds.to_string(),
        ]);
    }
    args.extend(task.argv.iter().cloned());
    Ok((args, workspace_archive))
}

pub fn container_name(run_id: &str, task_id: &str) -> String {
    format!("pnm-{run_id}-{task_id}")
}

fn workspace_task_command(capture_workspace_snapshot: bool) -> &'static str {
    if !capture_workspace_snapshot {
        return "tar -xf - -C /workspace && wall_seconds=\"$1\" && shift && exec /bin/busybox timeout -s KILL \"$wall_seconds\" \"$@\"";
    }

    r#"tar -xf - -C /workspace || exit 125
wall_seconds="$1"
output_limit="$2"
shift 2
mkdir -p /tmp/wardnm-meta || exit 125
/bin/busybox timeout -s KILL "$wall_seconds" "$@" > /tmp/wardnm-meta/stdout 2> /tmp/wardnm-meta/stderr
task_status=$?
stdout_bytes=$(wc -c < /tmp/wardnm-meta/stdout)
stderr_bytes=$(wc -c < /tmp/wardnm-meta/stderr)
stdout_truncated=0
stderr_truncated=0
[ "$stdout_bytes" -le "$output_limit" ] || stdout_truncated=1
[ "$stderr_bytes" -le "$output_limit" ] || stderr_truncated=1
printf 'WNMOUT1\n%s\n%s\n%s\n%s\n' "$((stdout_bytes < output_limit ? stdout_bytes : output_limit))" "$((stderr_bytes < output_limit ? stderr_bytes : output_limit))" "$stdout_truncated" "$stderr_truncated"
head -c "$output_limit" /tmp/wardnm-meta/stdout
head -c "$output_limit" /tmp/wardnm-meta/stderr
unsupported=$(find /workspace ! -type d ! -type f -print -quit) || exit 125
[ -z "$unsupported" ] || exit 125
(cd /workspace && find . -type f | tar -cf - -T -) || exit 125
exit "$task_status""#
}

fn safe_identifier(value: &str, max_length: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && value.as_bytes()[0].is_ascii_lowercase()
}

fn valid_pinned_image(value: &str) -> bool {
    let Some((repository, digest)) = value.rsplit_once("@sha256:") else {
        return false;
    };
    !repository.is_empty()
        && repository.len() <= 255
        && repository.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"./_-".contains(&byte)
        })
        && digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn capture_limited<R: Read>(mut reader: R, limit: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut captured = Vec::with_capacity(limit.min(4096));
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok((captured, truncated)),
            Ok(count) => {
                let remaining = limit.saturating_sub(captured.len());
                captured.extend_from_slice(&buffer[..count.min(remaining)]);
                truncated |= count > remaining;
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

fn container_timed_out(deadline_reached: bool, exit_code: Option<i32>) -> bool {
    deadline_reached || exit_code == Some(137)
}

fn docker_environment(
    environment: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    const ALLOWED: &[&str] = &[
        "PATH",
        "HOME",
        "USERPROFILE",
        "HOMEDRIVE",
        "HOMEPATH",
        "SYSTEMROOT",
        "WINDIR",
        "APPDATA",
        "LOCALAPPDATA",
        "TEMP",
        "TMP",
        "DOCKER_CONFIG",
    ];

    environment
        .into_iter()
        .filter(|(name, _)| {
            name.to_str().is_some_and(|name| {
                ALLOWED
                    .iter()
                    .any(|allowed| name.eq_ignore_ascii_case(allowed))
            })
        })
        .collect()
}

pub struct NodeManager {
    pub config: ManagerConfig,
    pub docker_binary: String,
    pub cancelled: AtomicBool,
}

impl NodeManager {
    pub fn run(&self, task: &TaskSpec) -> Result<RunResult, RunError> {
        self.run_task(task, false)
    }

    pub fn run_with_workspace_snapshot(&self, task: &TaskSpec) -> Result<RunResult, RunError> {
        if task.workspace_snapshot.is_none() {
            return Err(RunError::Invalid(ValidationError::WorkspaceSnapshot));
        }
        self.run_task(task, true)
    }

    fn run_task(
        &self,
        task: &TaskSpec,
        capture_workspace_snapshot: bool,
    ) -> Result<RunResult, RunError> {
        let (mut args, workspace_archive) =
            build_docker_command(task, &self.config, capture_workspace_snapshot)
                .map_err(RunError::Invalid)?;
        let name = container_name(&self.config.run_id, &task.id);
        if capture_workspace_snapshot {
            args[0] = "create".into();
            args.retain(|argument| argument != "--rm");
            let created = Command::new(&self.docker_binary)
                .arg("--context")
                .arg("default")
                .args(args.iter())
                .env_clear()
                .envs(docker_environment(std::env::vars_os()))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map_err(|_| RunError::EngineUnavailable)?;
            if !created.success() {
                return Err(RunError::EngineFailed);
            }
        }
        let mut command = Command::new(&self.docker_binary);
        command.arg("--context").arg("default");
        if capture_workspace_snapshot {
            command.args(["start", "--attach", "--interactive", &name]);
        } else {
            command.args(args);
        }
        let mut child = match command
            .env_clear()
            .envs(docker_environment(std::env::vars_os()))
            .stdin(if workspace_archive.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => {
                if capture_workspace_snapshot {
                    self.remove_container(&name)?;
                }
                return Err(RunError::EngineUnavailable);
            }
        };

        let stdout = child.stdout.take().ok_or(RunError::OutputReadFailed)?;
        let stderr = child.stderr.take().ok_or(RunError::OutputReadFailed)?;
        let output_limit = self.config.limits.output_bytes;
        let stdout_limit = if capture_workspace_snapshot {
            workspace_snapshot::MAX_TAR_BYTES + output_limit * 2 + 128
        } else {
            output_limit
        };
        let stdout_reader = thread::spawn(move || capture_limited(stdout, stdout_limit));
        let stderr_reader = thread::spawn(move || capture_limited(stderr, output_limit));
        let mut workspace_writer = match workspace_archive {
            Some(archive) => match child.stdin.take() {
                Some(mut stdin) => Some(thread::spawn(move || stdin.write_all(&archive))),
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    if self.remove_container(&name).is_err() {
                        return Err(RunError::CleanupFailed);
                    }
                    return Err(RunError::WorkspaceTransferFailed);
                }
            },
            None => None,
        };
        let deadline = Instant::now() + Duration::from_secs(self.config.limits.wall_seconds + 5);
        let mut timed_out = false;
        let status = loop {
            if self.cancelled.load(Ordering::Acquire) {
                let _ = child.kill();
                if let Some(writer) = workspace_writer.take() {
                    let _ = writer.join();
                }
                let _ = child.wait();
                self.remove_container(&name)?;
                return Err(RunError::Cancelled);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                Ok(None) => {
                    timed_out = true;
                    let _ = child.kill();
                    if let Some(writer) = workspace_writer.take() {
                        let _ = writer.join();
                    }
                    let status = child.wait();
                    if !capture_workspace_snapshot {
                        self.remove_container(&name)?;
                    }
                    break status.map_err(|_| RunError::EngineFailed)?;
                }
                Err(_) => {
                    let _ = child.kill();
                    if let Some(writer) = workspace_writer.take() {
                        let _ = writer.join();
                    }
                    let _ = child.wait();
                    self.remove_container(&name)?;
                    return Err(RunError::EngineFailed);
                }
            }
        };
        timed_out = container_timed_out(timed_out, status.code());
        if workspace_writer.is_some_and(|writer| !matches!(writer.join(), Ok(Ok(())))) {
            self.remove_container(&name)?;
            return Err(RunError::WorkspaceTransferFailed);
        }
        let (stdout, stdout_truncated) = stdout_reader
            .join()
            .map_err(|_| RunError::OutputReadFailed)?
            .map_err(|_| RunError::OutputReadFailed)?;
        let (stderr, stderr_truncated) = stderr_reader
            .join()
            .map_err(|_| RunError::OutputReadFailed)?
            .map_err(|_| RunError::OutputReadFailed)?;
        let result = if capture_workspace_snapshot {
            if stdout_truncated {
                Err(RunError::WorkspaceOutputInvalid)
            } else {
                decode_workspace_result_frame(&stdout, output_limit).and_then(|frame| {
                    let workspace_snapshot = if status.success() && !timed_out {
                        let files = workspace_snapshot::decode_tar(&frame.archive)
                            .map_err(|_| RunError::WorkspaceOutputInvalid)?;
                        Some(
                            workspace_snapshot::encode(&files)
                                .map_err(|_| RunError::WorkspaceOutputInvalid)?,
                        )
                    } else {
                        None
                    };
                    Ok((
                        frame.stdout,
                        frame.stderr,
                        frame.output_truncated || stderr_truncated,
                        workspace_snapshot,
                    ))
                })
            }
        } else {
            Ok((stdout, stderr, stdout_truncated || stderr_truncated, None))
        };
        if capture_workspace_snapshot {
            self.remove_container(&name)?;
        }
        let (stdout, stderr, output_truncated, workspace_snapshot) = result?;

        Ok(RunResult {
            task_id: task.id.clone(),
            exit_code: status.code(),
            timed_out,
            stdout,
            stderr,
            output_truncated,
            workspace_snapshot,
        })
    }

    fn remove_container(&self, name: &str) -> Result<(), RunError> {
        let status = Command::new(&self.docker_binary)
            .arg("--context")
            .arg("default")
            .args(["rm", "--force", "--", name])
            .env_clear()
            .envs(docker_environment(std::env::vars_os()))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|_| RunError::CleanupFailed)?;
        if status.success() {
            Ok(())
        } else {
            Err(RunError::CleanupFailed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::capture_limited;
    use super::container_timed_out;
    use super::decode_workspace_result_frame;
    use super::docker_environment;
    #[cfg(unix)]
    use super::{Limits, ManagerConfig, NodeManager, RunError, TaskSpec, PINNED_IMAGE};
    #[cfg(unix)]
    use crate::workspace_snapshot;
    use std::ffi::OsString;
    #[cfg(unix)]
    use std::fs;
    use std::io::Cursor;
    #[cfg(unix)]
    use std::path::PathBuf;
    #[cfg(unix)]
    use std::sync::atomic::{AtomicU64, Ordering};

    #[cfg(unix)]
    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn captures_only_the_configured_output_prefix_and_drains_the_rest() {
        let (output, truncated) = capture_limited(Cursor::new(b"abcdefgh"), 5).unwrap();
        assert_eq!(output, b"abcde");
        assert!(truncated);
    }

    #[test]
    fn reports_untruncated_output_at_the_exact_limit() {
        let (output, truncated) = capture_limited(Cursor::new(b"abcde"), 5).unwrap();
        assert_eq!(output, b"abcde");
        assert!(!truncated);
    }

    #[test]
    fn decodes_a_bounded_workspace_result_frame() {
        let snapshot = workspace_snapshot::encode_tar(&[workspace_snapshot::WorkspaceFile {
            path: "result.txt".into(),
            contents: b"done".to_vec(),
        }])
        .expect("encode archive");
        assert!(workspace_snapshot::decode_tar(&snapshot).is_ok());
        let mut frame = b"WNMOUT1\n3\n2\n0\n1\n".to_vec();
        frame.extend_from_slice(b"outer");
        frame.extend_from_slice(&snapshot);

        let result = decode_workspace_result_frame(&frame, 8).expect("decode frame");
        assert_eq!(result.stdout, b"out");
        assert_eq!(result.stderr, b"er");
        assert!(result.output_truncated);
        assert_eq!(
            workspace_snapshot::decode_tar(&result.archive).expect("decode snapshot"),
            [workspace_snapshot::WorkspaceFile {
                path: "result.txt".into(),
                contents: b"done".to_vec(),
            }]
        );
    }

    #[test]
    fn rejects_malformed_workspace_result_frames() {
        for frame in [
            b"invalid".as_slice(),
            b"WNMOUT1\n999\n0\n0\n0\n".as_slice(),
            b"WNMOUT1\n1\n0\n2\n0\nx".as_slice(),
            b"WNMOUT1\n1\n0\n0\n0\nx".as_slice(),
        ] {
            assert!(decode_workspace_result_frame(frame, 8).is_err());
        }
    }

    #[test]
    fn recognizes_container_deadline_termination() {
        assert!(container_timed_out(false, Some(137)));
        assert!(container_timed_out(true, Some(0)));
        assert!(!container_timed_out(false, Some(1)));
    }

    #[test]
    fn passes_only_non_secret_docker_context_environment() {
        let environment = docker_environment([
            (OsString::from("HOME"), OsString::from("/home/operator")),
            (OsString::from("PATH"), OsString::from("/usr/bin")),
            (
                OsString::from("DOCKER_CONFIG"),
                OsString::from("/tmp/docker"),
            ),
            (OsString::from("OPENAI_API_KEY"), OsString::from("secret")),
            (OsString::from("GH_TOKEN"), OsString::from("secret")),
            (
                OsString::from("DOCKER_HOST"),
                OsString::from("tcp://remote"),
            ),
        ]);
        let names = environment
            .iter()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(names, ["HOME", "PATH", "DOCKER_CONFIG"]);
    }

    #[cfg(unix)]
    #[test]
    fn streams_only_the_encoded_archive_to_the_container_engine_stdin() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir().join(format!(
            "wardnm-stdin-{}-{}",
            std::process::id(),
            NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).expect("create test directory");
        let docker = directory.join("docker");
        fs::write(&docker, "#!/bin/sh\ncat > \"$0.input\"\n").expect("write fake engine");
        let mut permissions = fs::metadata(&docker)
            .expect("engine metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&docker, permissions).expect("make fake engine executable");

        let files = [workspace_snapshot::WorkspaceFile {
            path: "src/main.js".into(),
            contents: b"process.exit(0)".to_vec(),
        }];
        let snapshot = workspace_snapshot::encode(&files).expect("encode snapshot");
        let expected_archive = workspace_snapshot::encode_tar(&files).expect("encode archive");
        let manager = NodeManager {
            config: ManagerConfig {
                image: PINNED_IMAGE.into(),
                run_id: "run-test".into(),
                limits: Limits {
                    wall_seconds: 10,
                    memory_bytes: 128 * 1024 * 1024,
                    cpu_millis: 500,
                    pids: 32,
                    output_bytes: 4096,
                },
            },
            docker_binary: docker.to_string_lossy().into_owned(),
            cancelled: Default::default(),
        };

        let result = manager
            .run(&TaskSpec {
                id: "task-test".into(),
                argv: vec!["node".into(), "src/main.js".into()],
                workspace_snapshot: Some(snapshot),
            })
            .expect("run fake engine");
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(
            fs::read(directory.join("docker.input")).expect("captured stdin"),
            expected_archive
        );

        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[cfg(unix)]
    #[test]
    fn returns_a_validated_workspace_snapshot_and_removes_the_container() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir().join(format!(
            "wardnm-result-{}-{}",
            std::process::id(),
            NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).expect("create test directory");
        let docker = directory.join("docker");
        fs::write(
            &docker,
            "#!/bin/sh\ncase \"$3\" in\ncreate) printf 'container-id\\n' ;;\nstart) cat > \"$0.input\"; printf 'WNMOUT1\\n11\\n0\\n0\\n0\\ntask-output'; cat \"$0.archive\" ;;\nrm) printf '%s\\n' \"$@\" > \"$0.cleanup\" ;;\nesac\n",
        )
        .expect("write fake engine");
        let mut permissions = fs::metadata(&docker)
            .expect("engine metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&docker, permissions).expect("make fake engine executable");
        let archive = workspace_snapshot::encode_tar(&[workspace_snapshot::WorkspaceFile {
            path: "result.txt".into(),
            contents: b"updated".to_vec(),
        }])
        .expect("encode result archive");
        fs::write(directory.join("docker.archive"), archive).expect("write result archive");
        let files = [workspace_snapshot::WorkspaceFile {
            path: "input.txt".into(),
            contents: b"input".to_vec(),
        }];
        let snapshot = workspace_snapshot::encode(&files).expect("encode snapshot");
        let manager = NodeManager {
            config: ManagerConfig {
                image: PINNED_IMAGE.into(),
                run_id: "run-result".into(),
                limits: Limits {
                    wall_seconds: 10,
                    memory_bytes: 128 * 1024 * 1024,
                    cpu_millis: 500,
                    pids: 32,
                    output_bytes: 4096,
                },
            },
            docker_binary: docker.to_string_lossy().into_owned(),
            cancelled: Default::default(),
        };

        let result = manager
            .run_with_workspace_snapshot(&TaskSpec {
                id: "task-result".into(),
                argv: vec!["cat".into(), "input.txt".into()],
                workspace_snapshot: Some(snapshot),
            })
            .expect("run fake engine");

        assert_eq!(result.stdout, b"task-output");
        assert_eq!(
            workspace_snapshot::decode(
                result
                    .workspace_snapshot
                    .as_deref()
                    .expect("workspace snapshot")
            )
            .expect("decode returned snapshot"),
            [workspace_snapshot::WorkspaceFile {
                path: "result.txt".into(),
                contents: b"updated".to_vec(),
            }]
        );
        assert!(fs::read_to_string(directory.join("docker.cleanup"))
            .expect("cleanup invocation")
            .contains("--force"));
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[cfg(unix)]
    fn cancelled_manager(cleanup_exit_code: i32) -> (PathBuf, NodeManager) {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir().join(format!(
            "wardnm-cleanup-{}-{}",
            std::process::id(),
            NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).expect("create test directory");
        let docker = directory.join("docker");
        fs::write(
            &docker,
            format!(
                "#!/bin/sh\nif [ \"$3\" = rm ]; then printf '%s\\n' \"$@\" > \"$0.cleanup\"; exit {cleanup_exit_code}; fi\nexec sleep 30\n"
            ),
        )
        .expect("write fake engine");
        let mut permissions = fs::metadata(&docker)
            .expect("engine metadata")
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&docker, permissions).expect("make fake engine executable");

        let manager = NodeManager {
            config: ManagerConfig {
                image: PINNED_IMAGE.into(),
                run_id: "run-test".into(),
                limits: Limits {
                    wall_seconds: 10,
                    memory_bytes: 128 * 1024 * 1024,
                    cpu_millis: 500,
                    pids: 32,
                    output_bytes: 4096,
                },
            },
            docker_binary: docker.to_string_lossy().into_owned(),
            cancelled: std::sync::atomic::AtomicBool::new(true),
        };

        (directory, manager)
    }

    #[cfg(unix)]
    #[test]
    fn reports_cancelled_after_forced_container_cleanup_succeeds() {
        let (directory, manager) = cancelled_manager(0);
        let result = manager.run(&TaskSpec {
            id: "task-test".into(),
            argv: vec!["true".into()],
            workspace_snapshot: None,
        });

        assert!(matches!(result, Err(RunError::Cancelled)));
        assert!(fs::read_to_string(directory.join("docker.cleanup"))
            .expect("cleanup invocation")
            .contains("--force"));
        fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[cfg(unix)]
    #[test]
    fn reports_cleanup_failure_instead_of_claiming_cancelled() {
        let (directory, manager) = cancelled_manager(1);
        let result = manager.run(&TaskSpec {
            id: "task-test".into(),
            argv: vec!["true".into()],
            workspace_snapshot: None,
        });

        assert!(matches!(result, Err(RunError::CleanupFailed)));
        fs::remove_dir_all(directory).expect("remove test directory");
    }
}
