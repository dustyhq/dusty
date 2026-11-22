//! Private debugger runtime, controls, breakpoint/watch policy and sourced selections.
mod launch;
mod presets;
use crate::app::windows::NavigationStamp;
use crate::app::workspace::Workspace;
use crate::app::workspace::WorkspaceVersion;
use crate::config::DebugConfig;
use crate::config::TerminalProfileConfig;
use crate::config::TerminalsConfig;
use crate::debugger::DapBreakpointState;
use crate::debugger::DapClient;
use crate::debugger::DapContext;
use crate::debugger::DapControl;
use crate::debugger::DapError;
use crate::debugger::DapEvent;
use crate::debugger::DapReplyDelivery;
use crate::debugger::DapReplyOutcome;
use crate::debugger::DapResponse;
use crate::debugger::DapSourceBreakpoint;
use crate::debugger::DapStackFrame;
use crate::debugger::DapTerminalReply;
use crate::editor::decorations::{Gravity, MarkId};
use crate::editor::text::ByteRange;
use crate::editor::{BufferId, EditorState};
use crate::persistence::PersistedWatch;
use crate::persistence::{PersistedBreakpoint, PersistedBreakpointId};
use crate::process::PtyId;
pub(in crate::app) use presets::PresetLaunch;

pub(in crate::app) const MAXIMUM_BREAKPOINTS: usize = 10_000;
pub(in crate::app) const MAXIMUM_WATCHES: usize = 1_000;
pub(in crate::app) const MAXIMUM_DEBUG_ARGUMENTS: usize = 4096;
pub(in crate::app) const MAXIMUM_DEBUG_ENVIRONMENT: usize = 4096;

pub(in crate::app) use launch::DebugIntent;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub(in crate::app) struct DebugKey {
    epoch: Arc<()>,
    index: usize,
}
pub(in crate::app) enum DebugResponse {
    Picker {
        kind: &'static str,
        rows: Vec<DebugRow>,
    },
    Result(DapResponse),
    Stale,
    /// Part of following a stop: the panes show it, nothing opens.
    Absorbed,
    /// The program stopped here, so the editor goes there.
    StoppedAt(DebugLocation),
    StoppedSource(DapStackFrame),
}
/// What is being fetched on its own after the program stops, so the source, stack and variables
/// are there without asking. Pickers remain for what the user asks for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StopFollow {
    Idle,
    Threads(Option<i64>),
    Stack,
    Scopes,
    Variables,
}
pub(in crate::app) struct DebugRow {
    pub label: String,
    pub detail: String,
    pub key: DebugKey,
}
enum Choice {
    Control(DapControl),
    Frame(DapStackFrame),
    /// A variable with no children: choosing it shows its value.
    Value(String),
}
pub(in crate::app) struct DebugSelection {
    expansion: Vec<i64>,
    source: DapContext,
    choice: Choice,
}
impl DebugSelection {
    pub(in crate::app) fn value(&self) -> Option<&str> {
        match &self.choice {
            Choice::Value(value) => Some(value),
            _ => None,
        }
    }
}
struct Choices {
    epoch: Arc<()>,
    source: DapContext,
    values: Vec<Choice>,
}
pub(in crate::app) struct SourceTarget {
    pub navigation: NavigationStamp,
    pub capture: Arc<()>,
    pub line: u64,
    pub column: u64,
}
struct Source {
    context: DapContext,
    target: SourceTarget,
}
struct PendingRequest {
    epoch: u64,
    submission: Option<u64>,
    frame: Option<i64>,
    automatic: bool,
}
pub(in crate::app) struct DebugLocation {
    pub path: PathBuf,
    pub line: u64,
    pub column: u64,
}
struct TerminalPolicy {
    name: String,
    profile: TerminalProfileConfig,
    cwd: PathBuf,
}
/// How many variables one page asks for: expansion is bounded rather than eager.
const VARIABLE_PAGE: u64 = 200;
/// How deep the inspector expands before it refuses, so a pathological chain cannot recurse away.
const MAX_EXPANSION_DEPTH: usize = 32;
/// WHAT a session is attached to or running. An attach must PRESERVE the target's identity: the
/// editor names the target it was given, keeps no program for it because it did not start that
/// process, and never presents an attached target as one it launched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::app) struct DapTargetIdentity {
    pub configuration: Option<String>,
    pub adapter: String,
    pub request: crate::config::DebugRequestKind,
    /// The program a launch runs. An ATTACH has none.
    pub program: Option<std::path::PathBuf>,
    /// Automatic source selection must keep targeting the same package on restart.
    pub automatic_program: bool,
    pub document: Option<std::path::PathBuf>,
    /// The target an ATTACH names, taken from the configuration's own arguments.
    pub target: Option<String>,
}
pub(super) struct Debugger {
    service: DapClient,
    launch: Option<launch::Launch>,
    install_launch: Option<PresetLaunch>,
    last_preset: Option<PresetLaunch>,
    cargo_build: Option<presets::CargoBuild>,
    build_jobs: Option<crate::process::JobSupervisor>,
    console: VecDeque<crate::debugger::DapConsoleEntry>,
    breakpoints: BTreeMap<PathBuf, Vec<SourceBreakpoint>>,
    last_breakpoint_id: u64,
    discarded_breakpoint_marks: Vec<BreakpointLocation>,
    execution_mark: Option<(PathBuf, u64, BreakpointLocation)>,
    /// Settings changed while persistence is loading must win over the saved values.
    changed_breakpoints: Option<BTreeSet<(PathBuf, u64)>>,
    changed_watches: Option<BTreeSet<String>>,
    latest_breakpoints: BTreeMap<PathBuf, BreakpointRequest>,
    /// Changed sources that still need to be sent to the adapter.
    breakpoint_refresh: BTreeSet<PathBuf>,
    breakpoint_picker_dirty: bool,
    /// The visible console row that an unterminated output chunk continues.
    output_line: Option<usize>,
    stop_follow: StopFollow,
    follow_request: Option<u64>,
    answer_request: Option<u64>,
    /// Why the program is paused, while it is.
    stop_reason: Option<String>,
    /// Where the program is paused, while it is.
    execution: Option<(PathBuf, u64)>,
    variables: Vec<String>,
    watch_values: BTreeMap<String, String>,
    /// One entry per evaluation in flight, in order: the watch it answers, or none for the console.
    evaluations: VecDeque<Option<String>>,
    /// How the session ended, when it has: leaving is not killing.
    end_state: Option<crate::debugger::DapSessionEnd>,
    /// Which STOP the session is currently in, incremented whenever execution runs on or stops
    /// again. Data captured at one stop is stamped with it, so a value read at an earlier stop
    /// can never be presented as current at a later one.
    stop_epoch: u64,
    /// The stop at which each stopped-data request was ISSUED. An answer whose request is known to
    /// belong to a DIFFERENT stop belongs to that stop, and is refused rather than presented as
    /// current. A request with no record is presented as before: the session cannot prove it
    /// belongs to another stop, so it does not pretend to know.
    issued_stack: VecDeque<u64>,
    requests: HashMap<u64, PendingRequest>,
    watch_refresh: VecDeque<String>,
    issued_scopes: VecDeque<(i64, u64)>,
    /// Pages of variables asked for and not yet answered, oldest first: the protocol's variables
    /// answer carries no request id, so a page is matched to its answer in the order it was sent.
    pending_variable_pages: VecDeque<(i64, u64, u64)>,
    /// What the adapter last reported about the process: threads, and the STACK of the current
    /// stop. Both are stopped data, so both go when execution runs on.
    threads: Vec<String>,
    stopped_thread: Option<i64>,
    stack: Vec<String>,
    /// The references OPEN on the expansion path, innermost last. A reference already on this path
    /// is shown as a reference and is never expanded again.
    expansion: Vec<i64>,
    /// The adapter the session is talking to, remembered from its events so a console record can
    /// name the actor that produced it.
    actor: Option<String>,
    /// The frame the console is BOUND to: the frame the user selected from the stack.
    console_frame: Option<i64>,
    /// Identity for each console submission, so a late answer updates ITS OWN record.
    next_submission: u64,
    /// Submissions still waiting for an answer: (id, stop, frame, actor) captured at submission.
    pending_submissions: VecDeque<(u64, u64, Option<i64>, Option<String>)>,
    /// Requests awaiting an answer, oldest first. A `setBreakpoints` response carries no request
    /// sequence, so answers are matched to requests in the order they were sent.
    watches: Vec<String>,
    configuration: Option<String>,
    /// What this session is attached to or running, RETAINED across a successful start so an
    /// attached target's identity is not lost the moment the request goes out.
    target: Option<DapTargetIdentity>,
    terminal_profile: Option<TerminalPolicy>,
    /// Started from the active file, with an adapter from the tool catalog. Such a session has no
    /// configuration naming a terminal, so the adapter's program runs in one as it asks.
    adapter_runs_terminal: bool,
    terminal_replies: HashMap<PtyId, DapTerminalReply>,
    reply_deliveries: Vec<(String, i64, DapReplyDelivery)>,
    policy: DebugConfig,
    scope: WorkspaceVersion,
    choices: Option<Choices>,
    event_source: Option<DapContext>,
    source: Option<Source>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum BreakpointRequest {
    Pending(u64, u64),
    Acknowledged(u64, u64),
    Rejected(u64, u64),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(in crate::app) struct BreakpointId(u64);

pub(in crate::app) enum BreakpointConditionTarget {
    Existing {
        path: PathBuf,
        identity: BreakpointId,
    },
    New {
        path: PathBuf,
        location: BreakpointLocation,
    },
}

impl BreakpointConditionTarget {
    pub(in crate::app) fn cancel(self, editor: &mut EditorState) {
        if let Self::New { location, .. } = self {
            location.remove_mark(editor);
        }
    }
}

#[derive(Clone, Copy)]
pub(in crate::app) enum BreakpointLocation {
    Closed(u64),
    Open {
        buffer: BufferId,
        mark: MarkId,
        // Used only if the mark disappears, not to resolve a location in an open buffer.
        last_line: u64,
    },
}

impl BreakpointLocation {
    pub(in crate::app) fn attach(path: &Path, line: u64, editor: &mut EditorState) -> Self {
        let location = editor
            .buffers
            .iter_mut()
            .find(|(_, buffer)| buffer.path.as_deref() == Some(path))
            .and_then(|(id, buffer)| {
                let line_index = usize::try_from(line.checked_sub(1)?).ok()?;
                let position = buffer.text.line_to_byte(line_index).ok()?;
                let mark = buffer.decorations.insert(
                    &buffer.text,
                    "debug",
                    ByteRange::empty(position),
                    Gravity::Right,
                    Gravity::Right,
                )?;
                Some(Self::Open {
                    buffer: id,
                    mark,
                    last_line: line,
                })
            });
        location.unwrap_or(Self::Closed(line))
    }

    fn live_line(self, path: &Path, editor: &EditorState) -> Option<u64> {
        let Self::Open { buffer, mark, .. } = self else {
            return None;
        };
        let buffer = editor.buffers.get(buffer)?;
        if buffer.path.as_deref() != Some(path) {
            return None;
        }
        let position = buffer.decorations.range(mark)?.start;
        Some(buffer.text.byte_to_line(position).ok()? as u64 + 1)
    }

    fn last_line(self) -> u64 {
        match self {
            Self::Closed(line)
            | Self::Open {
                last_line: line, ..
            } => line,
        }
    }

    pub(in crate::app) fn line(self, path: &Path, editor: &EditorState) -> u64 {
        self.live_line(path, editor)
            .unwrap_or_else(|| self.last_line())
    }

    fn sync(&mut self, path: &Path, editor: &mut EditorState) {
        if let Some(line) = self.live_line(path, editor) {
            if let Self::Open { last_line, .. } = self {
                *last_line = line;
            }
        } else {
            self.remove_mark(editor);
            *self = Self::attach(path, self.last_line(), editor);
        }
    }

    pub(in crate::app) fn remove_mark(self, editor: &mut EditorState) {
        if let Self::Open { buffer, mark, .. } = self
            && let Some(buffer) = editor.buffers.get_mut(buffer)
        {
            buffer.decorations.remove(mark);
        }
    }
}

fn valid_breakpoint_text(text: &str) -> bool {
    text.len() <= 64 * 1024 && !text.contains('\0')
}

struct SourceBreakpoint {
    // The condition prompt keeps this identity even if the source mark is reattached.
    identity: BreakpointId,
    persisted_id: PersistedBreakpointId,
    location: BreakpointLocation,
    column: Option<u64>,
    condition: Option<String>,
    hit_condition: Option<String>,
    log_message: Option<String>,
    disabled: bool,
    verdict: Option<DapBreakpointState>,
    // Last confirmed line while a replacement request is pending.
    pending_display_line: Option<u64>,
}

impl SourceBreakpoint {
    fn request_row(&self, path: &Path, editor: &EditorState) -> DapSourceBreakpoint {
        DapSourceBreakpoint {
            path: path.into(),
            line: self.location.line(path, editor),
            column: self.column,
            condition: self.condition.clone(),
            hit_condition: self.hit_condition.clone(),
            log_message: self.log_message.clone(),
        }
    }

    /// Where the breakpoint is shown, which may differ from its requested mark.
    fn display_line(&self, path: &Path, editor: &EditorState) -> u64 {
        match self.state() {
            Some(DapBreakpointState::Verified { line }) => line,
            Some(DapBreakpointState::Requested { accepted: true }) => self
                .pending_display_line
                .unwrap_or_else(|| self.location.line(path, editor)),
            _ => self.location.line(path, editor),
        }
    }

    fn mark_pending(&mut self) {
        if let Some(DapBreakpointState::Verified { line }) = self.verdict {
            self.pending_display_line = Some(line);
        }
        self.verdict = Some(DapBreakpointState::Requested { accepted: true });
    }

    fn set_verdict(&mut self, verdict: Option<DapBreakpointState>) -> bool {
        let had_pending_line = self.pending_display_line.take().is_some();
        std::mem::replace(&mut self.verdict, verdict) != verdict || had_pending_line
    }

    fn state(&self) -> Option<DapBreakpointState> {
        if self.disabled {
            Some(DapBreakpointState::Disabled)
        } else {
            self.verdict
        }
    }
}

impl Debugger {
    pub(in crate::app) fn new(policy: &DebugConfig, scope: WorkspaceVersion) -> Self {
        Self {
            service: DapClient::new(),
            launch: None,
            install_launch: None,
            last_preset: None,
            cargo_build: None,
            build_jobs: None,
            console: VecDeque::new(),
            breakpoints: BTreeMap::new(),
            last_breakpoint_id: 0,
            discarded_breakpoint_marks: Vec::new(),
            execution_mark: None,
            changed_breakpoints: Default::default(),
            changed_watches: Default::default(),
            latest_breakpoints: BTreeMap::new(),
            breakpoint_refresh: std::collections::BTreeSet::new(),
            breakpoint_picker_dirty: false,
            output_line: None,
            end_state: None,
            stop_epoch: 0,
            issued_stack: VecDeque::new(),
            requests: HashMap::new(),
            watch_refresh: VecDeque::new(),
            issued_scopes: VecDeque::new(),
            pending_variable_pages: VecDeque::new(),
            threads: Vec::new(),
            stopped_thread: None,
            stack: Vec::new(),
            expansion: Vec::new(),
            actor: None,
            console_frame: None,
            next_submission: 0,
            pending_submissions: VecDeque::new(),
            stop_follow: StopFollow::Idle,
            follow_request: None,
            answer_request: None,
            stop_reason: None,
            execution: None,
            variables: Vec::new(),
            watch_values: BTreeMap::new(),
            evaluations: VecDeque::new(),
            watches: Vec::new(),
            configuration: None,
            target: None,
            terminal_profile: None,
            adapter_runs_terminal: false,
            terminal_replies: HashMap::new(),
            reply_deliveries: Vec::new(),
            policy: policy.clone(),
            scope,
            choices: None,
            event_source: None,
            source: None,
        }
    }
    pub(in crate::app) fn refresh(&mut self, policy: &DebugConfig, scope: WorkspaceVersion) {
        if self.policy != *policy || self.scope != scope {
            let _ = self.stop();
            self.policy = policy.clone();
            self.scope = scope;
            self.watches.truncate(MAXIMUM_WATCHES);
        }
    }
    pub(in crate::app) fn stop(&mut self) -> Result<(), DapError> {
        let pending_launch = self.launch.take().is_some()
            || self.install_launch.is_some()
            || self.cargo_build.is_some();
        self.cancel_preset_launch();
        self.latest_breakpoints.clear();
        self.breakpoint_refresh.clear();
        for row in self.breakpoints.values_mut().flatten() {
            row.set_verdict(None);
        }
        self.breakpoint_picker_dirty = true;
        self.output_line = None;
        self.clear_paused_state();
        self.stop_reason = None;
        self.choices = None;
        self.event_source = None;
        self.source = None;
        self.reject_terminals("requesting debugger stopped");
        self.terminal_profile = None;
        self.adapter_runs_terminal = false;
        match self.service.stop() {
            Err(DapError::NotRunning) if pending_launch => Ok(()),
            result => result,
        }
    }
    pub(in crate::app) fn reset(&mut self, editor: &mut EditorState) {
        let _ = self.stop();
        self.event_source = None;
        self.console.clear();
        self.configuration = None;
        self.last_preset = None;
        self.remove_discarded_breakpoint_marks(editor);
        for rows in std::mem::take(&mut self.breakpoints).into_values() {
            for row in rows {
                row.location.remove_mark(editor);
            }
        }
        self.watches.clear();
    }
    pub(in crate::app) fn begin_shutdown(&mut self) {
        let _ = self.stop();
        self.service.begin_shutdown();
        if let Some(jobs) = self.build_jobs.as_mut() {
            jobs.begin_shutdown();
        }
    }
    pub(in crate::app) fn pending(&self) -> usize {
        self.service.pending() + self.reply_deliveries.len()
    }
    pub(in crate::app) fn is_running(&self) -> bool {
        self.service.is_running()
    }
    pub(in crate::app) fn active_adapter(&self) -> Option<&str> {
        self.service.active_adapter()
    }
    pub(in crate::app) fn configuration(&self) -> Option<&str> {
        self.configuration.as_deref()
    }
    /// The terminal for a program an auto-started session's adapter asks to run, so that the
    /// program can read what the user types.
    pub(in crate::app) fn adapter_terminal(
        &self,
        cwd: &std::path::Path,
        arguments: &[String],
        environment: &HashMap<String, String>,
    ) -> Option<TerminalProfileConfig> {
        if !self.adapter_runs_terminal {
            return None;
        }
        let (program, arguments) = arguments.split_first()?;
        // DAP argv[0] may name a PATH executable. Terminal preflight expects a
        // concrete path; joining `node` to the workspace would look for a source file.
        let path = PathBuf::from(program);
        let path = if path.components().count() == 1 && !path.is_absolute() {
            crate::tools::discover_executable(program).unwrap_or(path)
        } else {
            path
        };
        Some(TerminalProfileConfig {
            program: path,
            arguments: arguments.to_vec(),
            cwd: (!cwd.as_os_str().is_empty()).then(|| cwd.into()),
            environment: environment
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            columns: 80,
            rows: 24,
            float: false,
            close_on_exit: false,
        })
    }
    pub(in crate::app) fn terminal_request(
        &self,
        cwd: &std::path::Path,
        arguments: &[String],
        environment: &HashMap<String, String>,
        policy: &TerminalsConfig,
    ) -> Result<&str, String> {
        let original = self
            .terminal_profile
            .as_ref()
            .ok_or("runInTerminal is not enabled by the original debug configuration")?;
        if !self
            .service
            .context()
            .is_some_and(|c| self.service.current(&c))
            || policy.profiles.get(&original.name) != Some(&original.profile)
        {
            return Err("original debug terminal profile changed".into());
        }
        if cwd != original.cwd
            || arguments.first().map(String::as_str) != original.profile.program.to_str()
            || arguments.get(1..) != Some(original.profile.arguments.as_slice())
            || environment.len() != original.profile.environment.len()
            || original
                .profile
                .environment
                .iter()
                .any(|(k, v)| environment.get(k) != Some(v))
        {
            return Err(
                "runInTerminal request differs from the original configured terminal profile"
                    .into(),
            );
        }
        Ok(&original.name)
    }
    pub(in crate::app) fn reply_failures(&mut self) -> Vec<String> {
        let mut errors = Vec::new();
        self.reply_deliveries.retain(|(adapter, sequence, delivery)| match delivery.outcome() {
            DapReplyOutcome::Written => false,
            DapReplyOutcome::Failed | DapReplyOutcome::Unknown => { errors.push(format!("DAP {adapter} reverse request {sequence}: {:?}; no peer acknowledgement claimed", delivery.outcome())); false },
            _ => true,
        });
        errors
    }
    pub(in crate::app) fn control(&mut self, control: DapControl) -> Result<(), DapError> {
        self.control_request(control).map(|_| ())
    }
    fn control_request(&mut self, control: DapControl) -> Result<Option<u64>, DapError> {
        // Source requests require a private selection and an original activation target.
        if matches!(control, DapControl::Source { .. }) {
            return Err(DapError::StaleContext);
        }
        let epoch = self.stop_epoch;
        // A reference ALREADY OPEN on this path is shown as a reference and never expanded again.
        // The refusal happens HERE, before anything is sent, so no answer can materialise it
        // recursively. A later page of the node already open is not a re-entry: it continues what
        // is already displayed.
        if let DapControl::Variables {
            reference,
            start,
            count,
        } = &control
        {
            let (reference, start, count) = (*reference, *start, *count);
            let open = self.expansion.contains(&reference);
            let continuation = start > 0 && self.expansion.last() == Some(&reference);
            if !continuation && (open || self.expansion.len() >= MAX_EXPANSION_DEPTH) {
                self.note_refused_expansion(reference);
                return Ok(None);
            }
            let id = self.send_control(control)?;
            if !continuation {
                self.expansion.push(reference);
            }
            self.pending_variable_pages
                .push_back((reference, start, count));
            return Ok(Some(id));
        }
        let evaluation = match &control {
            DapControl::Evaluate {
                expression,
                context,
                ..
            } => Some((context == "watch").then(|| expression.clone())),
            _ => None,
        };
        let stack = matches!(&control, DapControl::StackTrace { .. });
        let scopes = match &control {
            DapControl::Scopes { frame_id } => Some(*frame_id),
            _ => None,
        };
        let id = self.send_control(control)?;
        self.evaluations.extend(evaluation);
        // Recorded only once the request is actually away, so a refused send leaves nothing that a
        // later answer could be matched against.
        self.note_issued(stack, scopes, epoch);
        Ok(Some(id))
    }
    fn send_control(&mut self, control: DapControl) -> Result<u64, DapError> {
        let repl = matches!(&control, DapControl::Evaluate { context, .. } if context != "watch");
        let breakpoint_path = match &control {
            DapControl::SetBreakpoints { path, .. } => Some(path.clone()),
            _ => None,
        };
        let result = self
            .service
            .context()
            .ok_or(DapError::NotRunning)
            .and_then(|context| {
                self.service
                    .control_tracked(&context, control)
                    .map(|id| (id, context.connection()))
            });
        match result {
            Ok((id, connection)) => {
                if let Some(path) = breakpoint_path {
                    self.latest_breakpoints
                        .insert(path, BreakpointRequest::Pending(id, connection));
                }
                let submission = repl
                    .then(|| self.pending_submissions.back().map(|p| p.0))
                    .flatten();
                self.requests.insert(
                    id,
                    PendingRequest {
                        epoch: self.stop_epoch,
                        submission,
                        frame: self.console_frame,
                        automatic: false,
                    },
                );
                Ok(id)
            }
            Err(error) => {
                if repl {
                    self.pending_submissions.pop_back();
                }
                Err(error)
            }
        }
    }
    pub(in crate::app) fn prepare_answer(&mut self, id: u64, request: &DapControl, failed: bool) {
        let pending = self.requests.remove(&id);
        let epoch = pending
            .as_ref()
            .map_or(self.stop_epoch, |pending| pending.epoch);
        let submission = pending.and_then(|pending| pending.submission);
        match request {
            DapControl::Evaluate {
                expression,
                context,
                ..
            } => {
                let watch = (context == "watch").then(|| expression.clone());
                if let Some(position) = self.evaluations.iter().position(|old| old == &watch) {
                    self.evaluations.remove(position);
                }
                self.evaluations.push_front(watch);
                if let Some(submission) = submission
                    && let Some(position) = self
                        .pending_submissions
                        .iter()
                        .position(|p| p.0 == submission)
                    && let Some(pending) = self.pending_submissions.remove(position)
                    && !failed
                {
                    self.pending_submissions.push_front(pending);
                }
            }
            DapControl::StackTrace { .. } => {
                self.issued_stack.clear();
                if !failed {
                    self.issued_stack.push_back(epoch);
                }
            }
            DapControl::Scopes { frame_id } => {
                self.issued_scopes.clear();
                if !failed {
                    self.issued_scopes.push_back((*frame_id, epoch));
                }
            }
            DapControl::Variables {
                reference,
                start,
                count,
            } => {
                self.pending_variable_pages.clear();
                if !failed {
                    self.pending_variable_pages
                        .push_back((*reference, *start, *count));
                }
            }
            _ => {}
        }
    }
    /// The stop a stopped-data request belongs to, recorded at issue time.
    pub(in crate::app) fn note_issued(&mut self, stack: bool, scopes: Option<i64>, epoch: u64) {
        if stack {
            self.issued_stack.push_back(epoch);
        }
        if let Some(frame) = scopes {
            self.issued_scopes.push_back((frame, epoch));
        }
    }
    /// Whether an answer issued at `epoch` may be presented now: only if it belongs to THIS stop.
    fn is_current_stop_answer(issued: Option<u64>, epoch: u64) -> bool {
        issued.is_none_or(|issued| issued == epoch)
    }
    /// A refused EXPANSION is visible too: the reference stays a reference.
    fn note_refused_expansion(&mut self, reference: i64) {
        self.push_console(crate::debugger::DapConsoleEntry::new(
            crate::debugger::DapConsoleOrigin::Notice,
            "dusty",
            format!("reference {reference} is already open on this path; it is not expanded again"),
        ));
    }
    /// A refused answer is visible rather than silent: it is a notice, not a value.
    fn note_refused_answer(&mut self) {
        self.push_console(crate::debugger::DapConsoleEntry::new(
            crate::debugger::DapConsoleOrigin::Notice,
            "dusty",
            "a debugger answer from an earlier stop was refused; it is not current",
        ));
    }
    pub(in crate::app) fn try_event(&mut self, editor: &EditorState) -> Option<DapEvent> {
        self.refresh_breakpoints(editor);
        self.refresh_watches();
        let (context, event) = loop {
            let (context, event) = self.service.try_owned_event()?;
            if !context.on_current_connection()
                && matches!(
                    &event,
                    DapEvent::Response {
                        response: DapResponse::Breakpoints(_)
                            | DapResponse::BreakpointChanged { .. },
                        ..
                    } | DapEvent::RequestFailed {
                        request: Some(DapControl::SetBreakpoints { .. }),
                        ..
                    }
                )
            {
                continue;
            }
            let answer = match &event {
                DapEvent::Response {
                    request_id,
                    request: Some(request),
                    ..
                }
                | DapEvent::RequestFailed {
                    request_id,
                    request: Some(request),
                    ..
                } => Some((*request_id, request)),
                _ => None,
            };
            if let Some((id, request)) = answer {
                let frame_dependent = matches!(
                    request,
                    DapControl::Scopes { .. } | DapControl::Variables { .. }
                ) || matches!(request, DapControl::Evaluate { context, .. } if context == "watch");
                // The transport owns the initial automatic threads request (ID zero);
                // subsequent follow requests are recorded when we send them.
                let stale_threads = id == 0
                    && matches!(request, DapControl::Threads)
                    && self.follow_request != Some(0);
                if stale_threads
                    || self.requests.get(&id).is_some_and(|pending| {
                        (pending.automatic && self.follow_request != Some(id))
                            || (frame_dependent
                                && (pending.epoch != self.stop_epoch
                                    || pending.frame != self.console_frame))
                    })
                {
                    self.requests.remove(&id);
                    if let DapControl::Evaluate {
                        expression,
                        context,
                        ..
                    } = request
                    {
                        let watch = (context == "watch").then(|| expression.clone());
                        if let Some(position) =
                            self.evaluations.iter().position(|old| old == &watch)
                        {
                            self.evaluations.remove(position);
                        }
                    }
                    continue;
                }
            }
            break (context, event);
        };
        self.answer_request = match &event {
            DapEvent::Response { request_id, .. } => Some(*request_id),
            _ => None,
        };
        match &event {
            DapEvent::Response {
                request_id,
                request: Some(request),
                ..
            } => self.prepare_answer(*request_id, request, false),
            DapEvent::RequestFailed {
                request_id,
                request: Some(request),
                ..
            } => self.prepare_answer(*request_id, request, true),
            _ => {}
        }
        match &event {
            DapEvent::Response {
                request: Some(DapControl::SetBreakpoints { path, breakpoints }),
                request_id,
                response,
                ..
            } if self.awaiting_breakpoints(path, *request_id, &context) => {
                self.latest_breakpoints.insert(
                    path.clone(),
                    BreakpointRequest::Acknowledged(*request_id, context.connection()),
                );
                self.apply_breakpoints(path, breakpoints, response, editor);
            }
            DapEvent::RequestFailed {
                request: Some(DapControl::SetBreakpoints { path, breakpoints }),
                request_id,
                ..
            } if self.awaiting_breakpoints(path, *request_id, &context) => {
                self.latest_breakpoints.insert(
                    path.clone(),
                    BreakpointRequest::Rejected(*request_id, context.connection()),
                );
                self.apply_breakpoints(
                    path,
                    breakpoints,
                    &DapResponse::Breakpoints(vec![(false, None, None); breakpoints.len()]),
                    editor,
                );
            }
            DapEvent::Response {
                response: DapResponse::Disconnected { terminate_debuggee },
                ..
            } => {
                self.end_state = Some(if *terminate_debuggee {
                    crate::debugger::DapSessionEnd::Terminated
                } else {
                    crate::debugger::DapSessionEnd::Detached
                });
                let _ = self.stop();
            }
            DapEvent::Response {
                response:
                    DapResponse::BreakpointChanged {
                        path,
                        requested,
                        request_index,
                        verified,
                        line,
                        removed,
                    },
                request_id,
                ..
            } => {
                if context.on_current_connection()
                    && self.latest_breakpoints.get(path)
                        == Some(&BreakpointRequest::Acknowledged(
                            *request_id,
                            context.connection(),
                        ))
                    && let Some(row) = self.breakpoints.get_mut(path).and_then(|rows| {
                        rows.iter_mut()
                            .filter(|row| !row.disabled)
                            .nth(*request_index)
                    })
                    && row.location.line(path, editor) == *requested
                {
                    let verdict = (!removed).then_some(if *verified {
                        DapBreakpointState::Verified {
                            line: line.unwrap_or(*requested),
                        }
                    } else {
                        DapBreakpointState::Requested { accepted: false }
                    });
                    self.breakpoint_picker_dirty |= row.set_verdict(verdict);
                    self.coalesce_verified_breakpoints(path, editor);
                }
            }
            _ => {}
        }
        self.event_source = Some(context);
        // Remember WHICH adapter is talking, so a record created later (by a submission) can still
        // name the actor that produced it.
        if let Some(actor) = match &event {
            DapEvent::Response { adapter, .. }
            | DapEvent::Output { adapter, .. }
            | DapEvent::Stopped { adapter, .. }
            | DapEvent::Running { adapter, .. }
            | DapEvent::RequestFailed { adapter, .. }
            | DapEvent::RunInTerminal { adapter, .. }
            | DapEvent::Failed { adapter, .. } => Some(adapter.clone()),
            _ => None,
        } {
            self.actor = Some(actor);
        }
        if self
            .source
            .as_ref()
            .is_some_and(|s| !self.service.current(&s.context))
        {
            self.source = None;
        }
        match &event {
            DapEvent::RunInTerminal {
                adapter,
                request_seq,
                reply,
                ..
            } => {
                // At most 8 resident actors x 32 pending reverse replies.
                if self.reply_deliveries.len() >= 256 {
                    let _ = reply.respond(false, Some("debugger reply delivery queue full".into()));
                    let _ = self.stop();
                    return Some(DapEvent::Failed {
                        adapter: adapter.clone(),
                        message: "debugger reply delivery queue full; write outcome unknown".into(),
                    });
                }
                self.reply_deliveries
                    .push((adapter.clone(), *request_seq, reply.delivery()));
            }
            DapEvent::RequestFailed { command, .. } if command == "source" => {
                self.source = None;
            }
            DapEvent::RequestFailed { request_id, .. }
                if self.follow_request == Some(*request_id) =>
            {
                self.stop_follow = StopFollow::Idle;
                self.follow_request = None;
            }
            DapEvent::Output { adapter, output } => {
                self.record_program_output(adapter, output);
            }
            DapEvent::Stopped {
                reason, thread_id, ..
            } => {
                // A STOP is a new stop: whatever was captured at the previous one is not current.
                self.begin_stop();
                self.stop_reason = Some(reason.clone());
                self.stop_follow = StopFollow::Threads(*thread_id);
                // The transport issues the initial threads request at each stop with ID zero.
                self.follow_request = Some(0);
            }
            DapEvent::Running { .. } => {
                // Execution is running, so NOTHING captured while stopped is current.
                self.begin_running();
            }
            _ => {}
        }
        Some(event)
    }
    /// Record what the user TYPED and what the adapter ANSWERED, with their own origins, so a
    /// result is never presented as program output and the two can be told apart.
    pub(in crate::app) fn record_console_command(&mut self, text: String) {
        self.next_submission = self.next_submission.wrapping_add(1);
        let submission = self.next_submission;
        // Captured NOW, at submission: this is the stop and frame that produced it, even if the
        // answer arrives after the session has moved on to another stop.
        let stop = self.stop_epoch;
        let frame = self.console_frame;
        let actor = self.actor.clone();
        let mut entry = crate::debugger::DapConsoleEntry::new(
            crate::debugger::DapConsoleOrigin::Command,
            "repl",
            text,
        );
        entry.submission = Some(submission);
        self.push_console(entry);
        self.pending_submissions
            .push_back((submission, stop, frame, actor));
    }
    fn record_program_output(&mut self, adapter: &str, output: &str) {
        for chunk in output.split_inclusive('\n') {
            let line = if let Some(index) = self.output_line {
                index
            } else {
                self.push_console(crate::debugger::DapConsoleEntry::new(
                    crate::debugger::DapConsoleOrigin::ProgramOutput,
                    adapter,
                    String::new(),
                ));
                self.console.len() - 1
            };
            let entry = &mut self.console[line];
            entry
                .text
                .push_str(chunk.strip_suffix('\n').unwrap_or(chunk));
            if chunk.ends_with('\n') {
                if entry.text.ends_with('\r') {
                    entry.text.pop();
                }
                self.output_line = None;
            } else {
                self.output_line = Some(line);
            }
        }
    }
    pub(in crate::app) fn record_inspected_value(&mut self, text: String) {
        self.push_console(crate::debugger::DapConsoleEntry::new(
            crate::debugger::DapConsoleOrigin::Evaluation,
            "inspector",
            text,
        ));
    }
    pub(in crate::app) fn record_console_evaluation(&mut self, text: String) {
        // The answer belongs to the OLDEST submission still waiting for one, so a LATE result
        // updates the record that ASKED for it and never the newer draft typed after it.
        let pending = self.pending_submissions.pop_front();
        let answer = text.clone();
        let mut entry = crate::debugger::DapConsoleEntry::new(
            crate::debugger::DapConsoleOrigin::Evaluation,
            "repl",
            text,
        );
        if let Some((submission, stop, frame, actor)) = pending {
            entry.submission = Some(submission);
            entry.stop = Some(stop);
            entry.frame = frame;
            entry.actor = actor;
            // The submission's OWN record is updated in place, which is what makes it its result.
            if let Some(record) = self
                .console
                .iter_mut()
                .rev()
                .find(|record| record.submission == Some(submission))
            {
                record.result = Some(answer);
            }
        }
        self.push_console(entry);
    }
    fn push_console(&mut self, mut entry: crate::debugger::DapConsoleEntry) {
        // Provenance is stamped HERE, once, for every record: what the record itself supplied is
        // kept, and what is ambient is filled in.
        if entry.stop.is_none() {
            entry.stop = Some(self.stop_epoch);
        }
        if entry.frame.is_none() {
            entry.frame = self.console_frame;
        }
        if entry.actor.is_none() {
            entry.actor = self.actor.clone();
        }
        self.console.push_back(entry);
        while self.console.len() > 10_000 {
            self.console.pop_front();
            self.output_line = self.output_line.and_then(|index| index.checked_sub(1));
        }
    }
    pub(in crate::app) fn console(
        &self,
    ) -> impl Iterator<Item = &crate::debugger::DapConsoleEntry> {
        self.console.iter()
    }
    pub(in crate::app) fn watches(&self) -> &[String] {
        &self.watches
    }
    pub(in crate::app) fn add_watch(&mut self, expression: String) {
        if !expression.is_empty()
            && expression.len() <= 64 * 1024
            && !expression.contains('\0')
            && self.watches.len() < MAXIMUM_WATCHES
            && !self.watches.contains(&expression)
        {
            if let Some(changed) = &mut self.changed_watches {
                changed.insert(expression.clone());
            }
            self.watches.push(expression.clone());
            // Paused, the value can be shown now rather than at the next stop.
            if self.console_frame.is_some() {
                self.watch_refresh.push_back(expression);
                self.refresh_watches();
            }
        }
    }
    pub(in crate::app) fn remove_watch(&mut self, expression: &str) {
        if let Some(changed) = &mut self.changed_watches {
            changed.insert(expression.into());
        }
        self.watches.retain(|watch| watch != expression);
        self.watch_refresh.retain(|watch| watch != expression);
        self.watch_values.remove(expression);
    }
    pub(in crate::app) fn breakpoint_condition_target(
        &self,
        path: PathBuf,
        line: u64,
        editor: &mut EditorState,
    ) -> (String, BreakpointConditionTarget) {
        if let Some(row) = self.displayed_breakpoint(&path, line, editor) {
            (
                row.condition.clone().unwrap_or_default(),
                BreakpointConditionTarget::Existing {
                    path,
                    identity: row.identity,
                },
            )
        } else {
            let location = BreakpointLocation::attach(&path, line, editor);
            (
                String::new(),
                BreakpointConditionTarget::New { path, location },
            )
        }
    }

    pub(in crate::app) fn set_breakpoint_condition(
        &mut self,
        target: BreakpointConditionTarget,
        condition: &str,
        editor: &mut EditorState,
    ) -> Result<(), String> {
        let condition = condition.trim();
        if !valid_breakpoint_text(condition) {
            target.cancel(editor);
            return Err("invalid debugger breakpoint".into());
        }
        let condition = (!condition.is_empty()).then(|| condition.to_owned());
        let (path, identity) = match target {
            BreakpointConditionTarget::Existing { path, identity } => (path, identity),
            BreakpointConditionTarget::New { path, location } => {
                let line = location.line(&path, editor);
                location.remove_mark(editor);
                // An edit while the prompt was open can move this mark onto an existing anchor.
                let Some(row) = self.breakpoint_at_anchor(&path, line, editor) else {
                    self.change_breakpoint(
                        DapSourceBreakpoint {
                            path,
                            line,
                            column: None,
                            condition,
                            hit_condition: None,
                            log_message: None,
                        },
                        None,
                        false,
                        editor,
                    )?;
                    return Ok(());
                };
                (path, row.identity)
            }
        };
        let Some(rows) = self.breakpoints.get_mut(&path) else {
            return Ok(());
        };
        let Some(row) = rows.iter_mut().find(|row| row.identity == identity) else {
            return Ok(());
        };
        if let Some(changed) = &mut self.changed_breakpoints {
            changed.insert((path.clone(), row.location.line(&path, editor)));
        }
        row.condition = condition;
        for row in rows {
            row.mark_pending();
        }
        self.queue_breakpoints(&path);
        self.refresh_breakpoints(editor);
        Ok(())
    }

    #[cfg(test)]
    pub(in crate::app) fn breakpoint(
        &mut self,
        row: DapSourceBreakpoint,
        toggle: bool,
        editor: &mut EditorState,
    ) -> Result<bool, String> {
        let existing = self
            .breakpoint_at_anchor(&row.path, row.line, editor)
            .map(|row| row.identity);
        self.change_breakpoint(row, existing, toggle, editor)
    }

    fn breakpoint_on_display_line(
        &mut self,
        row: DapSourceBreakpoint,
        toggle: bool,
        editor: &mut EditorState,
    ) -> Result<bool, String> {
        let existing = self
            .displayed_breakpoint(&row.path, row.line, editor)
            .map(|row| row.identity);
        self.change_breakpoint(row, existing, toggle, editor)
    }

    fn change_breakpoint(
        &mut self,
        row: DapSourceBreakpoint,
        existing: Option<BreakpointId>,
        toggle: bool,
        editor: &mut EditorState,
    ) -> Result<bool, String> {
        let path = row.path.clone();
        let removed = self.update_breakpoint(row, existing, toggle, None, editor)?;
        self.queue_breakpoints(&path);
        self.refresh_breakpoints(editor);
        Ok(removed)
    }

    fn update_breakpoint(
        &mut self,
        row: DapSourceBreakpoint,
        identity: Option<BreakpointId>,
        toggle: bool,
        saved: Option<(bool, Option<PersistedBreakpointId>)>,
        editor: &mut EditorState,
    ) -> Result<bool, String> {
        if row.line == 0
            || !row.path.is_absolute()
            || [
                row.condition.as_deref(),
                row.hit_condition.as_deref(),
                row.log_message.as_deref(),
            ]
            .into_iter()
            .flatten()
            .any(|text| !valid_breakpoint_text(text))
        {
            return Err("invalid debugger breakpoint".into());
        }
        let new_identity = if identity.is_none() {
            if self.breakpoints.values().map(Vec::len).sum::<usize>() >= MAXIMUM_BREAKPOINTS {
                return Err("debugger breakpoint limit reached".into());
            }
            self.last_breakpoint_id = self
                .last_breakpoint_id
                .checked_add(1)
                .ok_or("debugger breakpoint limit reached")?;
            Some(BreakpointId(self.last_breakpoint_id))
        } else {
            None
        };
        let rows = self.breakpoints.entry(row.path.clone()).or_default();
        let (removed, anchor) = if let Some(identity) = new_identity {
            let (disabled, persisted_id) = saved.unwrap_or_default();
            rows.push(SourceBreakpoint {
                identity,
                persisted_id: persisted_id.unwrap_or_else(PersistedBreakpointId::fresh),
                location: BreakpointLocation::attach(&row.path, row.line, editor),
                column: row.column,
                condition: row.condition,
                hit_condition: row.hit_condition,
                log_message: row.log_message,
                disabled,
                verdict: None,
                pending_display_line: None,
            });
            (false, row.line)
        } else if let Some(index) = rows.iter().position(|old| Some(old.identity) == identity) {
            let old = &mut rows[index];
            let anchor = old.location.line(&row.path, editor);
            if toggle {
                rows.remove(index).location.remove_mark(editor);
            } else {
                old.column = row.column;
                old.condition = row.condition;
                old.hit_condition = row.hit_condition;
                old.log_message = row.log_message;
            }
            (toggle, anchor)
        } else {
            return Ok(false);
        };
        if let Some(changed) = &mut self.changed_breakpoints {
            changed.insert((row.path.clone(), anchor));
        }
        if rows.is_empty() {
            self.breakpoints.remove(&row.path);
        } else {
            for row in rows {
                row.mark_pending();
            }
        }
        Ok(removed)
    }

    fn awaiting_breakpoints(&self, path: &Path, request_id: u64, context: &DapContext) -> bool {
        if !context.on_current_connection() || self.breakpoint_refresh.contains(path) {
            return false;
        }
        // A child connection replays the same request IDs, including startup ID zero.
        match self.latest_breakpoints.get(path).copied() {
            Some(BreakpointRequest::Pending(id, connection)) => {
                id == request_id && connection <= context.connection()
            }
            Some(
                BreakpointRequest::Acknowledged(id, connection)
                | BreakpointRequest::Rejected(id, connection),
            ) => id == request_id && connection < context.connection(),
            None => request_id == 0,
        }
    }

    fn apply_breakpoints(
        &mut self,
        path: &Path,
        rows: &[DapSourceBreakpoint],
        answer: &DapResponse,
        editor: &EditorState,
    ) {
        let Some(current) = self.breakpoints.get_mut(path) else {
            return;
        };
        let states = crate::debugger::breakpoint_states(rows, &[], answer);
        // A new placement can share a requested line with a relocated breakpoint.
        // Match each answer only once, in the order those rows were sent.
        let mut matched = BTreeSet::new();
        for (row, state) in rows.iter().zip(states) {
            if row.path != path {
                continue;
            }
            if let Some(breakpoint) = current.iter_mut().find(|r| {
                !r.disabled
                    && !matched.contains(&r.identity)
                    && r.location.line(path, editor) == row.line
                    && r.column == row.column
                    && r.condition == row.condition
                    && r.hit_condition == row.hit_condition
                    && r.log_message == row.log_message
            }) {
                matched.insert(breakpoint.identity);
                self.breakpoint_picker_dirty |= breakpoint.set_verdict(Some(state));
            }
        }
        self.coalesce_verified_breakpoints(path, editor);
    }

    /// The adapter may place two otherwise identical requests on the same executable line.
    fn coalesce_verified_breakpoints(&mut self, path: &Path, editor: &EditorState) {
        let Some(rows) = self.breakpoints.get_mut(path) else {
            return;
        };
        let duplicates: BTreeSet<_> = {
            let mut seen = HashSet::new();
            rows.iter()
                .filter_map(|row| {
                    let Some(DapBreakpointState::Verified { line }) = row.state() else {
                        return None;
                    };
                    let key = (
                        line,
                        row.column,
                        row.condition.as_deref(),
                        row.hit_condition.as_deref(),
                        row.log_message.as_deref(),
                    );
                    (!seen.insert(key)).then_some(row.identity)
                })
                .collect()
        };
        if duplicates.is_empty() {
            return;
        }
        rows.retain(|row| {
            if duplicates.contains(&row.identity) {
                if let Some(changed) = &mut self.changed_breakpoints {
                    changed.insert((path.into(), row.location.line(path, editor)));
                }
                self.discarded_breakpoint_marks.push(row.location);
                false
            } else {
                true
            }
        });
        // Replace the adapter list too, so subsequent events index the surviving rows.
        self.queue_breakpoints(path);
    }

    fn remove_discarded_breakpoint_marks(&mut self, editor: &mut EditorState) {
        for location in self.discarded_breakpoint_marks.drain(..) {
            location.remove_mark(editor);
        }
    }

    fn breakpoint_at_anchor(
        &self,
        path: &Path,
        line: u64,
        editor: &EditorState,
    ) -> Option<&SourceBreakpoint> {
        self.breakpoints
            .get(path)?
            .iter()
            .find(|row| row.location.line(path, editor) == line)
    }

    #[cfg(test)]
    fn breakpoint_disabled(&self, path: &Path, line: u64, editor: &EditorState) -> Option<bool> {
        self.breakpoint_at_anchor(path, line, editor)
            .map(|row| row.disabled)
    }

    #[cfg(test)]
    fn set_breakpoint_disabled(
        &mut self,
        path: &Path,
        line: u64,
        disabled: bool,
        editor: &EditorState,
    ) {
        if let Some(row) = self.breakpoint_at_anchor(path, line, editor) {
            let _ = self.set_breakpoint_disabled_by_id(path, row.identity, disabled, editor);
        }
    }

    /// Change only the enabled state and return the line where the sign is now shown.
    fn set_breakpoint_disabled_by_id(
        &mut self,
        path: &Path,
        identity: BreakpointId,
        disabled: bool,
        editor: &EditorState,
    ) -> Option<u64> {
        let rows = self.breakpoints.get_mut(path)?;
        let row = rows.iter_mut().find(|row| row.identity == identity)?;
        if let Some(changed) = &mut self.changed_breakpoints {
            changed.insert((path.to_path_buf(), row.location.line(path, editor)));
        }
        row.disabled = disabled;
        row.set_verdict(Some(DapBreakpointState::Requested { accepted: true }));
        let line = row.display_line(path, editor);
        for row in rows {
            row.mark_pending();
        }
        self.queue_breakpoints(path);
        self.refresh_breakpoints(editor);
        Some(line)
    }

    /// Keep a last known position for closed files and merge marks that land on the same line.
    pub(in crate::app) fn sync_breakpoint_locations(&mut self, editor: &mut EditorState) {
        self.remove_discarded_breakpoint_marks(editor);
        let mut refresh = Vec::new();
        for (path, rows) in &mut self.breakpoints {
            let mut old_lines = BTreeMap::new();
            let mut needs_refresh = false;
            for row in rows.iter_mut() {
                let before = row.location.last_line();
                row.location.sync(path, editor);
                let after = row.location.last_line();
                if before != after {
                    if let Some(changed) = &mut self.changed_breakpoints {
                        changed.insert((path.clone(), before));
                        changed.insert((path.clone(), after));
                    }
                    row.set_verdict(Some(DapBreakpointState::Requested { accepted: true }));
                    old_lines.insert(row.identity, before);
                    needs_refresh |= !row.disabled;
                }
            }
            if !old_lines.is_empty() {
                self.breakpoint_picker_dirty = true;
                if rows.len() > 1 {
                    needs_refresh |= Self::merge_breakpoints(rows, &old_lines, editor);
                }
            }
            if needs_refresh {
                refresh.push(path.clone());
            }
        }
        for path in refresh {
            self.queue_breakpoints(&path);
        }
    }

    /// Keep the enabled row (the earlier one on ties) without taking settings from the loser.
    fn merge_breakpoints(
        rows: &mut Vec<SourceBreakpoint>,
        old_lines: &BTreeMap<BreakpointId, u64>,
        editor: &mut EditorState,
    ) -> bool {
        let mut by_line = HashMap::<u64, (BreakpointId, bool, u64, bool)>::new();
        for row in rows.iter() {
            let line = row.location.last_line();
            let prior = old_lines.get(&row.identity).copied().unwrap_or(line);
            let (winner, disabled, first_prior, converged) =
                by_line
                    .entry(line)
                    .or_insert((row.identity, row.disabled, prior, false));
            *converged |= *first_prior != prior;
            if *disabled && !row.disabled {
                *winner = row.identity;
                *disabled = false;
            }
        }
        // Co-located marks created by a cursor placement aren't an edit collision.
        let mut removed_enabled = false;
        rows.retain(|row| {
            let (winner, _, _, converged) = by_line[&row.location.last_line()];
            if converged && winner != row.identity {
                // Undo remaps surviving marks; it does not restore the discarded breakpoint.
                row.location.remove_mark(editor);
                removed_enabled |= !row.disabled;
                false
            } else {
                true
            }
        });
        removed_enabled
    }

    fn queue_breakpoints(&mut self, path: &Path) {
        self.breakpoint_picker_dirty = true;
        if self.service.is_running() {
            self.latest_breakpoints.remove(path);
            self.breakpoint_refresh.insert(path.into());
        }
    }

    fn enabled_breakpoints(&self, path: &Path, editor: &EditorState) -> Vec<DapSourceBreakpoint> {
        self.breakpoints
            .get(path)
            .into_iter()
            .flatten()
            .filter(|row| !row.disabled)
            .map(|row| row.request_row(path, editor))
            .collect()
    }

    fn refresh_breakpoints(&mut self, editor: &EditorState) {
        for _ in 0..16 {
            let Some(path) = self.breakpoint_refresh.first().cloned() else {
                break;
            };
            let breakpoints = self.enabled_breakpoints(&path, editor);
            match self.send_control(DapControl::SetBreakpoints {
                path: path.clone(),
                breakpoints,
            }) {
                Ok(_) => {
                    self.breakpoint_refresh.remove(&path);
                }
                Err(DapError::Full) => break,
                Err(error) => {
                    self.breakpoint_refresh.remove(&path);
                    self.push_console(crate::debugger::DapConsoleEntry::new(
                        crate::debugger::DapConsoleOrigin::Notice,
                        "dusty",
                        error.to_string(),
                    ));
                }
            }
        }
    }
    /// TERMINATE: ask for the debuggee to end with the session, and record that we asked.
    pub(in crate::app) fn terminate_debuggee(&mut self) -> Result<(), String> {
        self.end_session(DapControl::Disconnect {
            terminate_debuggee: true,
        })
    }
    /// DETACH: leave the session WITHOUT asking the debuggee to stop. The editor is no longer
    /// attached; whether the program keeps running is the adapter's side of the contract, and this
    /// editor does not claim it killed anything it did not ask to kill.
    pub(in crate::app) fn detach_debuggee(&mut self) -> Result<(), String> {
        self.end_session(DapControl::Disconnect {
            terminate_debuggee: false,
        })
    }
    fn end_session(&mut self, control: DapControl) -> Result<(), String> {
        self.service.control(control).map_err(|e| e.to_string())
    }
    /// The threads the adapter last reported.
    pub(in crate::app) fn threads(&self) -> &[String] {
        &self.threads
    }
    /// The stack of the CURRENT stop, or nothing when execution has run on.
    pub(in crate::app) fn stack(&self) -> &[String] {
        &self.stack
    }
    /// What this session is attached to or running, when it started: the target's identity.
    pub(in crate::app) fn target(&self) -> Option<&DapTargetIdentity> {
        self.target.as_ref()
    }
    /// How this session ended, if it has: `Detached` and `Terminated` are different facts.
    pub(in crate::app) fn end_state(&self) -> Option<crate::debugger::DapSessionEnd> {
        self.end_state
    }
    /// Resolve only a displayed sign, using the same row order as the gutter.
    fn displayed_breakpoint(
        &self,
        path: &Path,
        line: u64,
        editor: &EditorState,
    ) -> Option<&SourceBreakpoint> {
        self.breakpoints
            .get(path)?
            .iter()
            .find(|row| row.display_line(path, editor) == line)
    }

    fn breakpoint_rows(&self) -> impl Iterator<Item = (&Path, &SourceBreakpoint)> {
        self.breakpoints
            .iter()
            .flat_map(|(path, rows)| rows.iter().map(move |row| (path.as_path(), row)))
    }

    #[cfg(test)]
    fn breakpoint_state(
        &self,
        path: &Path,
        line: u64,
        editor: &EditorState,
    ) -> Option<DapBreakpointState> {
        self.breakpoints
            .get(path)?
            .iter()
            .find(|row| row.location.line(path, editor) == line)?
            .state()
    }

    /// Include sources with only disabled rows so the adapter can clear them.
    pub(in crate::app) fn startup_breakpoints(
        &mut self,
        editor: &EditorState,
    ) -> Vec<(PathBuf, Vec<DapSourceBreakpoint>)> {
        self.breakpoint_picker_dirty = true;
        for row in self.breakpoints.values_mut().flatten() {
            row.set_verdict(Some(DapBreakpointState::Requested { accepted: true }));
        }
        self.breakpoints
            .keys()
            .map(|path| (path.clone(), self.enabled_breakpoints(path, editor)))
            .collect()
    }
    pub(in crate::app) fn stop_reason(&self) -> Option<&str> {
        self.stop_reason.as_deref()
    }
    pub(in crate::app) fn execution(&self) -> Option<&(PathBuf, u64)> {
        self.execution.as_ref()
    }
    pub(in crate::app) fn variables(&self) -> &[String] {
        &self.variables
    }
    pub(in crate::app) fn watch_value(&self, expression: &str) -> Option<&str> {
        self.watch_values.get(expression).map(String::as_str)
    }
    /// The frame an evaluation should run in, which is what makes locals visible to it.
    pub(in crate::app) fn stopped_thread(&self) -> Option<i64> {
        self.stopped_thread
    }
    pub(in crate::app) fn current_frame(&self) -> Option<i64> {
        self.console_frame
    }
    /// A watch that cannot be evaluated here is a normal thing to see in the pane, not an error.
    pub(in crate::app) fn watch_request_failed(&mut self, command: &str) -> bool {
        if command != "evaluate" {
            return false;
        }
        let Some(expression) = self.evaluations.pop_front().flatten() else {
            return false;
        };
        if self.watches.contains(&expression) {
            self.watch_values.insert(expression, "not available".into());
        }
        true
    }
    pub(in crate::app) fn persisted_breakpoints(
        &self,
        editor: &EditorState,
    ) -> Vec<PersistedBreakpoint> {
        self.breakpoints
            .iter()
            .flat_map(|(path, rows)| {
                rows.iter().map(|row| PersistedBreakpoint {
                    id: Some(row.persisted_id),
                    path: path.clone(),
                    line: row.location.line(path, editor),
                    column: row.column,
                    condition: row.condition.clone(),
                    hit_condition: row.hit_condition.clone(),
                    log_message: row.log_message.clone(),
                    disabled: row.disabled,
                })
            })
            .collect()
    }
    pub(in crate::app) fn persisted_watches(&self) -> Vec<PersistedWatch> {
        self.watches
            .iter()
            .map(|expression| PersistedWatch {
                expression: expression.clone(),
            })
            .collect()
    }
    pub(in crate::app) fn begin_restore(&mut self) {
        self.changed_breakpoints = Some(Default::default());
        self.changed_watches = Some(Default::default());
    }
    pub(in crate::app) fn finish_restore(&mut self) {
        self.changed_breakpoints = None;
        self.changed_watches = None;
    }
    pub(in crate::app) fn restore(
        &mut self,
        rows: Vec<PersistedBreakpoint>,
        watches: Vec<PersistedWatch>,
        workspace: &Workspace,
        editor: &mut EditorState,
    ) {
        self.sync_breakpoint_locations(editor);
        let mut refresh = BTreeSet::new();
        let changed_breakpoints = self.changed_breakpoints.take().unwrap_or_default();
        let changed_watches = self.changed_watches.take().unwrap_or_default();
        self.breakpoints.retain(|path, rows| {
            rows.retain(|row| {
                if changed_breakpoints.contains(&(path.clone(), row.location.line(path, editor))) {
                    return true;
                }
                row.location.remove_mark(editor);
                refresh.insert(path.clone());
                false
            });
            !rows.is_empty()
        });
        for r in rows.into_iter().take(MAXIMUM_BREAKPOINTS) {
            if changed_breakpoints.contains(&(r.path.clone(), r.line))
                || workspace.require_path(&r.path).is_err()
            {
                continue;
            }
            let path = r.path.clone();
            if self
                .update_breakpoint(
                    DapSourceBreakpoint {
                        path: r.path,
                        line: r.line,
                        column: r.column,
                        condition: r.condition,
                        hit_condition: r.hit_condition,
                        log_message: r.log_message,
                    },
                    None,
                    false,
                    Some((r.disabled, r.id)),
                    editor,
                )
                .is_ok()
            {
                refresh.insert(path);
            }
        }
        for path in refresh {
            self.queue_breakpoints(&path);
        }
        self.refresh_breakpoints(editor);
        self.watches.retain(|watch| changed_watches.contains(watch));
        self.watch_values
            .retain(|watch, _| changed_watches.contains(watch));
        self.watch_refresh
            .retain(|watch| changed_watches.contains(watch));
        for watch in watches.into_iter().take(MAXIMUM_WATCHES) {
            if !changed_watches.contains(&watch.expression) {
                self.add_watch(watch.expression);
            }
        }
    }
    fn install(&mut self, values: Vec<(String, String, Choice)>) -> Vec<DebugRow> {
        let source = self.event_source.take().or_else(|| self.service.context());
        let Some(source) = source.filter(|s| self.service.current(s)) else {
            self.choices = None;
            return Vec::new();
        };
        let epoch = Arc::new(());
        let mut choices = Vec::new();
        let rows = values
            .into_iter()
            .take(10_000)
            .enumerate()
            .map(|(index, (label, detail, choice))| {
                choices.push(choice);
                DebugRow {
                    label,
                    detail,
                    key: DebugKey {
                        epoch: epoch.clone(),
                        index,
                    },
                }
            })
            .collect();
        self.choices = Some(Choices {
            epoch,
            source,
            values: choices,
        });
        rows
    }
    pub(in crate::app) fn controls(
        &mut self,
        rows: Vec<(String, String, DapControl)>,
    ) -> Vec<DebugRow> {
        self.install(
            rows.into_iter()
                .map(|(a, b, c)| (a, b, Choice::Control(c)))
                .collect(),
        )
    }
    pub(in crate::app) fn response(&mut self, response: DapResponse) -> DebugResponse {
        if !self
            .event_source
            .as_ref()
            .is_some_and(|s| self.service.current(s))
        {
            return DebugResponse::Stale;
        }
        if matches!(response, DapResponse::Evaluate(_))
            && let Some(expression) = self.evaluations.pop_front().flatten()
        {
            if let DapResponse::Evaluate(value) = response
                && self.watches.contains(&expression)
            {
                self.watch_values.insert(expression, value.value);
            }
            return DebugResponse::Absorbed;
        }
        let response = match self.follow_stop(response) {
            Ok(response) => response,
            Err(followed) => return followed,
        };
        let (kind, rows) = match response {
            DapResponse::Disconnected { .. } | DapResponse::BreakpointChanged { .. } => {
                return DebugResponse::Absorbed;
            }
            DapResponse::Threads(rows) => ("threads", {
                // RETAINED: the left inspector shows the process's threads, not only a picker.
                self.threads = rows
                    .iter()
                    .map(|r| format!("{} ({})", r.name, r.id))
                    .collect();
                self.controls(
                    rows.into_iter()
                        .map(|r| {
                            (
                                r.name,
                                r.id.to_string(),
                                DapControl::StackTrace {
                                    thread_id: r.id,
                                    start: 0,
                                    levels: 256,
                                },
                            )
                        })
                        .collect(),
                )
            }),
            DapResponse::Stack(rows) => {
                let issued = self.issued_stack.pop_front();
                if Self::is_current_stop_answer(issued, self.stop_epoch) {
                    // RETAINED only here: an answer belonging to an earlier stop retains nothing.
                    self.stack = rows
                        .iter()
                        .map(|f| format!("{}:{}", f.name, f.line))
                        .collect();
                    ("frames", self.frames(rows))
                } else {
                    self.note_refused_answer();
                    ("frames", Vec::new())
                }
            }
            DapResponse::Scopes(_rows)
                if !Self::is_current_stop_answer(
                    self.issued_scopes.pop_front().map(|(_, epoch)| epoch),
                    self.stop_epoch,
                ) =>
            {
                self.note_refused_answer();
                ("scopes", Vec::new())
            }
            DapResponse::Scopes(rows) => {
                // Scopes are the ROOT of expansion: a fresh list starts a fresh path.
                self.expansion.clear();
                self.pending_variable_pages.clear();
                (
                    "scopes",
                    self.controls(
                        rows.into_iter()
                            .map(|r| {
                                (
                                    r.name,
                                    if r.expensive {
                                        "expensive".into()
                                    } else {
                                        String::new()
                                    },
                                    DapControl::Variables {
                                        reference: r.variables_reference,
                                        start: 0,
                                        count: VARIABLE_PAGE,
                                    },
                                )
                            })
                            .collect(),
                    ),
                )
            }
            DapResponse::Variables(rows) => ("variables", {
                // The transport discards responses from earlier stops.
                let page = self.pending_variable_pages.pop_front();
                let current = rows;
                let open: Vec<i64> = self.expansion.clone();
                let shown = current.len();
                let mut values: Vec<(String, String, Choice)> = current
                    .into_iter()
                    .map(|r| {
                        // A reference already OPEN on this path is shown AS a reference: the
                        // detail says so, and expanding it is refused.
                        let detail = if r.variables_reference != 0
                            && open.contains(&r.variables_reference)
                        {
                            format!(
                                "{} (already open, not expanded)",
                                r.type_name.unwrap_or_default()
                            )
                        } else {
                            r.type_name.unwrap_or_default()
                        };
                        let label = format!("{} = {}", r.name, r.value);
                        let choice = if r.variables_reference == 0 {
                            Choice::Value(label.clone())
                        } else {
                            Choice::Control(DapControl::Variables {
                                reference: r.variables_reference,
                                start: 0,
                                count: VARIABLE_PAGE,
                            })
                        };
                        (label, detail, choice)
                    })
                    .collect();
                // A FULL page means more may follow, so the continuation carries the EXACT
                // count shown and asks for the next page by OFFSET rather than guessing.
                if let Some((reference, start, count)) = page
                    && count > 0
                    && shown as u64 >= count
                {
                    values.push((
                        format!("load more ({shown} shown)"),
                        format!("continue from {}", start + shown as u64),
                        Choice::Control(DapControl::Variables {
                            reference,
                            start: start + shown as u64,
                            count: VARIABLE_PAGE,
                        }),
                    ));
                }
                self.install(values)
            }),
            value => return DebugResponse::Result(value),
        };
        DebugResponse::Picker { kind, rows }
    }
    /// Take the next step of following a stop, or hand back a response that is not part of it.
    fn follow_stop(&mut self, response: DapResponse) -> Result<DapResponse, DebugResponse> {
        if self.follow_request != self.answer_request {
            return Ok(response);
        }
        match (self.stop_follow, response) {
            (StopFollow::Threads(stopped), DapResponse::Threads(rows)) => {
                self.threads = rows
                    .iter()
                    .map(|r| format!("{} ({})", r.name, r.id))
                    .collect();
                self.stop_follow = StopFollow::Idle;
                self.stopped_thread = stopped.or(rows.first().map(|r| r.id));
                if let Some(thread_id) = self.stopped_thread {
                    self.stop_follow = StopFollow::Stack;
                    self.follow_control(DapControl::StackTrace {
                        thread_id,
                        start: 0,
                        levels: 256,
                    });
                }
                Err(DebugResponse::Absorbed)
            }
            (StopFollow::Stack, DapResponse::Stack(rows)) => {
                self.stop_follow = StopFollow::Idle;
                let issued = self.issued_stack.pop_front();
                if !Self::is_current_stop_answer(issued, self.stop_epoch) {
                    self.note_refused_answer();
                    return Err(DebugResponse::Absorbed);
                }
                self.stack = rows
                    .iter()
                    .map(|f| format!("{}:{}", f.name, f.line))
                    .collect();
                let Some(top) = rows.into_iter().next() else {
                    return Err(DebugResponse::Absorbed);
                };
                self.console_frame = Some(top.id);
                self.stop_follow = StopFollow::Scopes;
                self.follow_control(DapControl::Scopes { frame_id: top.id });
                self.watch_refresh = self.watches.iter().cloned().collect();
                self.refresh_watches();
                if top.source_reference.is_some_and(|reference| reference > 0) {
                    return Err(DebugResponse::StoppedSource(top));
                }
                Err(match top.path {
                    Some(path) => {
                        self.execution = Some((path.clone(), top.line));
                        DebugResponse::StoppedAt(DebugLocation {
                            path,
                            line: top.line,
                            column: top.column,
                        })
                    }
                    None => DebugResponse::Absorbed,
                })
            }
            (StopFollow::Scopes, DapResponse::Scopes(rows)) => {
                self.stop_follow = StopFollow::Idle;
                let issued = self.issued_scopes.pop_front().map(|(_, epoch)| epoch);
                if !Self::is_current_stop_answer(issued, self.stop_epoch) {
                    self.note_refused_answer();
                    return Err(DebugResponse::Absorbed);
                }
                self.expansion.clear();
                self.pending_variable_pages.clear();
                // The first scope is the locals, which is what VS Code opens too.
                if let Some(scope) = rows.into_iter().find(|scope| !scope.expensive) {
                    self.stop_follow = StopFollow::Variables;
                    self.follow_control(DapControl::Variables {
                        reference: scope.variables_reference,
                        start: 0,
                        count: VARIABLE_PAGE,
                    });
                }
                Err(DebugResponse::Absorbed)
            }
            (StopFollow::Variables, DapResponse::Variables(rows)) => {
                self.stop_follow = StopFollow::Idle;
                self.expansion.clear();
                self.pending_variable_pages.clear();
                self.variables = rows
                    .into_iter()
                    .filter(|row| !row.value.is_empty())
                    .map(|row| format!("{} = {}", row.name, row.value))
                    .collect();
                Err(DebugResponse::Absorbed)
            }
            (_, response) => Ok(response),
        }
    }
    pub(in crate::app) fn refresh_watches(&mut self) {
        if self
            .event_source
            .as_ref()
            .is_some_and(|source| !self.service.current(source))
        {
            self.watch_refresh.clear();
            return;
        }
        for _ in 0..16 {
            let Some(expression) = self.watch_refresh.front().cloned() else {
                break;
            };
            match self.control(DapControl::Evaluate {
                expression,
                frame_id: self.console_frame,
                context: "watch".into(),
            }) {
                Ok(()) => {
                    self.watch_refresh.pop_front();
                }
                Err(DapError::Full) => break,
                Err(_) => {
                    self.watch_refresh.clear();
                    break;
                }
            }
        }
    }
    pub(in crate::app) fn follow_control(&mut self, control: DapControl) {
        match self.control_request(control) {
            Ok(Some(id)) => {
                self.requests
                    .get_mut(&id)
                    .expect("issued follow request")
                    .automatic = true;
                self.follow_request = Some(id);
            }
            Ok(None) => {
                self.follow_request = None;
                self.stop_follow = StopFollow::Idle;
            }
            Err(error) => {
                self.follow_request = None;
                self.stop_follow = StopFollow::Idle;
                self.push_console(crate::debugger::DapConsoleEntry::new(
                    crate::debugger::DapConsoleOrigin::Notice,
                    "dusty",
                    error.to_string(),
                ));
            }
        }
    }
    fn clear_paused_state(&mut self) {
        self.stop_follow = StopFollow::Idle;
        self.follow_request = None;
        self.answer_request = None;
        self.execution = None;
        self.variables.clear();
        self.watch_values.clear();
        // The adapter actor drops what was in flight when a stop ends, so its answers never come.
        self.evaluations.clear();
        self.pending_submissions.clear();
        self.issued_stack.clear();
        self.issued_scopes.clear();
        self.watch_refresh.clear();
        self.requests.clear();
    }
    pub(in crate::app) fn frames(&mut self, frames: Vec<DapStackFrame>) -> Vec<DebugRow> {
        self.install(
            frames
                .into_iter()
                .map(|f| {
                    let detail = f
                        .path
                        .as_ref()
                        .map(|p| format!("{}:{}:{}", p.display(), f.line, f.column))
                        .unwrap_or_default();
                    (f.name.clone(), detail, Choice::Frame(f))
                })
                .collect(),
        )
    }
    pub(in crate::app) fn dismiss_choices(&mut self) {
        self.choices = None;
        // Hiding the inspector closes the expansion path with it, so a node can be opened again
        // after the inspector is reopened rather than being refused forever.
        self.expansion.clear();
        self.pending_variable_pages.clear();
    }
    /// Execution ran on: invalidate the stopped data and every value captured at that stop, so a
    /// previous value can never be presented as current.
    pub(in crate::app) fn begin_running(&mut self) {
        self.clear_paused_state();
        self.stop_reason = None;
        self.stop_epoch = self.stop_epoch.wrapping_add(1);
        self.choices = None;
        self.source = None;
        // Nothing captured at the stop that has ended is open any more.
        self.threads.clear();
        self.stack.clear();
        self.expansion.clear();
        self.pending_variable_pages.clear();
        // The bound frame belonged to the stop that has ended.
        self.console_frame = None;
    }
    /// A new stop began: the PREVIOUS stop's data is not current either, so it goes the same way
    /// before anything is captured for this stop.
    pub(in crate::app) fn begin_stop(&mut self) {
        self.clear_paused_state();
        self.stop_epoch = self.stop_epoch.wrapping_add(1);
        self.choices = None;
        self.source = None;
        // A new stop has its own references; the previous stop's path is not this one's.
        self.threads.clear();
        self.stack.clear();
        self.expansion.clear();
        self.pending_variable_pages.clear();
        // A new stop has its own frames; the previous frame is not this stop's frame.
        self.console_frame = None;
    }
    #[cfg(test)]
    #[must_use]
    pub(in crate::app) fn stop_epoch(&self) -> u64 {
        self.stop_epoch
    }
    pub(in crate::app) fn take_selection(
        &mut self,
        key: &DebugKey,
    ) -> Result<DebugSelection, DapError> {
        let choices = self
            .choices
            .as_ref()
            .filter(|c| Arc::ptr_eq(&c.epoch, &key.epoch) && self.service.current(&c.source))
            .ok_or(DapError::StaleContext)?;
        if key.index >= choices.values.len() {
            return Err(DapError::StaleContext);
        }
        let mut choices = self.choices.take().expect("checked choices");
        let choice = choices.values.swap_remove(key.index);
        // Selecting a frame BINDS the console to it: a submission then knows which frame produced
        // it, and the console is the one for that frame rather than for no frame at all.
        if let Choice::Frame(frame) = &choice {
            self.console_frame = Some(frame.id);
            self.variables.clear();
            self.watch_values.clear();
            self.watch_refresh = self.watches.iter().cloned().collect();
            self.stop_follow = StopFollow::Idle;
            self.follow_request = None;
        }
        Ok(DebugSelection {
            expansion: self.expansion.clone(),
            source: choices.source,
            choice,
        })
    }
    pub(in crate::app) fn activate(
        &mut self,
        selection: DebugSelection,
        mut target: SourceTarget,
    ) -> Result<Option<DebugLocation>, DapError> {
        if !self.service.current(&selection.source) {
            return Err(DapError::StaleContext);
        }
        match selection.choice {
            Choice::Value(_) => Ok(None),
            Choice::Control(control) => {
                self.expansion = selection.expansion;
                self.control(control)?;
                Ok(None)
            }
            Choice::Frame(frame) => {
                let location = if let Some(reference) = frame.source_reference.filter(|r| *r > 0) {
                    target.line = frame.line;
                    target.column = frame.column;
                    self.request_source(reference, target).map(|()| None)
                } else {
                    Ok(frame.path.map(|path| DebugLocation {
                        path,
                        line: frame.line,
                        column: frame.column,
                    }))
                };
                self.stop_follow = StopFollow::Scopes;
                self.follow_control(DapControl::Scopes { frame_id: frame.id });
                self.refresh_watches();
                location
            }
        }
    }
    pub(in crate::app) fn request_source(
        &mut self,
        reference: i64,
        target: SourceTarget,
    ) -> Result<(), DapError> {
        if self.source.is_some() {
            return Err(DapError::Full);
        }
        let context = self.service.context().ok_or(DapError::NotRunning)?;
        self.service
            .control_owned(&context, DapControl::Source { reference })?;
        self.source = Some(Source { context, target });
        Ok(())
    }
    #[cfg(test)]
    pub(in crate::app) fn source_pending(&self) -> bool {
        self.source.is_some()
    }
    pub(in crate::app) fn take_source(&mut self) -> Option<SourceTarget> {
        let source = self.source.take()?;
        self.service
            .current(&source.context)
            .then_some(source.target)
    }
    pub(in crate::app) fn register_terminal(&mut self, id: PtyId, reply: DapTerminalReply) {
        self.terminal_replies.insert(id, reply);
    }
    pub(in crate::app) fn take_terminal(&mut self, id: &PtyId) -> Option<DapTerminalReply> {
        self.terminal_replies.remove(id)
    }
    pub(in crate::app) fn terminal_current(&self, id: &PtyId) -> bool {
        self.terminal_replies
            .get(id)
            .is_none_or(DapTerminalReply::current)
    }
    pub(in crate::app) fn reject_terminals(&mut self, reason: &str) {
        // Keep the association until terminal preflight completes: a rejected reply must still
        // fail the debugger ownership check rather than look like an ordinary terminal.
        for reply in self.terminal_replies.values() {
            let _ = reply.respond(false, Some(reason.into()));
        }
    }
}

pub(in crate::app) mod commands;

use crate::app::App;
use crate::app::TrustedAction;
use crate::app::picker::{PickerOrigin, PickerPayload};
use crate::editor::text::PositionCodec;
use crate::editor::text::PositionEncoding;
use crate::editor::text::TextBuffer;
use crate::language::lsp::language_id_for_path;
use crate::ui::PickerSurfaceKind;
use crate::workspace::PickerAction;
use crate::workspace::PickerItem;
use crate::workspace::PickerItemId;

impl App {
    pub(in crate::app) fn start_dap_configuration(&mut self, name: &str) {
        self.debugger_session.last_preset = None;
        match self.debugger_session.request_debug(
            (Some(name), None),
            None,
            None,
            (
                &self.configuration.settings().debug,
                &self.configuration.settings().terminals,
                self.configuration.binding_revision(),
            ),
            &self.workspace,
            self.tooling.entries(),
        ) {
            Ok(intent) => self.run_trusted(TrustedAction::StartDebug(intent)),
            Err(error) => self.show_error(format!("{error}; check [debug.adapters] and [debug.sessions] in config.toml, then press Space d s")),
        }
    }
}

impl App {
    pub(in crate::app) fn start_dap(
        &mut self,
        requested: Option<String>,
        program: Option<PathBuf>,
    ) {
        if requested.is_none() && program.is_none() {
            self.open_debug_launch_picker();
            return;
        }
        self.debugger_session.last_preset = None;
        let document = self
            .state
            .active_buffer_id()
            .and_then(|id| self.state.buffers.get(id))
            .and_then(|buffer| buffer.path.as_deref())
            .and_then(|path| language_id_for_path(path).map(|language| (path, language)));
        let Some(document) = document else {
            self.show_error(self.message("dap_no_buffer", &[]));
            return;
        };
        match self.debugger_session.request_debug(
            (None, requested.as_deref()),
            program,
            Some(document),
            (
                &self.configuration.settings().debug,
                &self.configuration.settings().terminals,
                self.configuration.binding_revision(),
            ),
            &self.workspace,
            self.tooling.entries(),
        ) {
            Ok(intent) => self.run_trusted(TrustedAction::StartDebug(intent)),
            Err(error) => self.show_error(error),
        }
    }
}

impl App {
    pub(in crate::app) fn authorize_debug_launch(
        &mut self,
        intent: crate::app::debugger::DebugIntent,
    ) {
        if let Err(error) = self
            .debugger_session
            .authorize_debug(intent, &self.workspace)
        {
            self.show_error(error);
            return;
        }
        self.advance_debug_launch();
    }
}

impl App {
    pub(in crate::app) fn advance_debug_launch(&mut self) {
        match self.debugger_session.advance_debug(
            self.workspace
                .execution_lane(crate::app::workspace::DiscoveryKind::Debugger),
            &self.configuration.settings().debug,
            self.configuration.binding_revision(),
            self.tooling.entries(),
            &self.state,
        ) {
            Ok(Some(name)) => {
                self.notify_message(self.message("dap_started", &[("adapter", name)]), false);
                self.notices.mark_current_passing();
            }
            Ok(None) => {}
            // A launch cancelled by revoked trust says how to get trust back, as tasks do.
            Err(error) => match self.workspace.execution_allowed() {
                Err(failure) => self.show_workspace_failure(failure),
                Ok(()) => self.show_error(error),
            },
        }
    }
}

impl App {
    pub(in crate::app) fn process_dap_project_root(&mut self) {
        if let Some(result) = self
            .workspace
            .project_result(crate::app::workspace::DiscoveryKind::Debugger)
            && let Err(error) = self.debugger_session.debug_root_result(result)
        {
            let _ = self.debugger_session.stop();
            self.show_error(error);
        }
        self.advance_debug_launch();
    }
}

impl App {
    pub(in crate::app) fn process_dap_events(&mut self) {
        self.poll_debug_presets();
        for error in self.debugger_session.reply_failures() {
            self.show_error(error);
        }
        self.debugger_session.refresh(
            &self.configuration.settings().debug,
            self.workspace.version(),
        );
        // These marks have no decoration of their own; only their projected signs affect drawing.
        self.debugger_session
            .sync_breakpoint_locations(&mut self.state.value);
        for _ in 0..256 {
            let Some(event) = self.debugger_session.try_event(&self.state) else {
                break;
            };
            match event {
                DapEvent::Starting { .. } => {}
                DapEvent::RequestFailed {
                    command, message, ..
                } => {
                    if !self.debugger_session.watch_request_failed(&command) {
                        self.show_error(message);
                    }
                }
                // "Starting" is over, and it would cover the variables the stop just filled in.
                // Keys go back to the source, or F10 would go to the program's terminal.
                DapEvent::Stopped { .. } => {
                    self.notices.clear_passing();
                    self.project_notice();
                    self.set_terminal_focus(false);
                }
                // The panes and the execution sign show these.
                DapEvent::Running { .. } | DapEvent::Output { .. } => {}
                DapEvent::Response {
                    response: DapResponse::Disconnected { .. },
                    ..
                } => {
                    self.notify_message(
                        if self.debugger_session.end_state()
                            == Some(crate::debugger::DapSessionEnd::Terminated)
                        {
                            "debug session terminated"
                        } else {
                            "debug session detached; program left running"
                        }
                        .into(),
                        false,
                    );
                }
                DapEvent::Response { response, .. } => {
                    match self.debugger_session.response(response) {
                        crate::app::debugger::DebugResponse::Stale
                        | crate::app::debugger::DebugResponse::Absorbed => {}
                        crate::app::debugger::DebugResponse::StoppedAt(location) => {
                            // A library frame outside the workspace stays in the stack pane.
                            if self.workspace.roots().iter().any(|root| {
                                location.path.starts_with(&root.identity.canonical_path)
                            }) {
                                self.navigate_debug_location(location);
                            }
                        }
                        crate::app::debugger::DebugResponse::StoppedSource(frame) => {
                            let target = crate::app::debugger::SourceTarget {
                                line: frame.line,
                                column: frame.column,
                                navigation: self.window_session.navigation_stamp(
                                    self.state.window_version(),
                                    self.state.active_view(),
                                ),
                                capture: self.input.capture_epoch(),
                            };
                            if let Err(error) = self.debugger_session.request_source(
                                frame.source_reference.expect("stopped source reference"),
                                target,
                            ) {
                                self.show_error(error);
                            }
                        }
                        crate::app::debugger::DebugResponse::Picker { kind, rows } => {
                            self.install_debug_rows(kind, rows)
                        }
                        crate::app::debugger::DebugResponse::Result(response) => {
                            match response {
                                DapResponse::Disconnected { .. }
                                | DapResponse::BreakpointChanged { .. } => {}
                                DapResponse::Evaluate(value) => {
                                    // The answer is recorded as an EVALUATION RESULT, which is a
                                    // different origin from the command that asked for it.
                                    self.debugger_session.record_console_evaluation(format!(
                                        "{} = {}",
                                        value.name, value.value
                                    ));
                                    self.notify_message(
                                        self.message(
                                            "dap_evaluate_result",
                                            &[("name", value.name), ("value", value.value)],
                                        ),
                                        false,
                                    );
                                }
                                DapResponse::Breakpoints(_) => {}
                                DapResponse::Threads(_)
                                | DapResponse::Stack(_)
                                | DapResponse::Scopes(_)
                                | DapResponse::Variables(_) => {
                                    unreachable!("owner converts response rows")
                                }
                                DapResponse::Source {
                                    name,
                                    content,
                                    mime_type,
                                } => {
                                    let Some(target) = self.debugger_session.take_source() else {
                                        continue;
                                    };
                                    if !self.window_session.navigation_current(
                                        &target.navigation,
                                        &self.state.window_version(),
                                        self.state.active_view(),
                                    ) || !self.input.capture_current(&target.capture)
                                    {
                                        continue;
                                    }
                                    let mut draft = self.state.draft();
                                    if let Err(error) = draft.open_virtual_read_only(
                                        TextBuffer::from_text(&content),
                                        "dap".into(),
                                        0,
                                        0,
                                        0,
                                    ) {
                                        self.show_error(error);
                                        continue;
                                    }
                                    if let Err(error) = self.commit_editor_draft(draft, None, false)
                                    {
                                        self.show_error(error);
                                        continue;
                                    }
                                    if let Some(view) = self.state.active_view() {
                                        let buffer = self.state.views[view].buffer;
                                        let text = &self.state.buffers[buffer].text;
                                        let line = target.line.saturating_sub(1) as usize;
                                        let line = line.min(text.len_lines().saturating_sub(1));
                                        let position = match PositionCodec::new(text).lsp_to_byte(
                                            line,
                                            target.column.saturating_sub(1) as usize,
                                            PositionEncoding::Utf16,
                                        ) {
                                            Ok(position) => position,
                                            Err(error) => {
                                                self.show_error(error);
                                                continue;
                                            }
                                        };
                                        self.state.views[view].cursor = position;
                                        self.state.mark_editor_changed();
                                        self.ensure_cursor_visible();
                                    }
                                    self.notify_message(
                                        self.message(
                                            "dap_source_loaded",
                                            &[
                                                ("name", name),
                                                ("mime", mime_type.unwrap_or_default()),
                                            ],
                                        ),
                                        false,
                                    );
                                }
                            }
                        }
                    }
                }
                DapEvent::Terminated { adapter } => {
                    let _ = self.debugger_session.stop();
                    self.notify_message(
                        self.message(
                            "dap_stopped",
                            &[("adapter", adapter), ("reason", "terminated".into())],
                        ),
                        false,
                    );
                }
                DapEvent::RunInTerminal {
                    reply,
                    cwd,
                    arguments,
                    environment,
                    ..
                } => {
                    let reply = reply.claim();
                    if let Some(program) =
                        self.debugger_session
                            .adapter_terminal(&cwd, &arguments, &environment)
                    {
                        self.open_adapter_terminal(program, reply);
                        continue;
                    }
                    let profile = match self.debugger_session.terminal_request(
                        &cwd,
                        &arguments,
                        &environment,
                        &self.configuration.settings().terminals,
                    ) {
                        Ok(profile) => profile.to_owned(),
                        Err(error) => {
                            let _ = reply.respond(false, Some(error));
                            continue;
                        }
                    };
                    if !reply.current() {
                        continue;
                    }
                    self.request_terminal_open(&profile, Some(reply));
                }
                DapEvent::Failed { adapter, message } => {
                    let _ = self.debugger_session.stop();
                    // A rejected launch leaves an empty terminal whose "exit 0" reads as a finished run.
                    self.close_debug_terminal();
                    self.show_error(self.message(
                        "dap_failed",
                        &[("adapter", adapter), ("diagnostic", message)],
                    ));
                }
            }
        }

        // The arrangement APPEARS when the session does: the panes are projected on every pump, so
        // starting, stopping and reporting all refresh what the layout shows.
        self.project_debug_panes();
    }
}

impl App {
    /// The program's terminal from the last run. It stays after a run ends so its output can be
    /// read, and goes when the user stops the program or runs it again.
    pub(in crate::app) fn close_debug_terminal(&mut self) {
        let Some(id) = self.debug_terminal.take() else {
            return;
        };
        if self.terminal_session.request_stop(id).is_ok() {
            self.sync_terminal_panel();
        }
    }
}

impl App {
    fn open_adapter_terminal(
        &mut self,
        program: crate::config::TerminalProfileConfig,
        reply: crate::debugger::DapTerminalReply,
    ) {
        match self.terminal_session.open_program(
            crate::app::terminal::DEBUG_TERMINAL_PROFILE,
            program,
            &self.workspace,
            self.window_session
                .navigation_stamp(self.state.window_version(), self.state.active_view()),
            self.input.capture_epoch(),
            false,
        ) {
            Ok(intent) => self.run_trusted(TrustedAction::OpenDapTerminal { intent, reply }),
            Err(error) => {
                let _ = reply.respond(false, Some(error.clone()));
                self.show_error(error);
            }
        }
    }
}

impl App {
    pub(in crate::app) fn navigate_debug_location(
        &mut self,
        location: crate::app::debugger::DebugLocation,
    ) {
        let position = crate::language::lsp::LspPosition {
            line: usize::try_from(location.line.saturating_sub(1)).unwrap_or(usize::MAX),
            character: usize::try_from(location.column.saturating_sub(1)).unwrap_or(usize::MAX),
        };
        self.navigate_location_encoded(
            &crate::language::lsp::LspLocation {
                uri: crate::language::lsp::file_uri(&location.path),
                range: crate::language::lsp::LspRange {
                    start: position.clone(),
                    end: position,
                },
            },
            PositionEncoding::Utf16,
            true,
        );
    }
}

impl App {
    pub(in crate::app) fn open_dap_picker(
        &mut self,
        kind: &str,
        rows: Vec<(String, String, DapControl)>,
    ) {
        let saved = rows
            .iter()
            .map(|(label, detail, _)| (label.clone(), detail.clone()))
            .collect::<Vec<_>>();
        let rows = self.debugger_session.controls(rows);
        if kind == "watches" && rows.is_empty() {
            let items = saved
                .into_iter()
                .enumerate()
                .map(|(index, (label, detail))| {
                    (
                        PickerItem {
                            badge: String::new(),
                            kind: crate::workspace::PickerItemKind::Entry,
                            id: PickerItemId(format!("dap:watches:{index}")),
                            label,
                            detail,
                            source: "dap".into(),
                            action: PickerAction::None,
                        },
                        PickerPayload::Command(String::new()),
                    )
                })
                .collect();
            self.install_surface_picker(
                PickerSurfaceKind::Debug,
                self.message("dap_picker_title", &[("kind", kind.into())]),
                items,
            );
        } else {
            self.install_debug_rows(kind, rows);
        }
    }
}

impl App {
    pub(in crate::app) fn install_debug_rows(
        &mut self,
        kind: &str,
        rows: Vec<crate::app::debugger::DebugRow>,
    ) {
        let items = rows
            .into_iter()
            .enumerate()
            .map(|(index, row)| {
                (
                    PickerItem {
                        badge: String::new(),
                        kind: crate::workspace::PickerItemKind::Entry,
                        id: PickerItemId(format!("dap:{kind}:{index}")),
                        label: row.label,
                        detail: row.detail,
                        source: "dap".into(),
                        action: PickerAction::None,
                    },
                    PickerPayload::Debug(row.key),
                )
            })
            .collect();
        self.install_surface_picker(
            PickerSurfaceKind::Debug,
            self.message("dap_picker_title", &[("kind", kind.into())]),
            items,
        );
    }
}

impl App {
    fn cursor_breakpoint_place(&self) -> Option<(PathBuf, u64)> {
        let view = &self.state.views[self.state.active_view()?];
        let buffer = &self.state.buffers[view.buffer];
        let line = buffer.text.byte_to_line(view.cursor).ok()? as u64 + 1;
        Some((buffer.path.clone()?, line))
    }
}

impl App {
    pub(in crate::app) fn configure_breakpoint(
        &mut self,
        condition: Option<String>,
        hit_condition: Option<String>,
        log_message: Option<String>,
    ) {
        let Some((path, line)) = self.cursor_breakpoint_place() else {
            return;
        };
        if let Err(error) = self.debugger_session.breakpoint_on_display_line(
            DapSourceBreakpoint {
                path,
                line,
                column: None,
                condition,
                hit_condition,
                log_message,
            },
            false,
            &mut self.state,
        ) {
            self.show_error(error);
        }
    }
}

impl App {
    pub(in crate::app) fn remove_picked_breakpoint(&mut self, path: &Path, identity: BreakpointId) {
        let Some(row) = self
            .debugger_session
            .breakpoints
            .get(path)
            .and_then(|rows| rows.iter().find(|row| row.identity == identity))
        else {
            return;
        };
        let requested = row.request_row(path, &self.state);
        match self.debugger_session.change_breakpoint(
            requested,
            Some(identity),
            true,
            &mut self.state,
        ) {
            Ok(_) => self.project_debug_panes(),
            Err(error) => self.show_error(error),
        }
    }

    pub(in crate::app) fn open_breakpoint_states(&mut self) {
        let running = self.debugger_session.is_running();
        let items = self
            .debugger_session
            .breakpoint_rows()
            .map(|(path, row)| {
                let line = row.display_line(path, &self.state);
                let state = row
                    .state()
                    .filter(|state| running || *state == DapBreakpointState::Disabled)
                    .map(DapBreakpointState::label);
                let mut detail = state.unwrap_or_default().to_owned();
                if let Some(condition) = &row.condition {
                    if !detail.is_empty() {
                        detail.push_str(", ");
                    }
                    detail.push_str("if ");
                    detail.push_str(condition);
                }
                if let Some(message) = &row.log_message {
                    if !detail.is_empty() {
                        detail.push_str(", ");
                    }
                    // Keep the full message searchable, but let it clip after the condition.
                    detail.push_str("logs: ");
                    detail.push_str(message);
                }
                (
                    PickerItem {
                        badge: String::new(),
                        kind: crate::workspace::PickerItemKind::Entry,
                        id: PickerItemId(format!("breakpoint:{}", row.identity.0)),
                        label: format!("{}:{line}", self.workspace_relative_label(path)),
                        detail,
                        source: "debugger".into(),
                        action: PickerAction::None,
                    },
                    PickerPayload::Breakpoint {
                        path: path.into(),
                        identity: row.identity,
                    },
                )
            })
            .collect::<Vec<_>>();
        let title = format!("Breakpoints · {}", items.len());
        self.install_surface_picker_state(
            PickerSurfaceKind::Generic,
            title,
            items,
            String::new(),
            true,
            PickerOrigin::Breakpoints,
        );
    }
}

impl App {
    /// Enable or disable the breakpoint on the cursor line. Disabling is SESSION state, so it
    /// survives whatever the adapter later says about that breakpoint.
    pub(in crate::app) fn toggle_breakpoint_disabled(&mut self) {
        let Some(view) = self.state.active_view() else {
            self.show_error("no view is active");
            return;
        };
        let Some(view_state) = self.state.views.get(view) else {
            self.show_error("no view is active");
            return;
        };
        let buffer = view_state.buffer;
        let cursor = view_state.cursor;
        let Some(path) = self
            .state
            .buffers
            .get(buffer)
            .and_then(|buffer| buffer.path.clone())
        else {
            self.show_error("active buffer has no file path");
            return;
        };
        let Some(buffer) = self.state.buffers.get(buffer) else {
            return;
        };
        let line = buffer.text.byte_to_line(cursor).unwrap_or(0) as u64 + 1;
        let Some(row) = self
            .debugger_session
            .displayed_breakpoint(&path, line, &self.state)
        else {
            self.show_error("no breakpoint on this line");
            return;
        };
        let disabled = !row.disabled;
        let Some(line) = self.debugger_session.set_breakpoint_disabled_by_id(
            &path,
            row.identity,
            disabled,
            &self.state,
        ) else {
            return;
        };
        self.notify_message(
            format!(
                "breakpoint {}:{} is now {}",
                path.display(),
                line,
                if disabled { "disabled" } else { "enabled" }
            ),
            false,
        );
    }
}

impl App {
    /// TERMINATE: ask for the debuggee to end with the session.
    pub(in crate::app) fn terminate_debug_session(&mut self) {
        if self.debugger_session.install_launch.is_some()
            || self.debugger_session.cargo_build.is_some()
        {
            self.debugger_session.cancel_preset_launch();
            self.notify_message("Debug launch cancelled; adapter installation, if started, remains in Tools (Space c m)".into(), false);
            return;
        }
        if let Err(error) = self.debugger_session.terminate_debuggee() {
            self.show_error(error);
        }
    }
}

impl App {
    /// Leave the debuggee running after the adapter accepts disconnect.
    pub(in crate::app) fn detach_debug_session(&mut self) {
        if let Err(error) = self.debugger_session.detach_debuggee() {
            self.show_error(error);
        }
    }
}

impl App {
    /// Project the operation bar from the repository's REAL state. It is projected whenever the
    /// status is refreshed rather than per frame, and it is cleared when nothing is paused, so
    /// its presence always means an operation is genuinely in progress.
    /// Project the debug panes from the session's OWN state: what it is attached to or running,
    /// the breakpoints it holds, its watches and its console. Absent when there is no session, so
    /// the whole arrangement appears and disappears as ONE decision.
    pub(in crate::app) fn project_debug_panes(&mut self) {
        self.debugger_session
            .remove_discarded_breakpoint_marks(&mut self.state);
        self.project_debug_signs();
        if std::mem::take(&mut self.debugger_session.breakpoint_picker_dirty)
            && self.search.breakpoint_picker_open(
                self.workspace.version(),
                self.configuration.binding_revision(),
            )
        {
            self.open_breakpoint_states();
            // A reply can change only the picker's status, without moving a gutter sign.
            self.ui.revision = self.ui.revision.wrapping_add(1);
        }
        let was_open = self.ui.debug_panes.is_some();
        let target = self
            .debugger_session
            .target()
            .filter(|_| self.debugger_session.is_running());
        let Some(target) = target else {
            self.ui.set(None, |ui| &mut ui.debug_panes);
            if was_open {
                self.window_session.set_debugging(false);
                self.sync_terminal_panel();
                // The windows make room for the panes, or take it back.
                self.window_session.invalidate();
                self.reconcile_editor_windows();
            }
            return;
        };
        let selected = self
            .ui
            .debug_panes
            .as_ref()
            .map_or(crate::ui::DebugPane::Sessions, |panes| panes.selected);
        let what = match target.request {
            crate::config::DebugRequestKind::Attach => match &target.target {
                Some(name) => format!("attached to {name}"),
                None => "attached".to_owned(),
            },
            crate::config::DebugRequestKind::Launch => match &target.program {
                Some(program) => format!("launched {}", self.workspace_relative_label(program)),
                None => "launched".to_owned(),
            },
        };
        let adapter = target.adapter.clone();
        let breakpoints = self
            .debugger_session
            .breakpoint_rows()
            .map(|(path, row)| {
                let line = row.display_line(path, &self.state);
                let place = format!("{}:{line}", self.workspace_relative_label(path));
                match &row.condition {
                    Some(condition) => format!("{place} if {condition}"),
                    None => place,
                }
            })
            .collect();
        let threads = self.debugger_session.threads().to_vec();
        let stack = self.debugger_session.stack().to_vec();
        let watches = self
            .debugger_session
            .watches()
            .iter()
            .map(|watch| match self.debugger_session.watch_value(watch) {
                Some(value) => format!("{watch} = {value}"),
                None => watch.clone(),
            })
            .collect();
        let status = match self.debugger_session.stop_reason() {
            Some(reason) => format!("paused: {reason}"),
            None => "running".to_owned(),
        };
        let console = self
            .debugger_session
            .console()
            .map(|entry| entry.text.clone())
            .collect();
        self.ui.debug_panes = Some(crate::ui::DebugPanesUiState {
            selected,
            target: format!("{adapter}: {what}"),
            status,
            threads,
            stack,
            breakpoints,
            variables: self.debugger_session.variables().to_vec(),
            watches,
            console,
        });
        if !was_open {
            self.window_session.set_debugging(true);
            self.sync_terminal_panel();
            // The windows make room for the panes, or take it back.
            self.window_session.invalidate();
            self.reconcile_editor_windows();
        }
    }
}

impl App {
    fn project_debug_signs(&mut self) {
        use crate::ui::{DebugSign, DebugSignKind};

        let mut signs = Vec::new();
        let execution = self.debugger_session.execution().cloned();
        let mark = &mut self.debugger_session.execution_mark;
        if let Some((path, line, location)) = mark
            && execution
                .as_ref()
                .is_none_or(|(source, row)| path != source || line != row)
        {
            location.remove_mark(&mut self.state.value);
            *mark = None;
        }
        if let Some((path, line)) = execution {
            let (_, _, location) =
                mark.get_or_insert_with(|| (path.clone(), line, BreakpointLocation::Closed(line)));
            if location.live_line(&path, &self.state).is_none() {
                location.remove_mark(&mut self.state.value);
                *location = BreakpointLocation::attach(&path, line, &mut self.state.value);
            }
            // Execution wins if it shares a line with a breakpoint.
            signs.push(DebugSign {
                line: location.line(&path, &self.state).saturating_sub(1) as usize,
                path,
                kind: DebugSignKind::Execution,
            });
        }
        signs.extend(
            self.debugger_session
                .breakpoint_rows()
                .map(|(path, row)| DebugSign {
                    path: path.into(),
                    line: row.display_line(path, &self.state).saturating_sub(1) as usize,
                    kind: if row.disabled {
                        DebugSignKind::DisabledBreakpoint
                    } else if row.condition.is_some()
                        || row.hit_condition.is_some()
                        || row.log_message.is_some()
                    {
                        DebugSignKind::ConditionalBreakpoint
                    } else {
                        DebugSignKind::Breakpoint
                    },
                }),
        );
        self.ui.set(signs, |ui| &mut ui.debug_signs);
    }
}

#[cfg(test)]
pub(in crate::app) mod tests;
