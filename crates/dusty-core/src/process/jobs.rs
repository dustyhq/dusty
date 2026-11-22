//! Bounded child-process jobs, output and cancellation.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
#[cfg(windows)]
use std::sync::atomic::AtomicU8;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::SyncSender;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use thiserror::Error;

const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct JobId(pub u64);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobStream {
    Stdout,
    Stderr,
}

impl JobStream {
    pub fn name(self) -> &'static str {
        match self {
            Self::Stdout => "standard output",
            Self::Stderr => "standard error",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Stdout => 0,
            Self::Stderr => 1,
        }
    }
}
/// Streams whose capture was abandoned before EOF.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AbandonedStreams(u8);

impl AbandonedStreams {
    pub const EMPTY: Self = Self(0);

    pub fn insert(&mut self, stream: JobStream) {
        self.0 |= 1 << stream.index();
    }

    pub fn contains(self, stream: JobStream) -> bool {
        self.0 & (1 << stream.index()) != 0
    }

    pub fn merge(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub fn names(self) -> Option<&'static str> {
        match (
            self.contains(JobStream::Stdout),
            self.contains(JobStream::Stderr),
        ) {
            (false, false) => None,
            (true, false) => Some("standard output"),
            (false, true) => Some("standard error"),
            (true, true) => Some("standard output and error"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct JobPolicy {
    pub maximum_output_bytes: usize,
    pub maximum_line_bytes: usize,
    pub maximum_stdin_bytes: usize,
    pub timeout: Duration,
}
impl Default for JobPolicy {
    fn default() -> Self {
        Self {
            maximum_output_bytes: 16 * 1024 * 1024,
            maximum_line_bytes: 1024 * 1024,
            maximum_stdin_bytes: 16 * 1024 * 1024,
            timeout: Duration::from_secs(300),
        }
    }
}
#[derive(Clone, Debug)]
pub struct JobRequest {
    pub program: PathBuf,
    pub argv0: Option<PathBuf>,
    pub arguments: Vec<std::ffi::OsString>,
    pub cwd: PathBuf,
    pub environment: BTreeMap<String, std::ffi::OsString>,
    pub root_revision: u64,
    pub stdin: Option<Vec<u8>>,
    pub policy: JobPolicy,
}
#[derive(Clone, Debug)]
pub enum JobEvent {
    Started {
        job: JobId,
        root_revision: u64,
    },
    Output {
        job: JobId,
        stream: JobStream,
        bytes: Vec<u8>,
        /// Capture is incomplete. At the job cap, reported once per affected
        /// stream, possibly with no captured bytes.
        truncated: bool,
        /// The supervisor stopped draining without reaching EOF, not a size limit.
        abandoned: bool,
    },
    Exited {
        job: JobId,
        code: Option<i32>,
        timed_out: bool,
    },
    Failed {
        job: JobId,
        message: String,
    },
}

struct JobSlot(Arc<AtomicUsize>);
impl Drop for JobSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
type Preflight = Box<dyn FnOnce(&Path) -> Result<(), String> + Send>;

struct Submission {
    preflight: Option<Preflight>,
    root: Option<crate::files::RootIdentity>,
    id: JobId,
    request: JobRequest,
    cancel: Arc<AtomicBool>,
    output_drain: Arc<OutputDrain>,
    slot: Arc<JobSlot>,
    input: Option<Receiver<Vec<u8>>>,
}
struct JobControl {
    cancel: Arc<AtomicBool>,
    output_drain: Arc<OutputDrain>,
}

/// Reserves worker capacity. Filesystem access, spawning and process cleanup run on workers.
pub struct JobSupervisor {
    next: Option<u64>,
    jobs: HashMap<JobId, JobControl>,
    inputs: HashMap<JobId, SyncSender<Vec<u8>>>,
    sender: SyncSender<Submission>,
    receiver: Option<Receiver<JobEvent>>,
    stop: Arc<AtomicBool>,
    residents: Arc<AtomicUsize>,
    worker: thread::JoinHandle<()>,
}
pub struct JobReservation<'a> {
    supervisor: &'a mut JobSupervisor,
    slot: Arc<JobSlot>,
}
impl JobSupervisor {
    pub fn new(event_capacity: usize) -> Self {
        Self::try_new(event_capacity).expect("job worker")
    }
    pub fn try_new(event_capacity: usize) -> io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<Submission>(256);
        let (events, receiver) = mpsc::sync_channel(event_capacity.clamp(1, 4096));
        let stop = Arc::new(AtomicBool::new(false));
        let closing = stop.clone();
        let worker = thread::Builder::new()
            .name("dusty-jobs".into())
            .spawn(move || {
                let mut engine = JobEngine::new(event_capacity);
                while !closing.load(Ordering::Acquire) {
                    // Child processes are polled every 2 ms while any run; with none, the
                    // worker sleeps until a job arrives or the supervisor is dropped, so an
                    // idle editor stays idle.
                    let next = if engine.jobs.is_empty() {
                        rx.recv().map_err(|_| mpsc::TryRecvError::Disconnected)
                    } else {
                        rx.try_recv()
                    };
                    match next {
                        Ok(submission) => {
                            let id = submission.id;
                            let revision = submission.request.root_revision;
                            let event = if submission.cancel.load(Ordering::Acquire) {
                                JobEvent::Failed {
                                    job: id,
                                    message: "job cancelled before launch".into(),
                                }
                            } else {
                                match engine.start(submission) {
                                    Ok(()) => JobEvent::Started {
                                        job: id,
                                        root_revision: revision,
                                    },
                                    Err(error) => JobEvent::Failed {
                                        job: id,
                                        message: error.to_string(),
                                    },
                                }
                            };
                            if !deliver(&events, event, &closing, &mut engine) {
                                break;
                            }
                        }
                        Err(mpsc::TryRecvError::Disconnected) => break,
                        Err(mpsc::TryRecvError::Empty) => {}
                    }
                    for event in engine.poll() {
                        if !deliver(&events, event, &closing, &mut engine) {
                            return;
                        }
                    }
                    if !engine.jobs.is_empty() {
                        thread::sleep(Duration::from_millis(2));
                    }
                }
            })?;
        Ok(Self {
            next: Some(1),
            jobs: HashMap::new(),
            inputs: HashMap::new(),
            sender: tx,
            receiver: Some(receiver),
            stop,
            residents: Arc::new(AtomicUsize::new(0)),
            worker,
        })
    }
    pub fn reserve(&mut self) -> Result<JobReservation<'_>, JobError> {
        if self.stop.load(Ordering::Acquire) || self.worker.is_finished() {
            return Err(JobError::Stopped);
        }
        if self.next.is_none() {
            return Err(JobError::Exhausted);
        }
        self.residents
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < 256).then_some(count + 1)
            })
            .map_err(|_| JobError::Full)?;
        let slot = Arc::new(JobSlot(self.residents.clone()));
        Ok(JobReservation {
            supervisor: self,
            slot,
        })
    }
    pub fn start(&mut self, request: JobRequest) -> Result<JobId, JobError> {
        self.reserve()?.submit(request)
    }
    /// Check the job's canonical working directory on the worker just before spawning.
    pub fn start_checked(
        &mut self,
        request: JobRequest,
        check: impl FnOnce(&Path) -> Result<(), String> + Send + 'static,
    ) -> Result<JobId, JobError> {
        self.reserve()?
            .submit_inner(request, None, None, Some(Box::new(check)))
    }
    /// Interactive jobs use raw output chunks and keep stdin open until explicitly closed.
    pub fn start_streaming(
        &mut self,
        request: JobRequest,
        root: crate::files::RootIdentity,
    ) -> Result<JobId, JobError> {
        let (sender, input) = mpsc::sync_channel(16);
        let id = self
            .reserve()?
            .submit_inner(request, Some(root), Some(input), None)?;
        self.inputs.insert(id, sender);
        Ok(id)
    }
    pub fn write_stdin(&mut self, id: JobId, bytes: Vec<u8>, close: bool) -> Result<(), JobError> {
        if bytes.len() > 1024 * 1024 {
            return Err(JobError::InvalidPolicy);
        }
        let sender = self.inputs.get(&id).ok_or(JobError::Unknown(id))?;
        if !bytes.is_empty() {
            sender.try_send(bytes).map_err(|error| match error {
                mpsc::TrySendError::Full(_) => JobError::Full,
                mpsc::TrySendError::Disconnected(_) => JobError::Unknown(id),
            })?;
        }
        if close {
            self.inputs.remove(&id);
        }
        Ok(())
    }
    pub fn poll(&mut self) -> Vec<JobEvent> {
        let mut events = Vec::new();
        if let Some(receiver) = &self.receiver {
            for _ in 0..256 {
                let Ok(event) = receiver.try_recv() else {
                    break;
                };
                let id = event.id();
                if !self.jobs.contains_key(&id) {
                    continue;
                }
                if matches!(event, JobEvent::Failed { .. } | JobEvent::Exited { .. }) {
                    self.jobs.remove(&id);
                    self.inputs.remove(&id);
                }
                events.push(event);
            }
        }
        if self.worker.is_finished() && events.is_empty() {
            events.extend(self.jobs.drain().map(|(job, _)| JobEvent::Failed {
                job,
                message: "job worker stopped; process/output outcome may be incomplete".into(),
            }));
        }
        events
    }
    pub fn cancel(&mut self, id: JobId) -> Result<(), JobError> {
        self.jobs
            .get(&id)
            .ok_or(JobError::Unknown(id))?
            .cancel
            .store(true, Ordering::Release);
        Ok(())
    }
    /// Reports that an active job exceeded an output limit, even if its event is queued.
    pub fn output_clipped(&self, id: JobId) -> bool {
        self.jobs
            .get(&id)
            .is_some_and(|job| job.output_drain.clipped.load(Ordering::Acquire))
    }
    pub fn is_running(&self, id: JobId) -> bool {
        self.jobs.contains_key(&id)
    }
    pub fn begin_shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        for job in self.jobs.values() {
            job.cancel.store(true, Ordering::Release);
        }
    }
    pub fn abandon(&mut self) {
        self.begin_shutdown();
        self.receiver.take();
        self.jobs.clear();
        self.inputs.clear();
    }
}
impl JobReservation<'_> {
    pub fn submit(self, request: JobRequest) -> Result<JobId, JobError> {
        self.submit_inner(request, None, None, None)
    }
    pub fn submit_scoped(
        self,
        request: JobRequest,
        root: crate::files::RootIdentity,
    ) -> Result<JobId, JobError> {
        if !request.cwd.starts_with(&root.canonical_path) {
            return Err(JobError::InvalidPolicy);
        }
        self.submit_inner(request, Some(root), None, None)
    }
    fn submit_inner(
        self,
        request: JobRequest,
        root: Option<crate::files::RootIdentity>,
        input: Option<Receiver<Vec<u8>>>,
        preflight: Option<Preflight>,
    ) -> Result<JobId, JobError> {
        check_request(&request, input.is_some())?;
        let supervisor = self.supervisor;
        let next = supervisor.next.ok_or(JobError::Exhausted)?;
        supervisor.next = next.checked_add(1);
        let id = JobId(next);
        let cancel = Arc::new(AtomicBool::new(false));
        let output_drain = Arc::new(OutputDrain::default());
        supervisor
            .sender
            .try_send(Submission {
                preflight,
                root,
                id,
                request,
                cancel: cancel.clone(),
                output_drain: output_drain.clone(),
                slot: self.slot,
                input,
            })
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => JobError::Full,
                mpsc::TrySendError::Disconnected(_) => JobError::Stopped,
            })?;
        supervisor.jobs.insert(
            id,
            JobControl {
                cancel,
                output_drain,
            },
        );
        Ok(id)
    }
}
impl Drop for JobSupervisor {
    fn drop(&mut self) {
        self.abandon();
    }
}
impl JobEvent {
    fn id(&self) -> JobId {
        match self {
            Self::Started { job, .. }
            | Self::Output { job, .. }
            | Self::Exited { job, .. }
            | Self::Failed { job, .. } => *job,
        }
    }
}
fn check_request(request: &JobRequest, streaming: bool) -> Result<(), JobError> {
    let argument_bytes = request
        .arguments
        .iter()
        .map(|argument| argument.len())
        .sum::<usize>();
    let environment_bytes = request
        .environment
        .iter()
        .map(|(key, value)| key.len().saturating_add(value.len()))
        .sum::<usize>();
    if (!streaming && !request.program.is_absolute())
        || request.program.as_os_str().is_empty()
        || !request.cwd.is_absolute()
        || request.arguments.len() > 4096
        || request.environment.len() > 4096
        || argument_bytes > 1024 * 1024
        || environment_bytes > 1024 * 1024
        || request.policy.maximum_output_bytes == 0
        || request.policy.maximum_output_bytes > 256 * 1024 * 1024
        || request.policy.maximum_line_bytes == 0
        || request.policy.maximum_line_bytes > 16 * 1024 * 1024
        || request.policy.maximum_stdin_bytes > 256 * 1024 * 1024
        || request
            .stdin
            .as_ref()
            .is_some_and(|bytes| bytes.len() > request.policy.maximum_stdin_bytes)
        || request.policy.timeout.is_zero()
        || request.policy.timeout > Duration::from_secs(86_400)
    {
        return Err(JobError::InvalidPolicy);
    }

    Ok(())
}
fn deliver(
    sender: &SyncSender<JobEvent>,
    mut event: JobEvent,
    stop: &AtomicBool,
    engine: &mut JobEngine,
) -> bool {
    loop {
        if stop.load(Ordering::Acquire) {
            return false;
        }
        match sender.try_send(event) {
            Ok(()) => return true,
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
            Err(mpsc::TrySendError::Full(value)) => event = value,
        }
        engine.expire();
        thread::sleep(Duration::from_millis(2));
    }
}
struct RunningJob {
    child: Child,
    started: Instant,
    timeout: Duration,
    cancel: Arc<AtomicBool>,
    input_cancel: Arc<AtomicBool>,
    readers: Arc<AtomicUsize>,
    io_failed: Arc<AtomicBool>,
    status: Option<Option<i32>>,
    output_drain: Arc<OutputDrain>,
    skip_pending_io: bool,
    timed_out: bool,
    killed: bool,
    _slot: Arc<JobSlot>,
}
impl RunningJob {
    fn observe_exit(&mut self, now: Instant) -> io::Result<()> {
        if self.status.is_none()
            && let Some(status) = job_exit_status(&mut self.child)?
        {
            self.status = Some(status);
            let _ = self.output_drain.exited_at.set(now);
        }
        Ok(())
    }
}
#[derive(Default)]
struct OutputDrain {
    exited_at: OnceLock<Instant>,
    deadline_elapsed: AtomicBool,
    abandoned: [AtomicBool; 2],
    overflowed_at: OnceLock<Instant>,
    #[cfg(windows)]
    read_phase: [AtomicU8; 2],
    clipped: AtomicBool,
}

#[cfg(windows)]
#[repr(u8)]
enum PipeReadPhase {
    BetweenReads = 0,
    Reading,
    Finished,
    Detached,
}

impl OutputDrain {
    #[cfg(windows)]
    fn detach_blocked_readers(&self, readers: &AtomicUsize) {
        if !self.cutoff_elapsed() {
            return;
        }
        // Queue backpressure is not a blocked read. A detached worker keeps
        // its slot until the handle unblocks, bounding resident workers.
        for stream in [JobStream::Stdout, JobStream::Stderr] {
            if self.read_phase[stream.index()]
                .compare_exchange(
                    PipeReadPhase::Reading as u8,
                    PipeReadPhase::Detached as u8,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                self.abandoned(stream).store(true, Ordering::Release);
                readers.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    fn abandoned(&self, stream: JobStream) -> &AtomicBool {
        &self.abandoned[stream.index()]
    }

    #[cfg(windows)]
    fn read_pipe(
        &self,
        stream: JobStream,
        pipe: &mut impl Read,
        bytes: &mut [u8],
    ) -> io::Result<usize> {
        let phase = &self.read_phase[stream.index()];
        if phase
            .compare_exchange(
                PipeReadPhase::BetweenReads as u8,
                PipeReadPhase::Reading as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Ok(0);
        }
        let result = pipe.read(bytes);
        if phase
            .compare_exchange(
                PipeReadPhase::Reading as u8,
                PipeReadPhase::BetweenReads as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Ok(0);
        }
        result
    }

    fn cutoff_elapsed(&self) -> bool {
        // Start the short grace only once both events have happened. Overflow can
        // be discovered late, for example after a backpressured record is sent.
        self.deadline_elapsed.load(Ordering::Acquire)
            || self
                .exited_at
                .get()
                .zip(self.overflowed_at.get())
                .is_some_and(|(exit, overflow)| exit.max(overflow).elapsed() >= OUTPUT_DRAIN_GRACE)
    }
}

struct JobOutputPipe<T> {
    pipe: super::CancellablePipe<T>,
    drain: Arc<OutputDrain>,
    stream: JobStream,
    closed: bool,
}

impl<T: Read> Read for JobOutputPipe<T> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.closed {
            return Ok(0);
        }
        self.pipe.retry(None, |pipe| {
            #[cfg(windows)]
            let result = self.drain.read_pipe(self.stream, pipe, bytes);
            #[cfg(not(windows))]
            let result = pipe.read(bytes);
            match result {
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock && self.drain.cutoff_elapsed() =>
                {
                    // A read that can still return bytes always gets a chance to do so.
                    // Blocking Windows reads are detached by the engine instead.
                    self.closed = true;
                    self.drain
                        .abandoned(self.stream)
                        .store(true, Ordering::Release);
                    Ok(0)
                }
                result => result,
            }
        })
    }
}

struct JobEngine {
    jobs: HashMap<JobId, RunningJob>,
    sender: SyncSender<JobEvent>,
    receiver: Receiver<JobEvent>,
    capacity: usize,
}
impl JobEngine {
    fn new(capacity: usize) -> Self {
        let (sender, receiver) = mpsc::sync_channel(capacity.clamp(1, 4096));
        Self {
            jobs: HashMap::new(),
            sender,
            receiver,
            capacity: capacity.clamp(1, 4096),
        }
    }
    fn start(&mut self, submission: Submission) -> Result<(), JobError> {
        let Submission {
            preflight,
            root,
            id,
            request,
            cancel,
            output_drain,
            slot,
            input,
        } = submission;
        let streaming = input.is_some();
        let cwd = crate::files::canonicalize(&request.cwd).map_err(|_| JobError::InvalidPolicy)?;
        if let Some(expected) = root
            && (crate::files::RootIdentity::capture(&expected.canonical_path)
                .ok()
                .as_ref()
                != Some(&expected)
                || !cwd.starts_with(&expected.canonical_path))
        {
            return Err(JobError::RootChanged);
        }
        if !request.cwd.is_dir() {
            return Err(JobError::InvalidPolicy);
        }
        if cancel.load(Ordering::Acquire) {
            return Err(JobError::Cancelled);
        }
        let mut command = Command::new(&request.program);
        command
            .args(&request.arguments)
            .current_dir(&cwd)
            .env_clear()
            .envs(&request.environment)
            .stdin(if request.stdin.is_some() || streaming {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
            if let Some(argv0) = &request.argv0 {
                command.arg0(argv0);
            }
        }
        if let Some(check) = preflight {
            check(&cwd).map_err(JobError::Preflight)?;
        }
        let mut child = command.spawn().map_err(JobError::Spawn)?;
        // The deadline can stop inherited stdin without dropping queued output.
        let input_cancel = Arc::new(AtomicBool::new(false));
        // Configure all pipes before starting I/O threads, so setup failure still owns the child.
        let pipes = (|| -> io::Result<_> {
            Ok((
                child
                    .stdin
                    .take()
                    .map(|pipe| crate::process::CancellablePipe::new(pipe, input_cancel.clone()))
                    .transpose()?,
                child
                    .stdout
                    .take()
                    .map(|pipe| crate::process::CancellablePipe::new(pipe, cancel.clone()))
                    .transpose()?,
                child
                    .stderr
                    .take()
                    .map(|pipe| crate::process::CancellablePipe::new(pipe, cancel.clone()))
                    .transpose()?,
            ))
        })();
        let (stdin, stdout, stderr) = match pipes {
            Ok(pipes) => pipes,
            Err(error) => {
                terminate_tree(&mut child, false);
                let _ = child.wait();
                return Err(JobError::Spawn(error));
            }
        };
        let readers = Arc::new(AtomicUsize::new(0));
        let io_failed = Arc::new(AtomicBool::new(false));
        let setup_io = (|| -> io::Result<()> {
            if let Some(input) = input {
                let mut stdin = stdin.expect("streaming stdin is piped");
                let initial = request.stdin.unwrap_or_default();
                let cancelled = input_cancel.clone();
                let lease = slot.clone();
                thread::Builder::new()
                    .name("dusty-job-stdin".into())
                    .spawn(move || {
                        let _lease = lease;
                        if stdin.write_all(&initial).is_err() {
                            return;
                        }
                        while !cancelled.load(Ordering::Acquire) {
                            match input.recv_timeout(Duration::from_millis(10)) {
                                Ok(bytes) => {
                                    if stdin.write_all(&bytes).is_err() {
                                        break;
                                    }
                                }
                                Err(mpsc::RecvTimeoutError::Timeout) => {}
                                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            }
                        }
                    })?;
            } else if let Some(bytes) = request.stdin
                && let Some(mut stdin) = stdin
            {
                let lease = slot.clone();
                let pending = readers.clone();
                let failed = io_failed.clone();
                let cancelled = input_cancel.clone();
                pending.fetch_add(1, Ordering::AcqRel);
                thread::Builder::new()
                    .name("dusty-job-stdin".into())
                    .spawn(move || {
                        let _lease = lease;
                        if stdin.write_all(&bytes).is_err() && !cancelled.load(Ordering::Acquire) {
                            failed.store(true, Ordering::Release);
                        }
                        pending.fetch_sub(1, Ordering::AcqRel);
                    })?;
            }
            let total = Arc::new(AtomicUsize::new(0));
            if let Some(stdout) = stdout {
                stream_reader(
                    id,
                    JobOutputPipe {
                        pipe: stdout,
                        drain: output_drain.clone(),
                        stream: JobStream::Stdout,
                        closed: false,
                    },
                    self.sender.clone(),
                    total.clone(),
                    (request.policy.clone(), streaming),
                    (
                        readers.clone(),
                        io_failed.clone(),
                        slot.clone(),
                        cancel.clone(),
                    ),
                )?;
            }
            if let Some(stderr) = stderr {
                stream_reader(
                    id,
                    JobOutputPipe {
                        pipe: stderr,
                        drain: output_drain.clone(),
                        stream: JobStream::Stderr,
                        closed: false,
                    },
                    self.sender.clone(),
                    total,
                    (request.policy.clone(), streaming),
                    (
                        readers.clone(),
                        io_failed.clone(),
                        slot.clone(),
                        cancel.clone(),
                    ),
                )?;
            }
            Ok(())
        })();
        if let Err(error) = setup_io {
            cancel.store(true, Ordering::Release);
            input_cancel.store(true, Ordering::Release);
            terminate_tree(&mut child, false);
            let _ = child.wait();
            return Err(JobError::Spawn(error));
        }
        self.jobs.insert(
            id,
            RunningJob {
                child,
                started: Instant::now(),
                timeout: request.policy.timeout,
                cancel,
                input_cancel,
                readers,
                io_failed,
                status: None,
                output_drain,
                skip_pending_io: false,
                timed_out: false,
                killed: false,
                _slot: slot,
            },
        );
        Ok(())
    }

    fn expire(&mut self) {
        for running in self.jobs.values_mut() {
            let now = Instant::now();
            let _ = running.observe_exit(now);
            let expired = now.duration_since(running.started) >= running.timeout;
            running.timed_out |= running.status.is_none() && expired;
            // After exit, a full queue must not prevent buffered output from reaching EOF.
            // Only a live leader can be stopped by the job deadline.
            let stop_for_timeout = expired && running.status.is_none();
            if expired || running.cancel.load(Ordering::Acquire) {
                running.input_cancel.store(true, Ordering::Release);
            }
            if expired && !stop_for_timeout {
                running
                    .output_drain
                    .deadline_elapsed
                    .store(true, Ordering::Release);
            }
            #[cfg(windows)]
            if running.status.is_some() {
                running
                    .output_drain
                    .detach_blocked_readers(&running.readers);
            }
            if stop_for_timeout
                && running.readers.load(Ordering::Acquire) != 0
                && !running.cancel.swap(true, Ordering::AcqRel)
            {
                running.skip_pending_io = true;
            }
            if !running.killed && (stop_for_timeout || running.cancel.load(Ordering::Acquire)) {
                if running.timed_out {
                    running.cancel.store(true, Ordering::Release);
                }
                terminate_tree(&mut running.child, running.status.is_some());
                running.killed = true;
            }
        }
    }
    fn poll(&mut self) -> Vec<JobEvent> {
        self.expire();
        let mut events = Vec::new();
        for _ in 0..256 {
            match self.receiver.try_recv() {
                Ok(event) => events.push(event),
                Err(_) => break,
            }
        }
        let mut done = Vec::new();
        let mut terminals = Vec::new();
        for (id, running) in &mut self.jobs {
            if let Err(error) = running.observe_exit(Instant::now()) {
                running.cancel.store(true, Ordering::Release);
                running.input_cancel.store(true, Ordering::Release);
                terminate_tree(&mut running.child, false);
                let _ = running.child.wait();
                events.push(JobEvent::Failed {
                    job: *id,
                    message: error.to_string(),
                });
                done.push(*id);
                continue;
            }
            if let Some(code) = running.status
                && (running.skip_pending_io || running.readers.load(Ordering::Acquire) == 0)
            {
                running.cancel.store(true, Ordering::Release);
                running.input_cancel.store(true, Ordering::Release);
                // Abandoning a pipe is not cancellation of the helpers holding it.
                let _ = running.child.wait();
                for stream in [JobStream::Stdout, JobStream::Stderr] {
                    if running
                        .output_drain
                        .abandoned(stream)
                        .load(Ordering::Acquire)
                    {
                        terminals.push(JobEvent::Output {
                            job: *id,
                            stream,
                            bytes: Vec::new(),
                            truncated: true,
                            abandoned: true,
                        });
                    }
                }
                terminals.push(if running.io_failed.load(Ordering::Acquire) {
                    JobEvent::Failed {
                        job: *id,
                        message: "job input/output transfer failed".into(),
                    }
                } else {
                    JobEvent::Exited {
                        job: *id,
                        code,
                        timed_out: running.timed_out,
                    }
                });
                done.push(*id);
            }
        }
        // Drain queued output before publishing exits. Abandoned readers are cancelled;
        // the supervisor discards any output racing in after their exit.
        if !terminals.is_empty() {
            for _ in 0..self.capacity {
                match self.receiver.try_recv() {
                    Ok(event) => events.push(event),
                    Err(_) => break,
                }
            }
        }
        events.extend(terminals);
        for id in done {
            self.jobs.remove(&id);
        }
        events
    }
}
impl Drop for JobEngine {
    fn drop(&mut self) {
        for job in self.jobs.values_mut() {
            job.cancel.store(true, Ordering::Release);
            job.input_cancel.store(true, Ordering::Release);
            if !job.killed {
                terminate_tree(&mut job.child, job.status.is_some());
            }
            let _ = job.child.wait();
        }
    }
}
fn stream_reader(
    id: JobId,
    reader: JobOutputPipe<impl Read + Send + 'static>,
    sender: SyncSender<JobEvent>,
    total: Arc<AtomicUsize>,
    (policy, streaming): (JobPolicy, bool),
    completion: (
        Arc<AtomicUsize>,
        Arc<AtomicBool>,
        Arc<JobSlot>,
        Arc<AtomicBool>,
    ),
) -> io::Result<()> {
    let (readers, failed, slot, cancel) = completion;
    readers.fetch_add(1, Ordering::AcqRel);
    thread::Builder::new()
        .name("dusty-job-output".into())
        .spawn(move || {
            struct End {
                readers: Arc<AtomicUsize>,
                #[cfg(windows)]
                drain: Arc<OutputDrain>,
                #[cfg(windows)]
                stream: JobStream,
            }
            impl Drop for End {
                fn drop(&mut self) {
                    #[cfg(windows)]
                    if self.drain.read_phase[self.stream.index()]
                        .swap(PipeReadPhase::Finished as u8, Ordering::AcqRel)
                        == PipeReadPhase::Detached as u8
                    {
                        // The engine already accounted for this reader at detachment.
                        return;
                    }
                    self.readers.fetch_sub(1, Ordering::AcqRel);
                }
            }
            let _end = End {
                readers,
                #[cfg(windows)]
                drain: reader.drain.clone(),
                #[cfg(windows)]
                stream: reader.stream,
            };
            let _slot = slot;
            let drain = reader.drain.clone();
            let stream = reader.stream;
            let mut reader = BufReader::new(reader);
            let mut output_limit_reported = false;
            loop {
                if cancel.load(Ordering::Acquire) {
                    break;
                }
                if output_limit_reported && drain.cutoff_elapsed() {
                    drain.abandoned(stream).store(true, Ordering::Release);
                    break;
                }
                let mut bytes = Vec::new();
                let capture = total.load(Ordering::Relaxed) < policy.maximum_output_bytes;
                let result = if !capture {
                    // Keep the pipe open for the producer, reusing the reader's buffer.
                    reader
                        .fill_buf()
                        .map(<[u8]>::len)
                        .inspect(|&read| reader.consume(read))
                } else if streaming {
                    bytes.resize(8192.min(policy.maximum_line_bytes), 0);
                    reader
                        .read(&mut bytes)
                        .inspect(|read| bytes.truncate(*read))
                } else {
                    reader
                        .by_ref()
                        .take(policy.maximum_line_bytes.saturating_add(1) as u64)
                        .read_until(b'\n', &mut bytes)
                };
                #[cfg(windows)]
                if drain.read_phase[stream.index()].load(Ordering::Acquire)
                    == PipeReadPhase::Detached as u8
                {
                    break;
                }
                let read = match result {
                    Ok(read) => read,
                    Err(_) => {
                        if !cancel.load(Ordering::Acquire) {
                            failed.store(true, Ordering::Release);
                        }
                        break;
                    }
                };
                if read == 0 {
                    break;
                }
                let line_truncated = bytes.len() > policy.maximum_line_bytes;
                let discard_tail = line_truncated && !bytes.ends_with(b"\n");
                bytes.truncate(policy.maximum_line_bytes);
                let allowed = if capture {
                    reserve_output(&total, policy.maximum_output_bytes, bytes.len())
                } else {
                    0
                };
                let output_truncated = !capture || allowed < bytes.len();
                bytes.truncate(allowed);
                let truncated = line_truncated || output_truncated && !output_limit_reported;
                output_limit_reported |= output_truncated;
                if output_truncated {
                    // Either pipe can consume the shared allowance. An exact fit,
                    // or a single overlong line, does not by itself mean overflow.
                    drain.overflowed_at.get_or_init(Instant::now);
                }
                if truncated {
                    // Stop can arrive while this event is waiting for queue space.
                    drain.clipped.store(true, Ordering::Release);
                }
                if !bytes.is_empty() || truncated {
                    let mut event = JobEvent::Output {
                        job: id,
                        stream,
                        bytes,
                        truncated,
                        abandoned: false,
                    };
                    loop {
                        if cancel.load(Ordering::Acquire) {
                            return;
                        }
                        match sender.try_send(event) {
                            Ok(()) => break,
                            Err(mpsc::TrySendError::Disconnected(_)) => return,
                            Err(mpsc::TrySendError::Full(value)) => {
                                event = value;
                                thread::sleep(Duration::from_millis(2));
                            }
                        }
                    }
                }
                if discard_tail && !output_truncated {
                    // Discard the rest of this physical line without allocating for it.
                    loop {
                        if drain.cutoff_elapsed() {
                            drain.abandoned(stream).store(true, Ordering::Release);
                            return;
                        }
                        match reader.fill_buf() {
                            Ok([]) => break,
                            Ok(tail) => {
                                let newline = tail.iter().position(|byte| *byte == b'\n');
                                let consumed = newline.map_or(tail.len(), |at| at + 1);
                                reader.consume(consumed);
                                if newline.is_some() || cancel.load(Ordering::Acquire) {
                                    break;
                                }
                            }
                            Err(_) => {
                                if !cancel.load(Ordering::Acquire) {
                                    failed.store(true, Ordering::Release);
                                }
                                return;
                            }
                        }
                    }
                }
            }
        })
        .map(|_| ())
}
fn reserve_output(total: &AtomicUsize, limit: usize, requested: usize) -> usize {
    let mut current = total.load(Ordering::Relaxed);
    loop {
        let allowed = requested.min(limit.saturating_sub(current));
        match total.compare_exchange_weak(
            current,
            current.saturating_add(allowed),
            Ordering::AcqRel,
            Ordering::Relaxed,
        ) {
            Ok(_) => return allowed,
            Err(actual) => current = actual,
        }
    }
}

fn job_exit_status(child: &mut Child) -> io::Result<Option<Option<i32>>> {
    crate::process::child_exit_status(child)
}

fn terminate_tree(child: &mut Child, reaped: bool) {
    crate::process::terminate_process_group(child.id());
    if !reaped {
        let _ = child.kill();
    }
}
#[derive(Debug, Error)]
pub enum JobError {
    #[error("job policy or request is invalid")]
    InvalidPolicy,
    #[error("cannot spawn job: {0}")]
    Spawn(#[source] io::Error),
    #[error("{0}")]
    Preflight(String),
    #[error("unknown job {0:?}")]
    Unknown(JobId),
    #[error("job queue is full")]
    Full,
    #[error("job worker stopped")]
    Stopped,
    #[error("job identity exhausted")]
    Exhausted,
    #[error("job cancelled before spawn")]
    Cancelled,
    #[error("task execution root filesystem identity changed")]
    RootChanged,
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn windows_total_cap_grace_detaches_both_quiet_pipes() {
        let (stdout, mut stdout_writer) = io::pipe().unwrap();
        let (stderr, stderr_writer) = io::pipe().unwrap();
        let mut engine = JobEngine::new(16);
        let id = JobId(1);
        let system_root = PathBuf::from(std::env::var_os("SystemRoot").unwrap());
        let child = Command::new(system_root.join("System32/cmd.exe"))
            .args(["/D", "/C", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let drain = Arc::new(OutputDrain::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let readers = Arc::new(AtomicUsize::new(0));
        let slot = Arc::new(JobSlot(Arc::new(AtomicUsize::new(1))));
        let failed = Arc::new(AtomicBool::new(false));
        let timeout = Duration::from_secs(30);
        let policy = JobPolicy {
            maximum_output_bytes: 4,
            timeout,
            ..JobPolicy::default()
        };
        let total = Arc::new(AtomicUsize::new(0));
        for (stream, pipe) in [(JobStream::Stdout, stdout), (JobStream::Stderr, stderr)] {
            stream_reader(
                id,
                JobOutputPipe {
                    pipe: crate::process::CancellablePipe::new(pipe, cancel.clone()).unwrap(),
                    drain: drain.clone(),
                    stream,
                    closed: false,
                },
                engine.sender.clone(),
                total.clone(),
                (policy.clone(), false),
                (
                    readers.clone(),
                    failed.clone(),
                    slot.clone(),
                    cancel.clone(),
                ),
            )
            .unwrap();
        }
        stdout_writer.write_all(b"abc\ndef\n").unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while drain.overflowed_at.get().is_none()
            || drain
                .read_phase
                .iter()
                .any(|phase| phase.load(Ordering::Acquire) != PipeReadPhase::Reading as u8)
        {
            assert!(
                Instant::now() < until,
                "reader did not block after clipping"
            );
            thread::sleep(Duration::from_millis(2));
        }
        engine.jobs.insert(
            id,
            RunningJob {
                child,
                started: Instant::now(),
                timeout,
                cancel,
                input_cancel: Arc::new(AtomicBool::new(false)),
                readers,
                io_failed: failed,
                status: None,
                output_drain: drain,
                skip_pending_io: false,
                timed_out: false,
                killed: false,
                _slot: slot,
            },
        );
        let mut clipped = false;
        let mut abandoned = AbandonedStreams::default();
        let mut exited = false;
        while !exited {
            for event in engine.poll() {
                match event {
                    JobEvent::Output {
                        stream,
                        abandoned: true,
                        ..
                    } => {
                        assert!(clipped && !exited);
                        abandoned.insert(stream);
                    }
                    JobEvent::Output {
                        truncated: true,
                        abandoned: false,
                        ..
                    } => clipped = true,
                    JobEvent::Output { bytes, .. } => assert_eq!(bytes, b"abc\n"),
                    JobEvent::Exited {
                        code, timed_out, ..
                    } => {
                        assert_eq!(code, Some(0));
                        assert!(!timed_out);
                        assert!(abandoned.contains(JobStream::Stdout));
                        assert!(abandoned.contains(JobStream::Stderr));
                        exited = true;
                    }
                    other => panic!("unexpected job event: {other:?}"),
                }
            }
            assert!(
                Instant::now() < until,
                "quiet clipped pipe blocked completion"
            );
            thread::sleep(Duration::from_millis(2));
        }
        // Both writers stayed open until after the terminal arrived.
        drop((stdout_writer, stderr_writer));
    }

    #[test]
    fn exited_windows_job_drains_its_full_queue_past_deadline() {
        let root = tempfile::tempdir().unwrap();
        let system_root =
            PathBuf::from(std::env::var_os("SystemRoot").expect("Windows system root"));
        let mut engine = JobEngine::new(64);
        let id = JobId(1);
        let request = JobRequest {
            program: system_root.join("System32/cmd.exe"),
            argv0: None,
            arguments: ["/D", "/S", "/C", "for /L %i in (1,1,1000) do @echo x"]
                .into_iter()
                .map(Into::into)
                .collect(),
            cwd: root.path().into(),
            environment: BTreeMap::from([("SystemRoot".into(), system_root.into_os_string())]),
            root_revision: 1,
            stdin: None,
            policy: JobPolicy {
                timeout: Duration::from_secs(10),
                ..JobPolicy::default()
            },
        };
        engine
            .start(Submission {
                preflight: None,
                root: None,
                id,
                request,
                cancel: Arc::new(AtomicBool::new(false)),
                output_drain: Arc::new(OutputDrain::default()),
                slot: Arc::new(JobSlot(Arc::new(AtomicUsize::new(1)))),
                input: None,
            })
            .unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        let running = engine.jobs.get_mut(&id).unwrap();
        while running.status.is_none()
            || running.output_drain.read_phase[JobStream::Stderr.index()].load(Ordering::Acquire)
                != PipeReadPhase::Finished as u8
        {
            running.observe_exit(Instant::now()).unwrap();
            assert!(
                Instant::now() < until,
                "leader and stderr reader did not exit with the stdout queue full"
            );
            thread::yield_now();
        }
        assert_eq!(running.status, Some(Some(0)));
        running.started = Instant::now() - running.timeout;
        let mut lines = 0;
        while !engine.jobs.is_empty() {
            for event in engine.poll() {
                match event {
                    JobEvent::Output {
                        bytes, truncated, ..
                    } => {
                        assert!(!truncated);
                        assert_eq!(bytes, b"x\r\n");
                        lines += 1;
                    }
                    JobEvent::Exited {
                        code, timed_out, ..
                    } => {
                        assert_eq!(code, Some(0));
                        assert!(!timed_out);
                    }
                    other => panic!("unexpected job event: {other:?}"),
                }
            }
            assert!(Instant::now() < until, "short lines did not drain");
            thread::yield_now();
        }
        assert_eq!(lines, 1000);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use crate::process::jobs::*;
    fn request(root: &std::path::Path, command: &str) -> JobRequest {
        JobRequest {
            program: PathBuf::from("/bin/sh"),
            argv0: None,
            arguments: vec!["-c".into(), command.into()],
            cwd: root.into(),
            environment: BTreeMap::new(),
            root_revision: 1,
            stdin: None,
            policy: JobPolicy {
                timeout: Duration::from_secs(2),
                ..JobPolicy::default()
            },
        }
    }
    #[test]
    fn job_process_receives_native_arguments_and_environment() {
        use std::os::unix::ffi::OsStringExt;
        let root = tempfile::tempdir().unwrap();
        let native = std::ffi::OsString::from_vec(b"path-\xff".to_vec());
        let mut request = request(root.path(), "printf '%s' \"$1\"; printf '%s' \"$PATH\"");
        request.arguments.extend(["sh".into(), native.clone()]);
        request.environment.insert("PATH".into(), native);
        let mut jobs = JobSupervisor::new(16);
        let id = jobs.start(request).unwrap();
        let mut bytes = Vec::new();
        let mut exited = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !exited {
            for event in jobs.poll() {
                match event {
                    JobEvent::Output { bytes: output, .. } => bytes.extend(output),
                    JobEvent::Exited {
                        job,
                        code,
                        timed_out,
                    } => {
                        assert_eq!(job, id);
                        assert_eq!(code, Some(0));
                        assert!(!timed_out);
                        exited = true;
                    }
                    JobEvent::Failed { message, .. } => panic!("{message}"),
                    JobEvent::Started { .. } => {}
                }
            }
            assert!(
                Instant::now() < deadline,
                "native argument job did not finish"
            );
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(bytes, b"path-\xffpath-\xff");
    }

    #[test]
    fn job_capacity_is_bounded_and_output_drains_before_completion() {
        let root = tempfile::tempdir().unwrap();
        let mut jobs = JobSupervisor::new(1);
        let id = jobs
            .start(request(&root.path().join("missing"), "true"))
            .expect("foreground must not stat/spawn");
        let deadline = Instant::now() + Duration::from_secs(3);
        while jobs.is_running(id) && Instant::now() < deadline {
            for event in jobs.poll() {
                assert!(matches!(event, JobEvent::Failed { .. }));
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(!jobs.is_running(id));
        let id = jobs
            .start(request(
                root.path(),
                "i=0; while [ $i -lt 32 ]; do printf 'line\n'; i=$((i+1)); done",
            ))
            .unwrap();
        thread::sleep(Duration::from_millis(30));
        let mut bytes = Vec::new();
        let mut terminal = 0;
        let deadline = Instant::now() + Duration::from_secs(3);
        while jobs.is_running(id) && Instant::now() < deadline {
            for event in jobs.poll() {
                match event {
                    JobEvent::Output { bytes: value, .. } => {
                        assert_eq!(terminal, 0);
                        bytes.extend(value)
                    }
                    JobEvent::Exited { code, .. } => {
                        assert_eq!(code, Some(0));
                        terminal += 1;
                        assert_eq!(bytes.len(), 160);
                    }
                    JobEvent::Started { .. } => {}
                    event => panic!("{event:?}"),
                }
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(terminal, 1);
        assert_eq!(bytes, b"line\n".repeat(32));
        let target = root.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let identity = crate::files::RootIdentity::capture(&target).unwrap();
        std::fs::rename(&target, root.path().join("old-target")).unwrap();
        std::fs::create_dir(&target).unwrap();
        let id = jobs
            .reserve()
            .unwrap()
            .submit_scoped(request(&target, "printf forbidden > executed"), identity)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut refused = false;
        while jobs.is_running(id) && Instant::now() < deadline {
            for event in jobs.poll() {
                if let JobEvent::Failed { message, .. } = event {
                    refused = message.contains("filesystem identity changed");
                }
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(refused);
        assert!(!target.join("executed").exists());
        // Simulated exhausted resident capacity; no filesystem or worker action is needed for this refusal.
        jobs.residents.store(256, Ordering::Release);
        assert!(matches!(jobs.reserve(), Err(JobError::Full)));
        jobs.residents.store(0, Ordering::Release);
        let loan = jobs.reserve().unwrap();
        drop(loan);
        assert_eq!(jobs.residents.load(Ordering::Acquire), 0);
        jobs.next = None;
        assert!(matches!(jobs.reserve(), Err(JobError::Exhausted)));
        jobs.next = Some(100);
        let id = jobs
            .start(request(root.path(), "while :; do :; done"))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut started = false;
        while !started && Instant::now() < deadline {
            started = jobs
                .poll()
                .iter()
                .any(|event| matches!(event,JobEvent::Started{job,..} if *job==id));
            thread::sleep(Duration::from_millis(2));
        }
        assert!(started);
        jobs.cancel(id).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while jobs.is_running(id) && Instant::now() < deadline {
            jobs.poll();
            thread::sleep(Duration::from_millis(2));
        }
        assert!(!jobs.is_running(id));
        jobs.begin_shutdown();
        assert!(matches!(jobs.reserve(), Err(JobError::Stopped)));
        let start = Instant::now();
        drop(jobs);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    fn submission(root: &std::path::Path, script: &str) -> (Submission, Arc<AtomicUsize>) {
        let residents = Arc::new(AtomicUsize::new(1));
        let mut request = request(root, script);
        request
            .environment
            .insert("PATH".into(), std::env::var_os("PATH").unwrap());
        (
            Submission {
                root: None,
                input: None,
                preflight: None,
                id: JobId(1),
                request,
                cancel: Arc::new(AtomicBool::new(false)),
                output_drain: Arc::new(OutputDrain::default()),
                slot: Arc::new(JobSlot(residents.clone())),
            },
            residents,
        )
    }

    #[test]
    fn queued_job_rechecks_cwd_symlink_containment() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let cwd = root.path().join("sub");
        std::fs::create_dir(&cwd).unwrap();
        let (mut queued, _) = submission(&cwd, "printf forbidden > marker");
        queued.root = Some(crate::files::RootIdentity::capture(root.path()).unwrap());
        std::fs::remove_dir(&cwd).unwrap();
        std::os::unix::fs::symlink(outside.path(), &cwd).unwrap();
        let mut engine = JobEngine::new(16);
        assert!(matches!(engine.start(queued), Err(JobError::RootChanged)));
        assert!(!outside.path().join("marker").exists());
    }

    #[test]
    fn completed_job_is_not_timed_out_when_polling_is_late() {
        let root = tempfile::tempdir().unwrap();
        let (mut queued, _) = submission(root.path(), "exit 0");
        queued.request.policy.timeout = Duration::from_millis(1);
        let mut engine = JobEngine::new(16);
        engine.start(queued).unwrap();
        let running = engine.jobs.get_mut(&JobId(1)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while job_exit_status(&mut running.child).unwrap().is_none() {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        assert_eq!(job_exit_status(&mut running.child).unwrap(), Some(Some(0)));
        running.started = Instant::now() - Duration::from_secs(1);
        let mut exited = false;
        while !engine.jobs.is_empty() {
            for event in engine.poll() {
                if let JobEvent::Exited {
                    code, timed_out, ..
                } = event
                {
                    assert_eq!(code, Some(0));
                    assert!(!timed_out);
                    exited = true;
                }
            }
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        assert!(exited);
    }

    #[test]
    fn oversized_output_lines_discard_the_tail_before_the_next_line() {
        let root = tempfile::tempdir().unwrap();
        let (mut queued, _) = submission(root.path(), "printf '123456789\\nok\\nabcdefghi'");
        queued.request.policy.maximum_line_bytes = 4;
        let mut engine = JobEngine::new(16);
        engine.start(queued).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut lines = Vec::new();
        while !engine.jobs.is_empty() {
            for event in engine.poll() {
                if let JobEvent::Output {
                    bytes, truncated, ..
                } = event
                {
                    lines.push((bytes, truncated));
                }
            }
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        assert_eq!(
            lines,
            vec![
                (b"1234".to_vec(), true),
                (b"ok\n".to_vec(), false),
                (b"abcd".to_vec(), true)
            ]
        );
    }

    #[test]
    fn job_cancellation_releases_escaped_descendants_pipe_readers() {
        struct Escaped(u32);
        impl Drop for Escaped {
            fn drop(&mut self) {
                crate::process::terminate_process_group(self.0);
            }
        }
        let root = tempfile::tempdir().unwrap();
        let (queued, residents) = submission(
            root.path(),
            "exec python3 -c 'import os,time\nif os.fork() == 0:\n os.setsid()\n print(os.getpid(), flush=True)\n time.sleep(30)\n'",
        );
        let cancel = queued.cancel.clone();
        let mut engine = JobEngine::new(1);
        engine.start(queued).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let escaped = loop {
            let output = engine.poll().into_iter().find_map(|event| match event {
                JobEvent::Output {
                    stream: JobStream::Stdout,
                    bytes,
                    ..
                } => Some(bytes),
                JobEvent::Output { bytes, .. } => {
                    panic!("fixture stderr: {}", String::from_utf8_lossy(&bytes))
                }
                _ => None,
            });
            if let Some(bytes) = output {
                break Escaped(String::from_utf8(bytes).unwrap().trim().parse().unwrap());
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        };
        cancel.store(true, Ordering::Release);
        while !engine.jobs.is_empty() || residents.load(Ordering::Acquire) != 0 {
            engine.poll();
            assert!(
                Instant::now() < deadline,
                "escaped descendant retained job readers"
            );
            thread::sleep(Duration::from_millis(2));
        }
        drop(escaped);
    }

    #[test]
    fn cancellation_releases_readers_without_draining_backpressured_output() {
        let root = tempfile::tempdir().unwrap();
        let (queued, _) = submission(root.path(), "printf 'one\ntwo\nthree\nfour\n'");
        let cancel = queued.cancel.clone();
        let mut engine = JobEngine::new(1);
        engine.start(queued).unwrap();
        assert!(matches!(
            engine
                .receiver
                .recv_timeout(Duration::from_secs(5))
                .unwrap(),
            JobEvent::Output { .. }
        ));
        cancel.store(true, Ordering::Release);
        engine.expire();
        let deadline = Instant::now() + Duration::from_secs(5);
        while engine.jobs[&JobId(1)].readers.load(Ordering::Acquire) != 0 {
            assert!(
                Instant::now() < deadline,
                "output delivery ignored cancellation"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn job_deadline_releases_descendant_pipes_after_parent_exit() {
        let root = tempfile::tempdir().unwrap();
        let (queued, residents) = submission(
            root.path(),
            "(while [ ! -e release ]; do sleep 0.05; done; printf alive > survived) >/dev/null & printf '%s\\n' \"$!\"",
        );
        let mut engine = JobEngine::new(16);
        engine.start(queued).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut helper = None;
        while helper.is_none() || engine.jobs[&JobId(1)].status.is_none() {
            for event in engine.poll() {
                if let JobEvent::Output { bytes, .. } = event {
                    let pid = std::str::from_utf8(&bytes).unwrap().trim().parse().unwrap();
                    helper = Some(nix::unistd::Pid::from_raw(pid));
                }
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        struct Helper(nix::unistd::Pid);
        impl Drop for Helper {
            fn drop(&mut self) {
                let _ = nix::sys::signal::kill(self.0, nix::sys::signal::Signal::SIGKILL);
            }
        }
        let _helper = Helper(helper.unwrap());
        assert_ne!(engine.jobs[&JobId(1)].readers.load(Ordering::Acquire), 0);
        let mut stderr_abandoned = false;
        let mut exited = false;
        while !engine.jobs.is_empty() || residents.load(Ordering::Acquire) != 0 {
            for event in engine.poll() {
                match event {
                    JobEvent::Output {
                        stream,
                        abandoned: true,
                        ..
                    } => {
                        assert_eq!(stream, JobStream::Stderr);
                        assert!(!exited);
                        stderr_abandoned = true;
                    }
                    JobEvent::Exited {
                        code, timed_out, ..
                    } => {
                        assert_eq!(code, Some(0));
                        assert!(!timed_out);
                        assert!(stderr_abandoned);
                        exited = true;
                    }
                    other => panic!("unexpected job event: {other:?}"),
                }
            }
            assert!(
                Instant::now() < deadline,
                "descendant pipes outlived the job deadline"
            );
            thread::sleep(Duration::from_millis(2));
        }
        assert!(stderr_abandoned && exited);
        std::fs::write(root.path().join("release"), []).unwrap();
        while !std::fs::read(root.path().join("survived")).is_ok_and(|data| data == b"alive") {
            assert!(Instant::now() < deadline, "abandonment stopped the helper");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn total_cap_grace_releases_both_pipes_without_stopping_the_helper() {
        let root = tempfile::tempdir().unwrap();
        let (mut queued, _) = submission(
            root.path(),
            "(while [ -d gate ]; do sleep 0.05; done; printf alive > survived) & printf 'abc\ndef\n'",
        );
        let gate = root.path().join("gate");
        std::fs::create_dir(&gate).unwrap();
        queued.request.policy.maximum_output_bytes = 4;
        queued.request.policy.timeout = Duration::from_secs(30);
        let mut engine = JobEngine::new(1);
        engine.start(queued).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        let mut records = Vec::new();
        let mut abandoned = AbandonedStreams::default();
        let mut exited = false;
        while !exited {
            for event in engine.poll() {
                match event {
                    JobEvent::Output {
                        stream,
                        abandoned: true,
                        ..
                    } => {
                        assert!(!exited);
                        assert_eq!(records, [(b"abc\n".to_vec(), false), (Vec::new(), true)]);
                        abandoned.insert(stream);
                    }
                    JobEvent::Output {
                        stream,
                        bytes,
                        truncated,
                        ..
                    } => {
                        assert_eq!(stream, JobStream::Stdout);
                        records.push((bytes, truncated));
                    }
                    JobEvent::Exited {
                        code, timed_out, ..
                    } => {
                        assert_eq!(code, Some(0));
                        assert!(!timed_out);
                        assert!(abandoned.contains(JobStream::Stdout));
                        assert!(abandoned.contains(JobStream::Stderr));
                        exited = true;
                    }
                    other => panic!("unexpected job event: {other:?}"),
                }
            }
            assert!(
                Instant::now() < until,
                "both pipes should hit the short grace"
            );
            thread::sleep(Duration::from_millis(2));
        }
        std::fs::remove_dir(gate).unwrap();
        while !std::fs::read(root.path().join("survived")).is_ok_and(|data| data == b"alive") {
            assert!(Instant::now() < until, "abandonment stopped the helper");
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn oversized_line_does_not_cut_off_later_in_budget_records() {
        let root = tempfile::tempdir().unwrap();
        let (mut queued, _) = submission(
            root.path(),
            "(printf 'oversized'; sleep 0.4; printf '\\nready\\n') &",
        );
        queued.request.policy.maximum_line_bytes = 6;
        queued.request.policy.maximum_output_bytes = 64;
        let mut engine = JobEngine::new(16);
        engine.start(queued).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        let mut records = Vec::new();
        while !engine.jobs.is_empty() {
            for event in engine.poll() {
                match event {
                    JobEvent::Output {
                        bytes,
                        truncated,
                        abandoned,
                        ..
                    } => {
                        assert!(!abandoned);
                        records.push((bytes, truncated));
                    }
                    JobEvent::Exited {
                        code, timed_out, ..
                    } => {
                        assert_eq!(code, Some(0));
                        assert!(!timed_out);
                    }
                    other => panic!("unexpected job event: {other:?}"),
                }
            }
            assert!(Instant::now() < until);
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            records,
            [(b"oversi".to_vec(), true), (b"ready\n".to_vec(), false)]
        );
    }

    #[test]
    fn helper_can_write_late_output_and_stay_alive_after_the_leader_exits() {
        struct Helper(nix::unistd::Pid);
        impl Drop for Helper {
            fn drop(&mut self) {
                let _ = nix::sys::signal::kill(self.0, nix::sys::signal::Signal::SIGKILL);
            }
        }

        let root = tempfile::tempdir().unwrap();
        let (mut queued, _) = submission(
            root.path(),
            "(sleep 1; printf 'ready\\n'; exec sleep 30 >/dev/null 2>&1) & printf '%s\\n' \"$!\"",
        );
        queued.request.policy.timeout = Duration::from_secs(5);
        let mut engine = JobEngine::new(16);
        engine.start(queued).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut helper = None;
        let mut ready = false;
        let mut exited = false;
        while !exited {
            for event in engine.poll() {
                match event {
                    JobEvent::Output {
                        bytes, truncated, ..
                    } => {
                        assert!(!truncated, "in-budget helper output was abandoned");
                        if bytes == b"ready\n" {
                            ready = true;
                        } else {
                            let pid = std::str::from_utf8(&bytes).unwrap().trim().parse().unwrap();
                            helper = Some(Helper(nix::unistd::Pid::from_raw(pid)));
                        }
                    }
                    JobEvent::Exited {
                        code, timed_out, ..
                    } => {
                        assert_eq!(code, Some(0));
                        assert!(!timed_out);
                        exited = true;
                    }
                    other => panic!("unexpected job event: {other:?}"),
                }
            }
            assert!(Instant::now() < deadline, "helper did not finish writing");
            thread::yield_now();
        }
        assert!(ready, "late helper output was lost");
        nix::sys::signal::kill(helper.expect("helper PID").0, None).unwrap();
    }

    #[test]
    fn short_lines_drain_past_deadline_after_exit_with_a_full_output_queue() {
        let root = tempfile::tempdir().unwrap();
        let (mut queued, _) = submission(
            root.path(),
            "i=0; while [ \"$i\" -lt 1500 ]; do printf 'x\\n'; i=$((i + 1)); done",
        );
        queued.request.policy.timeout = Duration::from_secs(10);
        let mut engine = JobEngine::new(64);
        engine.start(queued).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let running = engine.jobs.get_mut(&JobId(1)).unwrap();
        while job_exit_status(&mut running.child).unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "leader did not exit with the queue full"
            );
            thread::yield_now();
        }
        // Expire the job after observing its exit, without emptying the output queue.
        running.observe_exit(Instant::now()).unwrap();
        assert_eq!(running.status, Some(Some(0)));
        running.started = Instant::now() - running.timeout;

        let mut lines = 0;
        let mut exited = false;
        while !exited {
            for event in engine.poll() {
                match event {
                    JobEvent::Output {
                        bytes, truncated, ..
                    } => {
                        assert!(!truncated);
                        assert_eq!(bytes, b"x\n");
                        lines += 1;
                    }
                    JobEvent::Exited {
                        code, timed_out, ..
                    } => {
                        assert_eq!(code, Some(0));
                        assert!(!timed_out);
                        exited = true;
                    }
                    other => panic!("unexpected job event: {other:?}"),
                }
            }
            assert!(Instant::now() < deadline, "short lines did not drain");
            thread::yield_now();
        }
        assert_eq!(lines, 1500);
    }

    #[test]
    fn clipped_streaming_job_finishes_when_detached_writer_outlives_leader() {
        let root = tempfile::tempdir().unwrap();
        let (mut queued, residents) = submission(
            root.path(),
            "exec python3 -c 'import os,signal\nif os.fork() == 0:\n os.setsid()\n os.close(2)\n signal.alarm(5)\n while True: os.write(1, b\"x\" * 4096)\n'",
        );
        let (_input, reader) = mpsc::sync_channel(1);
        queued.input = Some(reader);
        queued.request.policy.maximum_output_bytes = 16;
        queued.request.policy.timeout = Duration::from_secs(2);
        let mut engine = JobEngine::new(16);
        engine.start(queued).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut clipped = false;
        let mut captured = 0;
        let mut exited = false;
        while !exited || residents.load(Ordering::Acquire) != 0 {
            for event in engine.poll() {
                match event {
                    JobEvent::Output {
                        bytes, truncated, ..
                    } => {
                        captured += bytes.len();
                        clipped |= truncated;
                    }
                    JobEvent::Exited {
                        code, timed_out, ..
                    } => {
                        assert_eq!(code, Some(0));
                        assert!(!timed_out);
                        assert!(clipped, "capture must be marked incomplete before exit");
                        exited = true;
                    }
                    other => panic!("unexpected job event: {other:?}"),
                }
            }
            assert!(
                Instant::now() < deadline,
                "detached writer kept the clipped job running past its deadline"
            );
            thread::yield_now();
        }
        assert_eq!(captured, 16);
    }

    #[test]
    fn job_cancellation_releases_descendants_after_parent_exit() {
        let root = tempfile::tempdir().unwrap();
        let (queued, residents) = submission(root.path(), "sleep 30 & printf spawned");
        let cancel = queued.cancel.clone();
        let mut engine = JobEngine::new(16);
        engine.start(queued).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while engine.jobs[&JobId(1)].status.is_none() {
            engine.poll();
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        assert_ne!(engine.jobs[&JobId(1)].readers.load(Ordering::Acquire), 0);
        let running = engine.jobs.get_mut(&JobId(1)).unwrap();
        let pid = rustix::process::Pid::from_raw(running.child.id() as i32).unwrap();
        // Observation must not reap the leader while cleanup can still signal its group.
        assert_eq!(job_exit_status(&mut running.child).unwrap(), Some(Some(0)));
        cancel.store(true, Ordering::Release);
        while !engine.jobs.is_empty() || residents.load(Ordering::Acquire) != 0 {
            engine.poll();
            assert!(Instant::now() < deadline, "descendant retained job readers");
            thread::sleep(Duration::from_millis(2));
        }
        use rustix::process::{WaitId, WaitIdOptions, waitid};
        assert!(matches!(
            waitid(
                WaitId::Pid(pid),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG
            ),
            Err(rustix::io::Errno::CHILD)
        ));
    }

    #[test]
    fn job_cancellation_kills_descendants_ignoring_sigterm() {
        let root = tempfile::tempdir().unwrap();
        let (queued, residents) = submission(
            root.path(),
            "(trap '' TERM; printf 'ready\\n'; exec sleep 30) & wait",
        );
        let cancel = queued.cancel.clone();
        let mut engine = JobEngine::new(16);
        engine.start(queued).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !engine
            .poll()
            .iter()
            .any(|event| matches!(event, JobEvent::Output { bytes, .. } if bytes == b"ready\n"))
        {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        cancel.store(true, Ordering::Release);
        while !engine.jobs.is_empty() || residents.load(Ordering::Acquire) != 0 {
            engine.poll();
            assert!(
                Instant::now() < deadline,
                "SIGTERM-resistant descendant retained job readers"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn job_launch_preserves_original_executable_invocation() {
        let root = tempfile::tempdir().unwrap();
        let invocation = root.path().join("custom-sh");
        std::os::unix::fs::symlink("/bin/sh", &invocation).unwrap();
        let (mut queued, _) = submission(root.path(), "printf '%s' \"$0\"");
        queued.request.program = crate::files::canonicalize(&invocation).unwrap();
        queued.request.argv0 = Some(invocation.clone());
        let mut engine = JobEngine::new(16);
        engine.start(queued).unwrap();
        let mut output = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !engine.jobs.is_empty() {
            for event in engine.poll() {
                if let JobEvent::Output { bytes, .. } = event {
                    output.extend(bytes);
                }
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            String::from_utf8(output).unwrap(),
            invocation.to_str().unwrap()
        );
    }
}
