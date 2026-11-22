//! Environment for Git commands scoped to the selected repository.
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use crate::process::{JobEvent, JobPolicy, JobRequest, JobStream, JobSupervisor};

const GIT_OPTIONS: [&str; 5] = [
    "--no-pager",
    "-c",
    "color.ui=false",
    "-c",
    "core.quotePath=false",
];

pub(super) const CHECKOUT_TIMEOUT: Duration = Duration::from_secs(120);

fn git_environment(extra: &[(String, OsString)]) -> BTreeMap<String, OsString> {
    let mut env = environment();
    env.extend(extra.iter().cloned());
    env.insert("LC_ALL".into(), "C".into());
    env
}

pub(super) fn protect_checkout(args: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut args = args.into_iter();
    args.next()
        .into_iter()
        .chain([OsString::from("--no-overwrite-ignore")])
        .chain(args)
        .collect()
}

pub(super) fn display_paths<'a>(paths: impl IntoIterator<Item = &'a Path>) -> String {
    paths
        .into_iter()
        .map(|path| match path.to_str() {
            Some(text) if !text.chars().any(char::is_control) => text.to_owned(),
            _ => format!("{path:?}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Run native Git from a background worker.
pub(super) fn git_output(
    root: &Path,
    args: &[OsString],
    extra: &[(String, OsString)],
) -> Result<Output, String> {
    let program =
        crate::process::discover_executable("git").ok_or("Git is not installed or not on PATH")?;
    Command::new(program)
        .current_dir(root)
        .env_clear()
        .envs(git_environment(extra))
        .args(GIT_OPTIONS)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|_| "Can't run Git".into())
}

pub(super) struct GitOutput {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl GitOutput {
    pub(super) fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Bound Git and its hooks while retaining raw stderr and NUL-delimited paths.
pub(super) fn supervised_git_output(
    root: &Path,
    args: &[OsString],
    extra: &[(String, OsString)],
    timeout: Duration,
) -> Result<GitOutput, String> {
    let program =
        crate::process::discover_executable("git").ok_or("Git is not installed or not on PATH")?;
    let mut jobs = JobSupervisor::try_new(16).map_err(|_| "Can't start Git")?;
    jobs.start(JobRequest {
        program,
        argv0: None,
        arguments: GIT_OPTIONS
            .into_iter()
            .map(OsString::from)
            .chain(args.iter().cloned())
            .collect(),
        cwd: root.to_path_buf(),
        environment: git_environment(extra),
        root_revision: 0,
        stdin: None,
        policy: JobPolicy {
            timeout,
            ..JobPolicy::default()
        },
    })
    .map_err(|_| "Can't start Git")?;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut incomplete = false;
    loop {
        for event in jobs.poll() {
            match event {
                JobEvent::Started { .. } => {}
                JobEvent::Output {
                    stream,
                    bytes,
                    truncated,
                    abandoned,
                    ..
                } => {
                    let output = match stream {
                        JobStream::Stdout => &mut stdout,
                        JobStream::Stderr => &mut stderr,
                    };
                    output.extend(bytes);
                    incomplete |= truncated || abandoned;
                }
                JobEvent::Exited {
                    timed_out: true, ..
                } => return Err("Git took too long".into()),
                JobEvent::Exited { code, .. } => {
                    if incomplete {
                        return Err("Git's output was incomplete".into());
                    }
                    return Ok(GitOutput {
                        code,
                        stdout,
                        stderr,
                    });
                }
                JobEvent::Failed { .. } => return Err("Can't run Git".into()),
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Git's NUL-delimited paths use native bytes on Unix and UTF-8 on Windows.
pub(super) fn native_path(bytes: &[u8]) -> Option<PathBuf> {
    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    };
    #[cfg(not(unix))]
    let path = PathBuf::from(std::str::from_utf8(bytes).ok()?);
    (!path.as_os_str().is_empty()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_))))
    .then_some(path)
}

/// Match only Git's untracked checkout refusal, not tracked-change errors,
/// hooks or arbitrary stderr lines. Git reports ignored paths as untracked here.
pub(super) fn checkout_ignored(root: &Path, stderr: &[u8]) -> Result<Vec<PathBuf>, String> {
    const OVERWRITTEN: &[u8] =
        b"error: The following untracked working tree files would be overwritten by checkout:";
    const REMOVED: &[u8] =
        b"error: The following untracked working tree files would be removed by checkout:";
    const UNREADABLE: &str = "Can't check which ignored files would be replaced";
    let mut refused = Vec::new();
    let mut group = false;
    for line in stderr.split(|byte| *byte == b'\n') {
        #[cfg(windows)]
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line == OVERWRITTEN || line == REMOVED {
            group = true;
        } else if group {
            match line.strip_prefix(b"\t") {
                Some(path) => refused.push(native_path(path).ok_or(UNREADABLE)?),
                None => group = false,
            }
        }
    }
    if refused.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = [
        "--literal-pathspecs",
        "ls-files",
        "-o",
        "-i",
        "--exclude-standard",
        "--full-name",
        "-z",
        "--",
    ]
    .map(OsString::from)
    .to_vec();
    args.extend(refused.iter().map(|path| path.as_os_str().to_owned()));
    let result = supervised_git_output(root, &args, &[], CHECKOUT_TIMEOUT)?;
    if !result.success() || (!result.stdout.is_empty() && !result.stdout.ends_with(&[0])) {
        return Err(UNREADABLE.into());
    }
    let mut ignored = Vec::new();
    for raw in result
        .stdout
        .split(|byte| *byte == 0)
        .filter(|raw| !raw.is_empty())
    {
        let path = native_path(raw).ok_or(UNREADABLE)?;
        if refused.iter().any(|refused| path.starts_with(refused)) {
            ignored.push(path);
        }
    }
    ignored.sort_unstable();
    ignored.dedup();
    Ok(ignored)
}

pub(super) fn environment() -> BTreeMap<String, OsString> {
    let mut environment = std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value)))
        .collect::<BTreeMap<_, _>>();
    // Inherited directory/index overrides must not retarget the selected repository.
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
    ] {
        environment.remove(key);
    }
    environment.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
    environment.insert("GIT_PAGER".into(), "cat".into());
    environment
}
