#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::io::{ErrorKind, Read};
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValidationError {
    TaskId,
    RunId,
    Arguments,
    Image,
    Limits,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::TaskId => "task_id_invalid",
            Self::RunId => "run_id_invalid",
            Self::Arguments => "arguments_invalid",
            Self::Image => "image_must_be_pinned_by_digest",
            Self::Limits => "resource_limits_out_of_bounds",
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
    Cancelled,
}

impl std::fmt::Display for RunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::Invalid(error) => return write!(formatter, "{error}"),
            Self::EngineUnavailable => "container_engine_unavailable",
            Self::EngineFailed => "container_run_failed",
            Self::OutputReadFailed => "container_output_unavailable",
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
    validate(task, config)?;
    let name = container_name(&config.run_id, &task.id);
    let limits = &config.limits;
    let args = vec![
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
        "--workdir".into(),
        "/tmp".into(),
        "--entrypoint".into(),
        "/bin/busybox".into(),
        config.image.clone(),
        "timeout".into(),
        "-s".into(),
        "KILL".into(),
        limits.wall_seconds.to_string(),
    ];
    let mut args = args;
    args.extend(task.argv.iter().cloned());
    Ok(args)
}

pub fn container_name(run_id: &str, task_id: &str) -> String {
    format!("pnm-{run_id}-{task_id}")
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
        let args = build_docker_args(task, &self.config).map_err(RunError::Invalid)?;
        let name = container_name(&self.config.run_id, &task.id);
        let mut child = Command::new(&self.docker_binary)
            .arg("--context")
            .arg("default")
            .args(args)
            .env_clear()
            .envs(docker_environment(std::env::vars_os()))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| RunError::EngineUnavailable)?;

        let stdout = child.stdout.take().ok_or(RunError::OutputReadFailed)?;
        let stderr = child.stderr.take().ok_or(RunError::OutputReadFailed)?;
        let output_limit = self.config.limits.output_bytes;
        let stdout_reader = thread::spawn(move || capture_limited(stdout, output_limit));
        let stderr_reader = thread::spawn(move || capture_limited(stderr, output_limit));
        let deadline = Instant::now() + Duration::from_secs(self.config.limits.wall_seconds + 5);
        let mut timed_out = false;
        let status = loop {
            if self.cancelled.load(Ordering::Acquire) {
                let _ = child.kill();
                self.remove_container(&name);
                return Err(RunError::Cancelled);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                Ok(None) => {
                    timed_out = true;
                    let _ = child.kill();
                    self.remove_container(&name);
                    break child.wait().map_err(|_| RunError::EngineFailed)?;
                }
                Err(_) => {
                    let _ = child.kill();
                    self.remove_container(&name);
                    return Err(RunError::EngineFailed);
                }
            }
        };
        timed_out = container_timed_out(timed_out, status.code());
        let (stdout, stdout_truncated) = stdout_reader
            .join()
            .map_err(|_| RunError::OutputReadFailed)?
            .map_err(|_| RunError::OutputReadFailed)?;
        let (stderr, stderr_truncated) = stderr_reader
            .join()
            .map_err(|_| RunError::OutputReadFailed)?
            .map_err(|_| RunError::OutputReadFailed)?;

        Ok(RunResult {
            task_id: task.id.clone(),
            exit_code: status.code(),
            timed_out,
            stdout,
            stderr,
            output_truncated: stdout_truncated || stderr_truncated,
        })
    }

    fn remove_container(&self, name: &str) {
        let _ = Command::new(&self.docker_binary)
            .arg("--context")
            .arg("default")
            .args(["rm", "--force", "--", name])
            .env_clear()
            .envs(docker_environment(std::env::vars_os()))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::capture_limited;
    use super::container_timed_out;
    use super::docker_environment;
    use std::ffi::OsString;
    use std::io::Cursor;

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
}
