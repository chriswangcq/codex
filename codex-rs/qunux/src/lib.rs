use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use thiserror::Error;

pub const SCHEMA_VERSION: u32 = 8;
pub const DEFAULT_PROCESS_ID: &str = "QP000";
pub const DEFAULT_THREAD_ID: &str = "QT000";
pub const DEFAULT_ROOT_PROBLEM_TITLE: &str = "Serve the user well in this Qunux process";
const LEGACY_DEFAULT_ROOT_PROBLEM_TITLE: &str = "Qunux root task";
pub const DEFAULT_ROOT_PROBLEM_BODY: &str = r#"# Serve the user well in this Qunux process

## Problem

This Qunux process exists to help the user effectively, safely, and continuously for the lifetime of this Codex session. It is a headless-first Agent OS process: the TUI cockpit, chat transcript, shell, files, tools, timers, webhooks, and external systems are all world I/O surfaces around the same durable runtime state.

At process birth, Qunux creates exactly this root problem as ordinary PTRC state. Qunux does not pre-create a ticket, pre-classify the problem, create child problems, record results or checks, or decide whether the process should wait. The LLM agent reads this problem, the current world event, and the runtime frontier, then decides the next legal PTRC move.

This root problem is a lifecycle mission, not an initialization task. Do not close it just because the process is ready. When there is no concrete demand from the user or world, park the thread with `qunux.wait` for user input, timer, external signal, child completion, or another relevant wake event. Close this root only for an explicit shutdown/end-of-process request.

## Success Criteria

- Understand whether each world input is conversational, clarifying, actionable, background signal, or absent.
- For actionable work, use the normal problem -> ticket -> result -> check closure loop.
- For broad or risky work, split into child problems instead of pretending the root is one-go.
- For pure small talk, narrow meta questions, acknowledgements, or "wait for my next message", answer visibly in the current thread and then wait; do not create a routing child problem or spawn a child thread merely to handle conversation plumbing.
- For unclear input, ask a focused clarification or wait instead of inventing work.
- For no current demand, wait for user input, timer, external signal, child completion, or another relevant wake event.
- Readiness, initialization, or absence of current work are not success conditions for closing the root problem.
- Keep visible user replies separate from Qunux state mutation: assistant messages are output; Qunux tools update runtime state.
- Preserve useful preferences, context, and evidence when they help future work.
- Avoid false completion: success requires evidence, criteria mapping, stress testing, and explicit residual-risk review.
"#;

#[derive(Debug, Error)]
pub enum QunuxError {
    #[error("invalid id `{0}`")]
    InvalidId(String),
    #[error("state error: {0}")]
    InvalidState(String),
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("json error at {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

pub type Result<T> = std::result::Result<T, QunuxError>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeContext {
    pub workspace_root: PathBuf,
    pub process_id: String,
    pub thread_id: String,
    pub actor_session_id: Option<String>,
    pub parent_actor_session_id: Option<String>,
}

impl RuntimeContext {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            process_id: DEFAULT_PROCESS_ID.to_string(),
            thread_id: DEFAULT_THREAD_ID.to_string(),
            actor_session_id: None,
            parent_actor_session_id: None,
        }
    }

    pub fn with_ids(
        workspace_root: impl Into<PathBuf>,
        process_id: impl Into<String>,
        thread_id: impl Into<String>,
    ) -> Result<Self> {
        let process_id = process_id.into();
        let thread_id = thread_id.into();
        validate_id(&process_id)?;
        validate_id(&thread_id)?;
        Ok(Self {
            workspace_root: workspace_root.into(),
            process_id,
            thread_id,
            actor_session_id: None,
            parent_actor_session_id: None,
        })
    }

    pub fn for_session(
        workspace_root: impl Into<PathBuf>,
        actor_session_id: impl Into<String>,
    ) -> Result<Self> {
        let actor_session_id = actor_session_id.into();
        validate_id(&actor_session_id)?;
        let process_id = process_id_for_session_id(&actor_session_id)?;
        Ok(Self {
            workspace_root: workspace_root.into(),
            process_id,
            thread_id: DEFAULT_THREAD_ID.to_string(),
            actor_session_id: Some(actor_session_id),
            parent_actor_session_id: None,
        })
    }

    pub fn with_actor_session_id(mut self, actor_session_id: impl Into<String>) -> Result<Self> {
        let actor_session_id = actor_session_id.into();
        validate_id(&actor_session_id)?;
        self.actor_session_id = Some(actor_session_id);
        Ok(self)
    }

    pub fn with_parent_actor_session_id(
        mut self,
        parent_actor_session_id: impl Into<String>,
    ) -> Result<Self> {
        let parent_actor_session_id = parent_actor_session_id.into();
        validate_id(&parent_actor_session_id)?;
        self.parent_actor_session_id = Some(parent_actor_session_id);
        Ok(self)
    }

    pub fn qunux_dir(&self) -> PathBuf {
        self.workspace_root.join(".qunux")
    }

    pub fn process_dir(&self) -> PathBuf {
        self.qunux_dir()
            .join("processes")
            .join(self.process_id.as_str())
    }

    pub fn state_path(&self) -> PathBuf {
        self.process_dir().join("closure.json")
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProblemStatus {
    Todo,
    Doing,
    Checking,
    Followup,
    Done,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TicketStatus {
    Created,
    Defined,
    Classified,
    Executing,
    Splitting,
    Done,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TicketClassification {
    OneGo,
    Split,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TicketChildMode {
    Split,
    Spawn,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Success,
    NotSuccess,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NextAction {
    CreateSolutionTicket,
    DefineTicket,
    ClassifyTicket,
    ExecuteTicket,
    SplitTicket,
    SpawnThread,
    WaitThread,
    WaitIo,
    HandleInbox,
    JoinThread,
    RecoverThread,
    RecordResult,
    CheckSuccess,
    None,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NextDisposition {
    Runnable,
    IoWait,
    Terminal,
}

impl NextDisposition {
    fn for_action(action: NextAction) -> Self {
        match action {
            NextAction::WaitThread | NextAction::WaitIo => Self::IoWait,
            NextAction::None => Self::Terminal,
            NextAction::CreateSolutionTicket
            | NextAction::DefineTicket
            | NextAction::ClassifyTicket
            | NextAction::ExecuteTicket
            | NextAction::SplitTicket
            | NextAction::SpawnThread
            | NextAction::HandleInbox
            | NextAction::JoinThread
            | NextAction::RecoverThread
            | NextAction::RecordResult
            | NextAction::CheckSuccess => Self::Runnable,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    Running,
    WaitingChildren,
    WaitingIo,
    Done,
    Failed,
    Cancelled,
    Recovered,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IoHandleKind {
    ChildThread,
    UserInput,
    Timer,
    ExternalSignal,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IoHandleStatus {
    Pending,
    Ready,
    Consumed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WaitMode {
    All,
    Any,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WaitStatus {
    Waiting,
    Ready,
    Consumed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IoEventKind {
    ChildThreadSpawned,
    ChildThreadDone,
    ChildThreadJoined,
    ActorCompletedWithoutThreadDone,
    ChildThreadFailed,
    ChildThreadSpawnFailed,
    ChildThreadRecovered,
    HandleReady,
    PassiveEventReceived,
    PassiveWaitCreated,
    PassiveEventMatched,
    PassiveEventInboxed,
    PassiveEventHandled,
    PassiveHandleReady,
    PassiveHandleConsumed,
    PassiveHandleCancelled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PassiveEventKind {
    UserInput,
    Timer,
    ExternalSignal,
}

impl PassiveEventKind {
    fn handle_kind(self) -> IoHandleKind {
        match self {
            Self::UserInput => IoHandleKind::UserInput,
            Self::Timer => IoHandleKind::Timer,
            Self::ExternalSignal => IoHandleKind::ExternalSignal,
        }
    }

    fn event_key_kind(self) -> &'static str {
        match self {
            Self::UserInput => "user.input",
            Self::Timer => "timer",
            Self::ExternalSignal => "external.signal",
        }
    }

    fn from_event_key_kind(kind: &str) -> Option<Self> {
        match kind {
            "user.input" => Some(Self::UserInput),
            "timer" => Some(Self::Timer),
            "external.signal" => Some(Self::ExternalSignal),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WaitSpecKind {
    UserInput,
    Timer,
    ExternalSignal,
    EventKey,
}

impl WaitSpecKind {
    fn passive_kind(self) -> Option<PassiveEventKind> {
        match self {
            Self::UserInput => Some(PassiveEventKind::UserInput),
            Self::Timer => Some(PassiveEventKind::Timer),
            Self::ExternalSignal => Some(PassiveEventKind::ExternalSignal),
            Self::EventKey => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EventKey {
    pub kind: String,
    #[serde(default)]
    pub resource: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub target_thread_id: Option<String>,
}

impl EventKey {
    pub fn new(
        kind: impl Into<String>,
        resource: Option<String>,
        source: Option<String>,
        target_thread_id: Option<String>,
    ) -> Self {
        Self {
            kind: kind.into(),
            resource,
            source,
            target_thread_id,
        }
    }

    pub fn passive(
        kind: PassiveEventKind,
        resource: Option<String>,
        source: Option<String>,
        target_thread_id: Option<String>,
    ) -> Self {
        Self::new(kind.event_key_kind(), resource, source, target_thread_id)
    }

    fn passive_kind(&self) -> PassiveEventKind {
        PassiveEventKind::from_event_key_kind(&self.kind)
            .unwrap_or(PassiveEventKind::ExternalSignal)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PassiveEventStatus {
    Inboxed,
    Matched,
    Handled,
    Duplicate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PassiveEvent {
    pub id: String,
    pub kind: PassiveEventKind,
    #[serde(default)]
    pub event_key: Option<EventKey>,
    pub status: PassiveEventStatus,
    pub target_thread_id: Option<String>,
    pub condition: Option<String>,
    pub source: Option<String>,
    pub summary: String,
    pub payload_ref: Option<String>,
    pub dedupe_key: Option<String>,
    pub matched_handle_ids: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InboxItem {
    pub id: String,
    pub passive_event_id: String,
    #[serde(default)]
    pub event_key: Option<EventKey>,
    pub target_thread_id: Option<String>,
    #[serde(default)]
    pub condition: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    pub summary: String,
    #[serde(default)]
    pub payload_ref: Option<String>,
    #[serde(default)]
    pub dedupe_key: Option<String>,
    pub status: PassiveEventStatus,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IoHandle {
    pub id: String,
    pub kind: IoHandleKind,
    pub owner_thread_id: String,
    pub target_thread_id: Option<String>,
    #[serde(default)]
    pub event_key: Option<EventKey>,
    #[serde(default)]
    pub condition: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub payload_ref: Option<String>,
    #[serde(default)]
    pub dedupe_key: Option<String>,
    #[serde(default)]
    pub resolved_event_id: Option<String>,
    pub status: IoHandleStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadWait {
    pub id: String,
    pub thread_id: String,
    pub handle_ids: Vec<String>,
    pub mode: WaitMode,
    pub status: WaitStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IoEvent {
    pub id: String,
    pub kind: IoEventKind,
    pub handle_id: Option<String>,
    pub thread_id: Option<String>,
    pub message: String,
    pub actor_thread_id: String,
    pub actor_session_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextForkPolicy {
    FullContext,
    SummaryContext,
    FreshContext,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextFork {
    pub policy: ContextForkPolicy,
    pub parent_thread_id: Option<String>,
    pub parent_actor_session_id: Option<String>,
    pub fork_turn_id: Option<String>,
    pub bootstrap_instruction: String,
    pub inherited_cwd: Option<String>,
    pub inherited_model: Option<String>,
    pub inherited_tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Thread {
    pub id: String,
    pub process_id: String,
    pub parent_thread_id: Option<String>,
    pub root_problem_id: String,
    pub status: ThreadStatus,
    pub context_fork: ContextFork,
    pub child_thread_ids: Vec<String>,
    pub created_from_problem_id: String,
    pub created_from_ticket_id: Option<String>,
    pub created_from_check_id: Option<String>,
    pub actor_session_id: Option<String>,
    pub codex_thread_id: Option<String>,
    pub joined_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadActorRoute {
    pub thread_id: String,
    pub is_current_thread: bool,
    pub actor_session_id: Option<String>,
    pub codex_thread_id: Option<String>,
}

impl ThreadActorRoute {
    pub fn bound_actor_id(&self) -> Option<&str> {
        self.codex_thread_id
            .as_deref()
            .or(self.actor_session_id.as_deref())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Problem {
    pub id: String,
    pub title: String,
    pub body: String,
    pub status: ProblemStatus,
    pub owner_thread_id: String,
    pub parent_id: Option<String>,
    pub created_from_ticket_id: Option<String>,
    #[serde(default)]
    pub created_from_ticket_mode: Option<TicketChildMode>,
    pub created_from_check_id: Option<String>,
    #[serde(default)]
    pub created_from_passive_event_id: Option<String>,
    #[serde(default)]
    pub created_from_inbox_item_id: Option<String>,
    #[serde(default)]
    pub created_from_user_task_kind: Option<PassiveEventKind>,
    pub ticket_id: Option<String>,
    pub child_problem_ids: Vec<String>,
    pub followup_problem_ids: Vec<String>,
    pub result_ids: Vec<String>,
    pub check_ids: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Ticket {
    pub id: String,
    pub problem_id: String,
    pub title: String,
    pub body: String,
    pub status: TicketStatus,
    pub classification: Option<TicketClassification>,
    pub classification_reason: Option<String>,
    pub result_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionResult {
    pub id: String,
    pub ticket_id: String,
    pub problem_id: String,
    pub title: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Check {
    pub id: String,
    pub problem_id: String,
    pub status: CheckStatus,
    pub title: String,
    pub body: String,
    pub result_ids: Vec<String>,
    pub followup_problem_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserTaskProblem {
    pub problem_id: String,
    pub parent_problem_id: String,
    pub inbox_item_id: String,
    pub passive_event_id: String,
    pub source_kind: PassiveEventKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScaffoldedTask {
    pub problem_id: String,
    pub parent_problem_id: String,
    pub ticket_id: String,
    pub inbox_item_id: Option<String>,
    pub passive_event_id: Option<String>,
    pub source_kind: Option<PassiveEventKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Event {
    pub kind: String,
    pub entity_id: String,
    pub message: String,
    pub actor_thread_id: String,
    pub actor_session_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessMetadata {
    pub id: String,
    pub root_actor_session_id: Option<String>,
    pub main_thread_id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClosureState {
    pub schema_version: u32,
    pub process_id: String,
    pub main_thread_id: String,
    pub process: ProcessMetadata,
    pub root_problem_id: String,
    pub next_thread_seq: u32,
    pub next_handle_seq: u32,
    pub next_wait_seq: u32,
    pub next_io_event_seq: u32,
    #[serde(default)]
    pub next_passive_event_seq: u32,
    #[serde(default)]
    pub next_inbox_item_seq: u32,
    pub next_problem_seq: u32,
    pub next_ticket_seq: u32,
    pub next_result_seq: u32,
    pub next_check_seq: u32,
    pub threads: BTreeMap<String, Thread>,
    pub handles: BTreeMap<String, IoHandle>,
    pub waits: BTreeMap<String, ThreadWait>,
    pub problems: BTreeMap<String, Problem>,
    pub tickets: BTreeMap<String, Ticket>,
    pub results: BTreeMap<String, ExecutionResult>,
    pub checks: BTreeMap<String, Check>,
    #[serde(default)]
    pub passive_events: Vec<PassiveEvent>,
    #[serde(default)]
    pub inbox: Vec<InboxItem>,
    pub io_events: Vec<IoEvent>,
    pub events: Vec<Event>,
}

impl ClosureState {
    pub fn new_root(
        process_id: String,
        main_thread_id: String,
        actor_session_id: Option<String>,
        title: String,
        body: String,
    ) -> Self {
        let now = Utc::now();
        let root_problem_id = "P000".to_string();
        let process = ProcessMetadata {
            id: process_id.clone(),
            root_actor_session_id: actor_session_id.clone(),
            main_thread_id: main_thread_id.clone(),
            created_at: now,
            updated_at: now,
        };
        let root_context_fork = ContextFork {
            policy: ContextForkPolicy::FullContext,
            parent_thread_id: None,
            parent_actor_session_id: None,
            fork_turn_id: None,
            bootstrap_instruction: "Root Qunux thread.".to_string(),
            inherited_cwd: None,
            inherited_model: None,
            inherited_tools: Vec::new(),
        };
        let root_thread = Thread {
            id: main_thread_id.clone(),
            process_id: process_id.clone(),
            parent_thread_id: None,
            root_problem_id: root_problem_id.clone(),
            status: ThreadStatus::Running,
            context_fork: root_context_fork,
            child_thread_ids: Vec::new(),
            created_from_problem_id: root_problem_id.clone(),
            created_from_ticket_id: None,
            created_from_check_id: None,
            actor_session_id: actor_session_id.clone(),
            codex_thread_id: actor_session_id.clone(),
            joined_at: None,
            created_at: now,
            updated_at: now,
        };
        let mut threads = BTreeMap::new();
        threads.insert(main_thread_id.clone(), root_thread);
        let mut problems = BTreeMap::new();
        problems.insert(
            root_problem_id.clone(),
            Problem {
                id: root_problem_id.clone(),
                title,
                body,
                status: ProblemStatus::Todo,
                owner_thread_id: main_thread_id.clone(),
                parent_id: None,
                created_from_ticket_id: None,
                created_from_ticket_mode: None,
                created_from_check_id: None,
                created_from_passive_event_id: None,
                created_from_inbox_item_id: None,
                created_from_user_task_kind: None,
                ticket_id: None,
                child_problem_ids: Vec::new(),
                followup_problem_ids: Vec::new(),
                result_ids: Vec::new(),
                check_ids: Vec::new(),
                created_at: now,
                updated_at: now,
            },
        );
        Self {
            schema_version: SCHEMA_VERSION,
            process_id,
            main_thread_id: main_thread_id.clone(),
            process,
            root_problem_id,
            next_thread_seq: 1,
            next_handle_seq: 0,
            next_wait_seq: 0,
            next_io_event_seq: 0,
            next_passive_event_seq: 0,
            next_inbox_item_seq: 0,
            next_problem_seq: 1,
            next_ticket_seq: 0,
            next_result_seq: 0,
            next_check_seq: 0,
            threads,
            handles: BTreeMap::new(),
            waits: BTreeMap::new(),
            problems,
            tickets: BTreeMap::new(),
            results: BTreeMap::new(),
            checks: BTreeMap::new(),
            passive_events: Vec::new(),
            inbox: Vec::new(),
            io_events: Vec::new(),
            events: vec![Event {
                kind: "process_created".to_string(),
                entity_id: "P000".to_string(),
                message: "Qunux closure process initialized".to_string(),
                actor_thread_id: main_thread_id,
                actor_session_id,
                created_at: now,
            }],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NextStep {
    pub action: NextAction,
    pub disposition: NextDisposition,
    pub thread_id: String,
    pub target_thread_id: Option<String>,
    pub problem_id: Option<String>,
    pub ticket_id: Option<String>,
    pub instruction: String,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Problem,
    Ticket,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeStatus {
    pub process_id: String,
    pub thread_id: String,
    pub thread_status: ThreadStatus,
    pub root_problem_id: String,
    pub thread_root_problem_id: String,
    pub total_threads: usize,
    pub open_threads: usize,
    pub total_problems: usize,
    pub open_problems: usize,
    pub total_tickets: usize,
    pub total_results: usize,
    pub total_checks: usize,
    pub total_handles: usize,
    pub pending_handles: usize,
    pub ready_handles: usize,
    pub failed_threads: usize,
    pub failed_handles: usize,
    pub waiting_threads: usize,
    pub passive_events: usize,
    pub inbox_items: usize,
    pub pending_inbox_items: usize,
    pub valid: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CurrentState {
    pub context: RuntimeContext,
    pub status: RuntimeStatus,
    pub next: NextStep,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpawnedThread {
    pub thread_id: String,
    pub root_problem_id: String,
    pub handle_id: String,
    pub wait_id: String,
    pub bootstrap_instruction: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JoinedThread {
    pub thread_id: String,
    pub root_problem_id: String,
    pub parent_thread_id: String,
    pub handle_id: String,
    pub wait_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecoveredThread {
    pub thread_id: String,
    pub root_problem_id: String,
    pub parent_thread_id: String,
    pub handle_id: String,
    pub wait_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PassiveEventInput {
    pub kind: PassiveEventKind,
    #[serde(default)]
    pub event_key: Option<EventKey>,
    pub target_thread_id: Option<String>,
    pub condition: Option<String>,
    pub source: Option<String>,
    pub summary: String,
    pub payload_ref: Option<String>,
    pub dedupe_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoutedPassiveEvent {
    pub kind: PassiveEventKind,
    pub event_key: EventKey,
    pub target_thread_id: Option<String>,
    pub input: PassiveEventInput,
}

pub struct EventRoutingAgent;

impl EventRoutingAgent {
    pub fn route_passive_event(input: PassiveEventInput) -> Result<RoutedPassiveEvent> {
        let event_key = input.event_key.clone().unwrap_or_else(|| {
            EventKey::passive(
                input.kind,
                input.condition.clone(),
                input.source.clone(),
                input.target_thread_id.clone(),
            )
        });
        if let Some(input_target_thread_id) = input.target_thread_id.as_deref()
            && event_key.target_thread_id.as_deref() != Some(input_target_thread_id)
        {
            return Err(QunuxError::InvalidState(format!(
                "passive event target thread {input_target_thread_id} does not match event key target {:?}",
                event_key.target_thread_id
            )));
        }
        Ok(RoutedPassiveEvent {
            kind: event_key.passive_kind(),
            target_thread_id: event_key.target_thread_id.clone(),
            event_key,
            input,
        })
    }
}

pub struct WaitWakeKernel;

impl WaitWakeKernel {
    fn matching_passive_handle_ids(
        state: &ClosureState,
        routed: &RoutedPassiveEvent,
    ) -> Vec<String> {
        let handle_kind = routed.kind.handle_kind();
        state
            .handles
            .values()
            .filter(|handle| {
                handle.kind == handle_kind
                    && handle.status == IoHandleStatus::Pending
                    && passive_input_matches_handle(&routed.input, &routed.event_key, handle)
            })
            .map(|handle| handle.id.clone())
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnableThread {
    pub thread_id: String,
    pub root_problem_id: String,
    pub wake_reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WakeDecision {
    pub event_id: String,
    pub event_key: EventKey,
    pub status: PassiveEventStatus,
    pub matched_handle_ids: Vec<String>,
    pub runnable_threads: Vec<RunnableThread>,
    pub inbox_item_id: Option<String>,
    pub duplicate_of_event_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PassiveEventReceipt {
    pub event_id: String,
    pub status: PassiveEventStatus,
    pub matched_handle_ids: Vec<String>,
    pub inbox_item_id: Option<String>,
    pub duplicate_of_event_id: Option<String>,
    pub wake_decision: WakeDecision,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PassiveWait {
    pub thread_id: String,
    pub handle_id: String,
    pub wait_id: String,
    pub kind: PassiveEventKind,
    pub event_key: EventKey,
    pub status: WaitStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WaitSpec {
    pub kind: WaitSpecKind,
    #[serde(default)]
    pub event_key: Option<EventKey>,
    #[serde(default)]
    pub target_thread_id: Option<String>,
    #[serde(default)]
    pub condition: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub dedupe_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParkedWait {
    pub thread_id: String,
    pub wait_id: String,
    pub handle_ids: Vec<String>,
    pub mode: WaitMode,
    pub status: WaitStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WaitHandleSnapshot {
    pub handle: IoHandle,
    pub wait: Option<ThreadWait>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConsumedWait {
    pub thread_id: String,
    pub handle_id: String,
    pub wait_id: String,
    pub kind: IoHandleKind,
    pub target_thread_id: Option<String>,
    pub resolved_event_id: Option<String>,
    pub payload_ref: Option<String>,
    pub joined_thread: Option<JoinedThread>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CancelledWait {
    pub thread_id: String,
    pub handle_id: String,
    pub wait_id: String,
    pub kind: IoHandleKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WaitCommand {
    Park {
        mode: WaitMode,
        reason: String,
        specs: Vec<WaitSpec>,
    },
    Status {
        handle_id: String,
    },
    Consume {
        handle_id: String,
    },
    Cancel {
        handle_id: String,
        reason: String,
    },
    Wake {
        event: PassiveEventInput,
    },
}

impl WaitCommand {
    pub fn op_name(&self) -> &'static str {
        match self {
            Self::Park { .. } => "park",
            Self::Status { .. } => "status",
            Self::Consume { .. } => "consume",
            Self::Cancel { .. } => "cancel",
            Self::Wake { .. } => "wake",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum WaitResult {
    Parked { wait: ParkedWait },
    Status { snapshot: WaitHandleSnapshot },
    Consumed { consumed: ConsumedWait },
    Cancelled { cancelled: CancelledWait },
    Woke { receipt: PassiveEventReceipt },
}

pub struct QunuxRuntime {
    context: RuntimeContext,
    state: ClosureState,
}

impl QunuxRuntime {
    pub fn load_or_init_for_session(
        workspace_root: impl Into<PathBuf>,
        actor_session_id: impl Into<String>,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<Self> {
        let context = RuntimeContext::for_session(workspace_root, actor_session_id)?;
        Self::load_or_init(context, title, body)
    }

    pub fn load_or_init(
        context: RuntimeContext,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<Self> {
        let title = title.into();
        let body = body.into();
        let path = context.state_path();
        if path.exists() {
            let mut runtime = Self::load(context)?;
            runtime.upgrade_legacy_pristine_root_problem(&title, &body)?;
            return Ok(runtime);
        }

        let state = ClosureState::new_root(
            context.process_id.clone(),
            context.thread_id.clone(),
            context.actor_session_id.clone(),
            title,
            body,
        );
        let runtime = Self { context, state }.with_resolved_thread();
        runtime.save()?;
        Ok(runtime)
    }

    pub fn load(context: RuntimeContext) -> Result<Self> {
        let path = context.state_path();
        let raw = fs::read_to_string(&path).map_err(|source| QunuxError::Io {
            path: path.clone(),
            source,
        })?;
        let mut state: ClosureState =
            serde_json::from_str(&raw).map_err(|source| QunuxError::Json {
                path: path.clone(),
                source,
            })?;
        if state.schema_version < SCHEMA_VERSION {
            state.schema_version = SCHEMA_VERSION;
        }
        Ok(Self { context, state }.with_resolved_thread())
    }

    fn with_resolved_thread(mut self) -> Self {
        if let Some(actor_session_id) = self.context.actor_session_id.as_deref()
            && let Some((thread_id, _)) = self.state.threads.iter().find(|(_, thread)| {
                thread.actor_session_id.as_deref() == Some(actor_session_id)
                    || thread.codex_thread_id.as_deref() == Some(actor_session_id)
            })
        {
            self.context.thread_id = thread_id.clone();
            return self;
        }

        if !self.state.threads.contains_key(&self.context.thread_id) {
            self.context.thread_id = self.state.main_thread_id.clone();
        }

        let thread_id = self.context.thread_id.clone();
        if let Some(actor_session_id) = self.context.actor_session_id.clone()
            && let Some(thread) = self.state.threads.get_mut(&thread_id)
            && thread.actor_session_id.is_none()
        {
            thread.actor_session_id = Some(actor_session_id.clone());
            thread.codex_thread_id.get_or_insert(actor_session_id);
            thread.updated_at = Utc::now();
        }
        if self.state.process.root_actor_session_id.is_none() {
            self.state.process.root_actor_session_id = self.context.actor_session_id.clone();
            self.state.process.updated_at = Utc::now();
        }
        self
    }

    pub fn context(&self) -> &RuntimeContext {
        &self.context
    }

    pub fn state(&self) -> &ClosureState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut ClosureState {
        &mut self.state
    }

    pub fn save(&self) -> Result<()> {
        let path = self.context.state_path();
        let parent = path.parent().expect("state path has parent");
        fs::create_dir_all(parent).map_err(|source| QunuxError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        let body =
            serde_json::to_string_pretty(&self.state).map_err(|source| QunuxError::Json {
                path: path.clone(),
                source,
            })?;
        fs::write(&path, body).map_err(|source| QunuxError::Io { path, source })
    }

    pub fn current(&self) -> CurrentState {
        CurrentState {
            context: self.context.clone(),
            status: self.status(),
            next: self.next(),
        }
    }

    pub fn target_thread_id_for_passive_event_kind(&self, kind: PassiveEventKind) -> String {
        let current_thread_id = self.context.thread_id.clone();
        self.find_pending_passive_handle_thread(
            &current_thread_id,
            kind.handle_kind(),
            &mut Vec::new(),
        )
        .unwrap_or(current_thread_id)
    }

    pub fn thread_actor_route(&self, thread_id: impl AsRef<str>) -> Result<ThreadActorRoute> {
        let thread_id = thread_id.as_ref();
        let thread = self.require_thread(thread_id)?;
        Ok(ThreadActorRoute {
            thread_id: thread_id.to_string(),
            is_current_thread: thread_id == self.context.thread_id,
            actor_session_id: thread.actor_session_id.clone(),
            codex_thread_id: thread.codex_thread_id.clone(),
        })
    }

    pub fn wait(&mut self, command: WaitCommand) -> Result<WaitResult> {
        match command {
            WaitCommand::Park {
                mode,
                reason,
                specs,
            } => Ok(WaitResult::Parked {
                wait: self.park_wait(mode, reason, specs)?,
            }),
            WaitCommand::Status { handle_id } => Ok(WaitResult::Status {
                snapshot: self.wait_status(handle_id)?,
            }),
            WaitCommand::Consume { handle_id } => Ok(WaitResult::Consumed {
                consumed: self.consume_wait_handle(handle_id)?,
            }),
            WaitCommand::Cancel { handle_id, reason } => Ok(WaitResult::Cancelled {
                cancelled: self.cancel_wait_handle(handle_id, reason)?,
            }),
            WaitCommand::Wake { event } => Ok(WaitResult::Woke {
                receipt: self.receive_passive_event(event)?,
            }),
        }
    }

    pub fn wait_for_user_input(
        &mut self,
        condition: Option<String>,
        source: Option<String>,
        dedupe_key: Option<String>,
        reason: impl Into<String>,
    ) -> Result<PassiveWait> {
        let spec = WaitSpec {
            kind: WaitSpecKind::UserInput,
            event_key: None,
            target_thread_id: None,
            condition,
            source,
            dedupe_key,
        };
        self.park_single_passive_wait(reason, spec)
    }

    pub fn wait_for_timer(
        &mut self,
        condition: Option<String>,
        source: Option<String>,
        dedupe_key: Option<String>,
        reason: impl Into<String>,
    ) -> Result<PassiveWait> {
        let spec = WaitSpec {
            kind: WaitSpecKind::Timer,
            event_key: None,
            target_thread_id: None,
            condition,
            source,
            dedupe_key,
        };
        self.park_single_passive_wait(reason, spec)
    }

    pub fn wait_for_external_signal(
        &mut self,
        condition: Option<String>,
        source: Option<String>,
        dedupe_key: Option<String>,
        reason: impl Into<String>,
    ) -> Result<PassiveWait> {
        let spec = WaitSpec {
            kind: WaitSpecKind::ExternalSignal,
            event_key: None,
            target_thread_id: None,
            condition,
            source,
            dedupe_key,
        };
        self.park_single_passive_wait(reason, spec)
    }

    pub fn create_passive_wait(
        &mut self,
        kind: PassiveEventKind,
        condition: Option<String>,
        source: Option<String>,
        dedupe_key: Option<String>,
        reason: impl Into<String>,
    ) -> Result<PassiveWait> {
        let spec = WaitSpec {
            kind: match kind {
                PassiveEventKind::UserInput => WaitSpecKind::UserInput,
                PassiveEventKind::Timer => WaitSpecKind::Timer,
                PassiveEventKind::ExternalSignal => WaitSpecKind::ExternalSignal,
            },
            event_key: None,
            target_thread_id: None,
            condition,
            source,
            dedupe_key,
        };
        self.park_single_passive_wait(reason, spec)
    }

    pub fn wait_for_event_key(
        &mut self,
        event_key: EventKey,
        dedupe_key: Option<String>,
        reason: impl Into<String>,
    ) -> Result<PassiveWait> {
        self.create_passive_wait_for_key(event_key, dedupe_key, reason)
    }

    fn create_passive_wait_for_key(
        &mut self,
        event_key: EventKey,
        dedupe_key: Option<String>,
        reason: impl Into<String>,
    ) -> Result<PassiveWait> {
        let spec = WaitSpec {
            kind: WaitSpecKind::EventKey,
            event_key: Some(event_key),
            target_thread_id: None,
            condition: None,
            source: None,
            dedupe_key,
        };
        self.park_single_passive_wait(reason, spec)
    }

    fn park_single_passive_wait(
        &mut self,
        reason: impl Into<String>,
        spec: WaitSpec,
    ) -> Result<PassiveWait> {
        let parked = self.park_wait(WaitMode::All, reason.into(), vec![spec])?;
        let handle_id = parked.handle_ids.first().cloned().ok_or_else(|| {
            QunuxError::InvalidState("passive wait did not create a handle".to_string())
        })?;
        let handle = self.require_handle(&handle_id)?;
        let event_key = handle.event_key.clone().ok_or_else(|| {
            QunuxError::InvalidState(format!("handle {handle_id} has no event key"))
        })?;
        Ok(PassiveWait {
            thread_id: parked.thread_id,
            handle_id,
            wait_id: parked.wait_id,
            kind: event_key.passive_kind(),
            event_key,
            status: parked.status,
        })
    }

    fn park_wait(
        &mut self,
        mode: WaitMode,
        reason: String,
        specs: Vec<WaitSpec>,
    ) -> Result<ParkedWait> {
        if specs.is_empty() {
            return Err(QunuxError::InvalidState(
                "wait park requires at least one spec".to_string(),
            ));
        }
        let thread_id = self.context.thread_id.clone();
        let thread_status = self.current_thread()?.status;
        if matches!(
            thread_status,
            ThreadStatus::Done
                | ThreadStatus::Failed
                | ThreadStatus::Cancelled
                | ThreadStatus::Recovered
        ) {
            return Err(QunuxError::InvalidState(format!(
                "thread {thread_id} cannot create wait from terminal status {thread_status:?}"
            )));
        }
        if let Some(frontier) = self.blocking_frontier_for_wait() {
            return Err(QunuxError::InvalidState(format!(
                "thread {thread_id} cannot wait while runnable action {:?} exists for problem {:?} ticket {:?}",
                frontier.action, frontier.problem_id, frontier.ticket_id
            )));
        }

        let wait_id = self.next_wait_id();
        let now = Utc::now();
        let mut handle_ids = Vec::with_capacity(specs.len());
        for spec in specs {
            let dedupe_key = spec.dedupe_key.clone();
            let (kind, event_key) = self.event_key_for_wait_spec(&thread_id, &spec)?;
            let handle_id = self.next_handle_id();
            let handle = IoHandle {
                id: handle_id.clone(),
                kind: kind.handle_kind(),
                owner_thread_id: thread_id.clone(),
                target_thread_id: None,
                event_key: Some(event_key.clone()),
                condition: event_key.resource.clone(),
                source: event_key.source.clone(),
                payload_ref: None,
                dedupe_key,
                resolved_event_id: None,
                status: IoHandleStatus::Pending,
                created_at: now,
                updated_at: now,
            };
            self.state.handles.insert(handle_id.clone(), handle);
            handle_ids.push(handle_id);
        }

        let wait = ThreadWait {
            id: wait_id.clone(),
            thread_id: thread_id.clone(),
            handle_ids: handle_ids.clone(),
            mode,
            status: WaitStatus::Waiting,
            created_at: now,
            updated_at: now,
        };
        self.state.waits.insert(wait_id.clone(), wait);
        {
            let thread = self.thread_mut(&thread_id)?;
            thread.status = ThreadStatus::WaitingIo;
            thread.updated_at = now;
        }
        for handle_id in &handle_ids {
            self.io_event(
                IoEventKind::PassiveWaitCreated,
                Some(handle_id),
                Some(&thread_id),
                format!("passive wait created: {reason}"),
            );
            self.match_inboxed_event_for_handle(handle_id)?;
        }
        let wait_status = self
            .state
            .waits
            .get(&wait_id)
            .map(|wait| wait.status)
            .unwrap_or(WaitStatus::Waiting);
        self.save()?;
        Ok(ParkedWait {
            thread_id,
            wait_id,
            handle_ids,
            mode,
            status: wait_status,
        })
    }

    fn event_key_for_wait_spec(
        &self,
        current_thread_id: &str,
        spec: &WaitSpec,
    ) -> Result<(PassiveEventKind, EventKey)> {
        let event_key = match spec.kind {
            WaitSpecKind::EventKey => spec.event_key.clone().ok_or_else(|| {
                QunuxError::InvalidState("event_key wait spec requires event_key".to_string())
            })?,
            kind => {
                if spec.event_key.is_some() {
                    return Err(QunuxError::InvalidState(
                        "non-event_key wait specs must not provide event_key".to_string(),
                    ));
                }
                let passive_kind = kind.passive_kind().expect("passive wait spec kind");
                EventKey::passive(
                    passive_kind,
                    spec.condition.clone(),
                    spec.source.clone(),
                    Some(
                        spec.target_thread_id
                            .clone()
                            .unwrap_or_else(|| current_thread_id.to_string()),
                    ),
                )
            }
        };
        if let Some(target_thread_id) = event_key.target_thread_id.as_deref() {
            self.require_thread(target_thread_id)?;
            if target_thread_id != current_thread_id {
                return Err(QunuxError::InvalidState(format!(
                    "thread {current_thread_id} cannot park a wait targeted at thread {target_thread_id}"
                )));
            }
        }
        Ok((event_key.passive_kind(), event_key))
    }

    fn wait_status(&self, handle_id: impl AsRef<str>) -> Result<WaitHandleSnapshot> {
        let handle_id = handle_id.as_ref();
        let handle = self.require_handle(handle_id)?.clone();
        self.require_handle_owned_by_current_thread(&handle)?;
        let wait = self
            .wait_id_for_handle(handle_id)
            .and_then(|wait_id| self.state.waits.get(&wait_id).cloned());
        Ok(WaitHandleSnapshot { handle, wait })
    }

    fn consume_wait_handle(&mut self, handle_id: impl AsRef<str>) -> Result<ConsumedWait> {
        let handle_id = handle_id.as_ref();
        let handle = self.require_handle(handle_id)?.clone();
        self.require_handle_owned_by_current_thread(&handle)?;
        if handle.kind == IoHandleKind::ChildThread {
            let target_thread_id = handle.target_thread_id.clone().ok_or_else(|| {
                QunuxError::InvalidState(format!("child-thread handle {handle_id} has no target"))
            })?;
            let joined = self.join_child_thread_in_state(
                &target_thread_id,
                &self.context.thread_id.clone(),
                format!("consumed child-thread wait handle {handle_id}"),
            )?;
            self.save()?;
            return Ok(ConsumedWait {
                thread_id: self.context.thread_id.clone(),
                handle_id: joined.handle_id.clone(),
                wait_id: joined.wait_id.clone(),
                kind: IoHandleKind::ChildThread,
                target_thread_id: Some(target_thread_id),
                resolved_event_id: None,
                payload_ref: None,
                joined_thread: Some(joined),
            });
        }
        if handle.status != IoHandleStatus::Ready {
            return Err(QunuxError::InvalidState(format!(
                "cannot consume handle {handle_id} while status is {:?}",
                handle.status
            )));
        }
        let wait_id = self
            .wait_id_for_handle(handle_id)
            .ok_or_else(|| QunuxError::InvalidState(format!("handle {handle_id} has no wait")))?;
        let now = Utc::now();
        {
            let handle = self.handle_mut(handle_id)?;
            handle.status = IoHandleStatus::Consumed;
            handle.updated_at = now;
        }
        let wait = self.wait_mut(&wait_id)?.clone();
        if wait.mode == WaitMode::Any {
            for sibling_handle_id in wait.handle_ids.iter().filter(|id| id.as_str() != handle_id) {
                if let Some(sibling) = self.state.handles.get_mut(sibling_handle_id)
                    && matches!(
                        sibling.status,
                        IoHandleStatus::Pending | IoHandleStatus::Ready
                    )
                {
                    sibling.status = IoHandleStatus::Cancelled;
                    sibling.updated_at = now;
                }
            }
            let wait = self.wait_mut(&wait_id)?;
            wait.status = WaitStatus::Consumed;
            wait.updated_at = now;
        } else {
            self.refresh_wait_statuses_for_handle(handle_id);
            self.mark_wait_consumed_if_all_handles_consumed(&wait_id)?;
        }
        if let Some(thread) = self.state.threads.get_mut(&self.context.thread_id) {
            thread.status = ThreadStatus::Running;
            thread.updated_at = now;
        }
        self.io_event(
            IoEventKind::PassiveHandleConsumed,
            Some(handle_id),
            Some(&self.context.thread_id.clone()),
            format!("passive wait handle {handle_id} consumed"),
        );
        self.save()?;
        Ok(ConsumedWait {
            thread_id: self.context.thread_id.clone(),
            handle_id: handle_id.to_string(),
            wait_id,
            kind: handle.kind,
            target_thread_id: handle.target_thread_id,
            resolved_event_id: handle.resolved_event_id,
            payload_ref: handle.payload_ref,
            joined_thread: None,
        })
    }

    fn cancel_wait_handle(
        &mut self,
        handle_id: impl AsRef<str>,
        reason: impl Into<String>,
    ) -> Result<CancelledWait> {
        let handle_id = handle_id.as_ref();
        let handle = self.require_handle(handle_id)?.clone();
        self.require_handle_owned_by_current_thread(&handle)?;
        if handle.kind == IoHandleKind::ChildThread {
            return Err(QunuxError::InvalidState(format!(
                "cannot cancel child-thread handle {handle_id}; close or fail the child thread instead"
            )));
        }
        if matches!(
            handle.status,
            IoHandleStatus::Consumed | IoHandleStatus::Failed | IoHandleStatus::Cancelled
        ) {
            return Err(QunuxError::InvalidState(format!(
                "cannot cancel handle {handle_id} while status is {:?}",
                handle.status
            )));
        }
        let wait_id = self
            .wait_id_for_handle(handle_id)
            .ok_or_else(|| QunuxError::InvalidState(format!("handle {handle_id} has no wait")))?;
        let now = Utc::now();
        {
            let handle = self.handle_mut(handle_id)?;
            handle.status = IoHandleStatus::Cancelled;
            handle.updated_at = now;
        }
        self.refresh_wait_statuses_for_handle(handle_id);
        self.mark_ready_wait_threads_running(handle_id);
        self.io_event(
            IoEventKind::PassiveHandleCancelled,
            Some(handle_id),
            Some(&self.context.thread_id.clone()),
            format!(
                "passive wait handle {handle_id} cancelled: {}",
                reason.into()
            ),
        );
        self.save()?;
        Ok(CancelledWait {
            thread_id: self.context.thread_id.clone(),
            handle_id: handle_id.to_string(),
            wait_id,
            kind: handle.kind,
        })
    }

    pub fn receive_passive_event(
        &mut self,
        input: PassiveEventInput,
    ) -> Result<PassiveEventReceipt> {
        let routed = EventRoutingAgent::route_passive_event(input)?;
        let event_key = routed.event_key.clone();
        let target_thread_id = routed.target_thread_id.clone();
        if let Some(thread_id) = target_thread_id.as_deref() {
            self.require_thread(thread_id)?;
        }
        let kind = routed.kind;
        if let Some(dedupe_key) = routed.input.dedupe_key.as_deref()
            && let Some(existing) = self
                .state
                .passive_events
                .iter()
                .find(|event| event.dedupe_key.as_deref() == Some(dedupe_key))
        {
            let event_key = event_key_from_passive_event(existing);
            let inbox_item_id = self
                .state
                .inbox
                .iter()
                .find(|item| item.passive_event_id == existing.id)
                .map(|item| item.id.clone());
            let wake_decision = WakeDecision {
                event_id: existing.id.clone(),
                event_key,
                status: PassiveEventStatus::Duplicate,
                matched_handle_ids: existing.matched_handle_ids.clone(),
                runnable_threads: Vec::new(),
                inbox_item_id: inbox_item_id.clone(),
                duplicate_of_event_id: Some(existing.id.clone()),
            };
            return Ok(PassiveEventReceipt {
                event_id: existing.id.clone(),
                status: PassiveEventStatus::Duplicate,
                matched_handle_ids: existing.matched_handle_ids.clone(),
                inbox_item_id,
                duplicate_of_event_id: Some(existing.id.clone()),
                wake_decision,
            });
        }

        let event_id = self.next_passive_event_id();
        let now = Utc::now();
        let matched_handle_ids = WaitWakeKernel::matching_passive_handle_ids(&self.state, &routed);
        let status = if matched_handle_ids.is_empty() {
            PassiveEventStatus::Inboxed
        } else {
            PassiveEventStatus::Matched
        };

        self.state.passive_events.push(PassiveEvent {
            id: event_id.clone(),
            kind,
            event_key: Some(event_key.clone()),
            status,
            target_thread_id: target_thread_id.clone(),
            condition: routed.input.condition.clone(),
            source: routed.input.source.clone(),
            summary: routed.input.summary.clone(),
            payload_ref: routed.input.payload_ref.clone(),
            dedupe_key: routed.input.dedupe_key.clone(),
            matched_handle_ids: matched_handle_ids.clone(),
            created_at: now,
        });
        self.io_event(
            IoEventKind::PassiveEventReceived,
            None,
            target_thread_id.as_deref(),
            format!(
                "passive {:?} event received: {}",
                kind, routed.input.summary
            ),
        );

        let mut inbox_item_id = None;
        let mut runnable_threads = Vec::new();
        if matched_handle_ids.is_empty() {
            let id = self.next_inbox_item_id();
            self.state.inbox.push(InboxItem {
                id: id.clone(),
                passive_event_id: event_id.clone(),
                event_key: Some(event_key.clone()),
                target_thread_id: target_thread_id.clone(),
                condition: routed.input.condition.clone(),
                source: routed.input.source.clone(),
                summary: routed.input.summary.clone(),
                payload_ref: routed.input.payload_ref.clone(),
                dedupe_key: routed.input.dedupe_key.clone(),
                status,
                created_at: now,
            });
            self.io_event(
                IoEventKind::PassiveEventInboxed,
                None,
                target_thread_id.as_deref(),
                format!("passive event {event_id} had no matching pending handle"),
            );
            inbox_item_id = Some(id);
        } else {
            for handle_id in &matched_handle_ids {
                {
                    let handle = self.handle_mut(handle_id)?;
                    handle.status = IoHandleStatus::Ready;
                    handle.payload_ref = routed.input.payload_ref.clone();
                    handle.resolved_event_id = Some(event_id.clone());
                    handle.updated_at = now;
                }
                self.refresh_wait_statuses_for_handle(handle_id);
                runnable_threads.extend(self.mark_ready_wait_threads_running(handle_id));
                self.io_event(
                    IoEventKind::PassiveEventMatched,
                    Some(handle_id),
                    target_thread_id.as_deref(),
                    format!("passive event {event_id} matched handle {handle_id}"),
                );
                self.io_event(
                    IoEventKind::PassiveHandleReady,
                    Some(handle_id),
                    target_thread_id.as_deref(),
                    "passive handle is ready",
                );
            }
        }
        dedupe_runnable_threads(&mut runnable_threads);
        self.save()?;
        let wake_decision = WakeDecision {
            event_id: event_id.clone(),
            event_key,
            status,
            matched_handle_ids: matched_handle_ids.clone(),
            runnable_threads,
            inbox_item_id: inbox_item_id.clone(),
            duplicate_of_event_id: None,
        };
        Ok(PassiveEventReceipt {
            event_id,
            status,
            matched_handle_ids,
            inbox_item_id,
            duplicate_of_event_id: None,
            wake_decision,
        })
    }

    pub fn acknowledge_inbox_item(
        &mut self,
        inbox_item_id: impl AsRef<str>,
        note: impl Into<String>,
    ) -> Result<InboxItem> {
        let inbox_item_id = inbox_item_id.as_ref();
        let note = note.into();
        let item_index = self
            .state
            .inbox
            .iter()
            .position(|item| item.id == inbox_item_id)
            .ok_or_else(|| {
                QunuxError::InvalidState(format!("unknown inbox item {inbox_item_id}"))
            })?;
        let event_id = self.state.inbox[item_index].passive_event_id.clone();
        let target_thread_id = self.state.inbox[item_index].target_thread_id.clone();
        if let Some(target_thread_id) = target_thread_id.as_deref()
            && target_thread_id != self.context.thread_id
        {
            return Err(QunuxError::InvalidState(format!(
                "thread {} cannot acknowledge inbox item {inbox_item_id} targeted at {target_thread_id}",
                self.context.thread_id
            )));
        }
        if self.state.inbox[item_index].status != PassiveEventStatus::Inboxed {
            return Err(QunuxError::InvalidState(format!(
                "inbox item {inbox_item_id} is {:?}, not inboxed",
                self.state.inbox[item_index].status
            )));
        }
        self.state.inbox[item_index].status = PassiveEventStatus::Handled;
        if let Some(event) = self
            .state
            .passive_events
            .iter_mut()
            .find(|event| event.id == event_id)
            && event.status == PassiveEventStatus::Inboxed
        {
            event.status = PassiveEventStatus::Handled;
        }
        let item = self.state.inbox[item_index].clone();
        self.io_event(
            IoEventKind::PassiveEventHandled,
            None,
            target_thread_id.as_deref(),
            format!("inbox item {inbox_item_id} handled: {note}"),
        );
        self.save()?;
        Ok(item)
    }

    pub fn create_user_task_from_inbox(
        &mut self,
        inbox_item_id: impl AsRef<str>,
        title: impl Into<String>,
        body: impl Into<String>,
        note: impl Into<String>,
    ) -> Result<UserTaskProblem> {
        let inbox_item_id = inbox_item_id.as_ref();
        let title = title.into();
        let body = body.into();
        let note = note.into();
        let item_index = self
            .state
            .inbox
            .iter()
            .position(|item| item.id == inbox_item_id)
            .ok_or_else(|| {
                QunuxError::InvalidState(format!("unknown inbox item {inbox_item_id}"))
            })?;
        let item = self.state.inbox[item_index].clone();
        if let Some(target_thread_id) = item.target_thread_id.as_deref()
            && target_thread_id != self.context.thread_id
        {
            return Err(QunuxError::InvalidState(format!(
                "thread {} cannot create user task from inbox item {inbox_item_id} targeted at {target_thread_id}",
                self.context.thread_id
            )));
        }
        if item.status != PassiveEventStatus::Inboxed {
            return Err(QunuxError::InvalidState(format!(
                "inbox item {inbox_item_id} is {:?}, not inboxed",
                item.status
            )));
        }
        let event_index = self
            .state
            .passive_events
            .iter()
            .position(|event| event.id == item.passive_event_id)
            .ok_or_else(|| {
                QunuxError::InvalidState(format!(
                    "inbox item {inbox_item_id} points to unknown passive event {}",
                    item.passive_event_id
                ))
            })?;
        let event = self.state.passive_events[event_index].clone();
        if event.status != PassiveEventStatus::Inboxed {
            return Err(QunuxError::InvalidState(format!(
                "passive event {} is {:?}, not inboxed",
                event.id, event.status
            )));
        }
        if item.event_key != event.event_key
            || item.target_thread_id != event.target_thread_id
            || item.condition != event.condition
            || item.source != event.source
            || item.dedupe_key != event.dedupe_key
        {
            return Err(QunuxError::InvalidState(format!(
                "inbox item {inbox_item_id} does not match passive event {}",
                event.id
            )));
        }

        let parent_problem_id = self.current_thread()?.root_problem_id.clone();
        self.require_problem_writable(&parent_problem_id)?;
        let problem_id = self.next_problem_id();
        let now = Utc::now();
        let problem = Problem {
            id: problem_id.clone(),
            title,
            body,
            status: ProblemStatus::Todo,
            owner_thread_id: self.context.thread_id.clone(),
            parent_id: Some(parent_problem_id.clone()),
            created_from_ticket_id: None,
            created_from_ticket_mode: None,
            created_from_check_id: None,
            created_from_passive_event_id: Some(event.id.clone()),
            created_from_inbox_item_id: Some(item.id.clone()),
            created_from_user_task_kind: Some(event.kind),
            ticket_id: None,
            child_problem_ids: Vec::new(),
            followup_problem_ids: Vec::new(),
            result_ids: Vec::new(),
            check_ids: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        self.state.problems.insert(problem_id.clone(), problem);
        self.problem_mut(&parent_problem_id)?
            .child_problem_ids
            .push(problem_id.clone());
        self.touch_problem(&parent_problem_id)?;
        self.state.inbox[item_index].status = PassiveEventStatus::Handled;
        self.state.passive_events[event_index].status = PassiveEventStatus::Handled;
        self.io_event(
            IoEventKind::PassiveEventHandled,
            None,
            item.target_thread_id.as_deref(),
            format!("inbox item {inbox_item_id} converted to user task {problem_id}: {note}"),
        );
        self.event(
            "problem_created",
            &problem_id,
            format!(
                "created user task from inbox item {} passive event {}",
                item.id, event.id
            ),
        );
        self.save()?;
        Ok(UserTaskProblem {
            problem_id,
            parent_problem_id,
            inbox_item_id: item.id,
            passive_event_id: event.id,
            source_kind: event.kind,
        })
    }

    pub fn scaffold_user_task_from_inbox(
        &mut self,
        inbox_item_id: impl AsRef<str>,
        problem_title: impl Into<String>,
        problem_body: impl Into<String>,
        ticket_title: impl Into<String>,
        ticket_body: impl Into<String>,
        note: impl Into<String>,
    ) -> Result<ScaffoldedTask> {
        let user_task =
            self.create_user_task_from_inbox(inbox_item_id, problem_title, problem_body, note)?;
        let ticket_id = self.create_ticket(&user_task.problem_id, ticket_title, ticket_body)?;
        Ok(ScaffoldedTask {
            problem_id: user_task.problem_id,
            parent_problem_id: user_task.parent_problem_id,
            ticket_id,
            inbox_item_id: Some(user_task.inbox_item_id),
            passive_event_id: Some(user_task.passive_event_id),
            source_kind: Some(user_task.source_kind),
        })
    }

    pub fn initialize_root_problem(
        &mut self,
        title: Option<String>,
        body: Option<String>,
    ) -> Result<()> {
        if title.is_none() && body.is_none() {
            return Ok(());
        }
        let root_problem_id = self.state.root_problem_id.clone();
        let root = self.require_problem(&root_problem_id)?.clone();
        let root_is_pristine = root.status == ProblemStatus::Todo
            && root.ticket_id.is_none()
            && root.child_problem_ids.is_empty()
            && root.followup_problem_ids.is_empty()
            && root.result_ids.is_empty()
            && root.check_ids.is_empty();
        if !root_is_pristine {
            return Ok(());
        }
        {
            let root = self.problem_mut(&root_problem_id)?;
            if let Some(title) = title {
                root.title = title;
            }
            if let Some(body) = body {
                root.body = body;
            }
            root.updated_at = Utc::now();
        }
        self.event(
            "root_problem_initialized",
            root_problem_id,
            "root problem content initialized",
        );
        self.save()
    }

    fn upgrade_legacy_pristine_root_problem(
        &mut self,
        current_default_title: &str,
        current_default_body: &str,
    ) -> Result<()> {
        let root_problem_id = self.state.root_problem_id.clone();
        let root = self.require_problem(&root_problem_id)?.clone();
        let root_is_pristine = root.status == ProblemStatus::Todo
            && root.ticket_id.is_none()
            && root.child_problem_ids.is_empty()
            && root.followup_problem_ids.is_empty()
            && root.result_ids.is_empty()
            && root.check_ids.is_empty();
        if !root_is_pristine || root.title != LEGACY_DEFAULT_ROOT_PROBLEM_TITLE {
            return Ok(());
        }

        {
            let root = self.problem_mut(&root_problem_id)?;
            root.title = current_default_title.to_string();
            root.body = current_default_body.to_string();
            root.updated_at = Utc::now();
        }
        self.event(
            "root_problem_upgraded",
            root_problem_id,
            "legacy pristine root problem upgraded to process mission",
        );
        self.save()
    }

    pub fn create_problem_from_ticket(
        &mut self,
        parent_id: impl AsRef<str>,
        ticket_id: impl AsRef<str>,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<String> {
        self.create_problem_from_ticket_with_mode(
            parent_id,
            ticket_id,
            TicketChildMode::Split,
            title,
            body,
        )
    }

    pub fn create_problem_from_ticket_with_mode(
        &mut self,
        parent_id: impl AsRef<str>,
        ticket_id: impl AsRef<str>,
        mode: TicketChildMode,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<String> {
        let parent_id = parent_id.as_ref();
        let ticket_id = ticket_id.as_ref();
        self.require_problem_writable(parent_id)?;
        let ticket = self.require_ticket(ticket_id)?.clone();
        if ticket.problem_id != parent_id {
            return Err(QunuxError::InvalidState(format!(
                "ticket {ticket_id} does not belong to problem {parent_id}"
            )));
        }
        match mode {
            TicketChildMode::Split => {
                if ticket.classification != Some(TicketClassification::Split) {
                    return Err(QunuxError::InvalidState(format!(
                        "ticket {ticket_id} is not classified as split"
                    )));
                }
                if ticket.status != TicketStatus::Splitting {
                    return Err(QunuxError::InvalidState(format!(
                        "ticket {ticket_id} must be splitting before creating split child problems"
                    )));
                }
            }
            TicketChildMode::Spawn => {
                let parent_status = self.require_problem(parent_id)?.status;
                if !matches!(parent_status, ProblemStatus::Doing) {
                    return Err(QunuxError::InvalidState(format!(
                        "problem {parent_id} must be doing before spawning runtime child problems"
                    )));
                }
                if ticket.classification != Some(TicketClassification::OneGo) {
                    return Err(QunuxError::InvalidState(format!(
                        "ticket {ticket_id} is not classified as one_go"
                    )));
                }
                if ticket.status != TicketStatus::Executing {
                    return Err(QunuxError::InvalidState(format!(
                        "ticket {ticket_id} must be executing before spawning runtime child problems"
                    )));
                }
            }
        }

        let problem_id = self.next_problem_id();
        let now = Utc::now();
        let problem = Problem {
            id: problem_id.clone(),
            title: title.into(),
            body: body.into(),
            status: ProblemStatus::Todo,
            owner_thread_id: self.context.thread_id.clone(),
            parent_id: Some(parent_id.to_string()),
            created_from_ticket_id: Some(ticket_id.to_string()),
            created_from_ticket_mode: Some(mode),
            created_from_check_id: None,
            created_from_passive_event_id: None,
            created_from_inbox_item_id: None,
            created_from_user_task_kind: None,
            ticket_id: None,
            child_problem_ids: Vec::new(),
            followup_problem_ids: Vec::new(),
            result_ids: Vec::new(),
            check_ids: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        self.state.problems.insert(problem_id.clone(), problem);
        self.problem_mut(parent_id)?
            .child_problem_ids
            .push(problem_id.clone());
        self.touch_problem(parent_id)?;
        self.event(
            "problem_created",
            &problem_id,
            format!("created from {mode:?} ticket {ticket_id}"),
        );
        self.save()?;
        Ok(problem_id)
    }

    pub fn create_ticket(
        &mut self,
        problem_id: impl AsRef<str>,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<String> {
        let problem_id = problem_id.as_ref();
        let problem = self.require_problem_writable(problem_id)?;
        if problem.status == ProblemStatus::Done {
            return Err(QunuxError::InvalidState(format!(
                "cannot create ticket for done problem {problem_id}"
            )));
        }
        if let Some(existing) = &problem.ticket_id {
            return Err(QunuxError::InvalidState(format!(
                "problem {problem_id} already has ticket {existing}"
            )));
        }

        let ticket_id = self.next_ticket_id();
        let now = Utc::now();
        let ticket = Ticket {
            id: ticket_id.clone(),
            problem_id: problem_id.to_string(),
            title: title.into(),
            body: body.into(),
            status: TicketStatus::Defined,
            classification: None,
            classification_reason: None,
            result_id: None,
            created_at: now,
            updated_at: now,
        };
        self.state.tickets.insert(ticket_id.clone(), ticket);
        self.problem_mut(problem_id)?.ticket_id = Some(ticket_id.clone());
        self.touch_problem(problem_id)?;
        self.event("ticket_created", &ticket_id, "solution ticket defined");
        self.save()?;
        Ok(ticket_id)
    }

    pub fn classify_ticket(
        &mut self,
        ticket_id: impl AsRef<str>,
        classification: TicketClassification,
        reason: impl Into<String>,
    ) -> Result<()> {
        let ticket_id = ticket_id.as_ref();
        let problem_id = self.require_ticket(ticket_id)?.problem_id.clone();
        self.require_problem_writable(&problem_id)?;
        let ticket = self.ticket_mut(ticket_id)?;
        if ticket.status != TicketStatus::Defined {
            return Err(QunuxError::InvalidState(format!(
                "cannot classify ticket {ticket_id} while status is {:?}",
                ticket.status
            )));
        }
        ticket.classification = Some(classification);
        ticket.classification_reason = Some(reason.into());
        ticket.status = TicketStatus::Classified;
        ticket.updated_at = Utc::now();
        self.event(
            "ticket_classified",
            ticket_id,
            format!("{classification:?}"),
        );
        self.save()
    }

    pub fn set_status(
        &mut self,
        kind: EntityKind,
        id: impl AsRef<str>,
        status: impl AsRef<str>,
    ) -> Result<()> {
        let id = id.as_ref();
        let status = status.as_ref();
        match kind {
            EntityKind::Problem => {
                if status != "doing" {
                    return Err(QunuxError::InvalidState(
                        "public problem status change only allows doing".to_string(),
                    ));
                }
                let current_thread_id = self.context.thread_id.clone();
                let problem = self.problem_mut(id)?;
                if problem.owner_thread_id != current_thread_id {
                    return Err(QunuxError::InvalidState(format!(
                        "thread {} cannot mutate problem {id} owned by {}",
                        current_thread_id, problem.owner_thread_id
                    )));
                }
                if problem.status == ProblemStatus::Done {
                    return Err(QunuxError::InvalidState(format!(
                        "cannot reopen done problem {id}"
                    )));
                }
                problem.status = ProblemStatus::Doing;
                problem.updated_at = Utc::now();
                self.event("problem_status_changed", id, "doing");
            }
            EntityKind::Ticket => {
                let next_status = match status {
                    "executing" => TicketStatus::Executing,
                    "splitting" => TicketStatus::Splitting,
                    other => {
                        return Err(QunuxError::InvalidState(format!(
                            "public ticket status change does not allow {other}"
                        )));
                    }
                };
                let problem_id = self.require_ticket(id)?.problem_id.clone();
                self.require_problem_writable(&problem_id)?;
                self.transition_ticket_to_active_status(id, next_status)?;
            }
        }
        self.save()
    }

    pub fn record_result(
        &mut self,
        ticket_id: impl AsRef<str>,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<String> {
        let ticket_id = ticket_id.as_ref();
        let ticket = self.require_ticket(ticket_id)?.clone();
        let problem_id = ticket.problem_id.clone();
        self.require_problem_writable(&problem_id)?;
        let problem_status = self.require_problem(&problem_id)?.status;
        if problem_status == ProblemStatus::Todo {
            return Err(QunuxError::InvalidState(format!(
                "problem {problem_id} must be doing before recording a result"
            )));
        }
        if problem_status == ProblemStatus::Done {
            return Err(QunuxError::InvalidState(format!(
                "cannot record result for done problem {problem_id}"
            )));
        }
        if ticket.status == TicketStatus::Done {
            return Err(QunuxError::InvalidState(format!(
                "ticket {ticket_id} already has a result"
            )));
        }
        if ticket.status == TicketStatus::Classified {
            match ticket.classification {
                Some(TicketClassification::OneGo) => {
                    self.transition_ticket_to_active_status(ticket_id, TicketStatus::Executing)?;
                }
                Some(TicketClassification::Split) => {
                    self.transition_ticket_to_active_status(ticket_id, TicketStatus::Splitting)?;
                }
                None => {
                    return Err(QunuxError::InvalidState(format!(
                        "ticket {ticket_id} is classified without a classification"
                    )));
                }
            }
        }

        let active_status = self.require_ticket(ticket_id)?.status;
        if !matches!(
            active_status,
            TicketStatus::Executing | TicketStatus::Splitting
        ) {
            return Err(QunuxError::InvalidState(format!(
                "cannot record result for ticket {ticket_id} while status is {active_status:?}"
            )));
        }
        let children = self.child_problem_ids_from_ticket(ticket_id);
        if active_status == TicketStatus::Splitting {
            if children.is_empty() {
                return Err(QunuxError::InvalidState(format!(
                    "cannot finish split ticket {ticket_id}; create at least one child problem first"
                )));
            }
        }
        let open_children: Vec<_> = children
            .into_iter()
            .filter(|child_id| {
                self.state
                    .problems
                    .get(child_id)
                    .is_some_and(|problem| problem.status != ProblemStatus::Done)
            })
            .collect();
        if !open_children.is_empty() {
            return Err(QunuxError::InvalidState(format!(
                "cannot finish ticket {ticket_id}; child problems still open: {}",
                open_children.join(", ")
            )));
        }

        let result_id = self.next_result_id();
        let result = ExecutionResult {
            id: result_id.clone(),
            ticket_id: ticket_id.to_string(),
            problem_id: problem_id.clone(),
            title: title.into(),
            body: body.into(),
            created_at: Utc::now(),
        };
        self.state.results.insert(result_id.clone(), result);
        {
            let ticket = self.ticket_mut(ticket_id)?;
            ticket.result_id = Some(result_id.clone());
            ticket.status = TicketStatus::Done;
            ticket.updated_at = Utc::now();
        }
        self.problem_mut(&problem_id)?
            .result_ids
            .push(result_id.clone());
        self.touch_problem(&problem_id)?;
        self.event(
            "result_recorded",
            &result_id,
            format!("recorded for ticket {ticket_id}"),
        );
        self.save()?;
        Ok(result_id)
    }

    pub fn check(
        &mut self,
        problem_id: impl AsRef<str>,
        status: CheckStatus,
        result_ids: Vec<String>,
        title: impl Into<String>,
        body: impl Into<String>,
        followup: Option<(String, String)>,
    ) -> Result<String> {
        let problem_id = problem_id.as_ref();
        let problem = self.require_problem_writable(problem_id)?.clone();
        if problem.status == ProblemStatus::Done {
            return Err(QunuxError::InvalidState(format!(
                "problem {problem_id} is done and cannot be checked again"
            )));
        }
        if result_ids.is_empty() {
            return Err(QunuxError::InvalidState(
                "check requires at least one result id".to_string(),
            ));
        }
        for result_id in &result_ids {
            self.require_result(result_id)?;
        }

        let open_followups = self.open_followup_ids(problem_id);
        if status == CheckStatus::NotSuccess && !open_followups.is_empty() {
            return Err(QunuxError::InvalidState(format!(
                "cannot create another follow-up while open follow-ups remain: {}",
                open_followups.join(", ")
            )));
        }
        if status == CheckStatus::Success {
            self.require_problem_closable(problem_id)?;
            if followup.is_some() {
                return Err(QunuxError::InvalidState(
                    "success check cannot include a follow-up".to_string(),
                ));
            }
        } else if followup.is_none() {
            return Err(QunuxError::InvalidState(
                "not_success check requires a follow-up problem".to_string(),
            ));
        }

        let check_id = self.next_check_id();
        let mut followup_problem_id = None;
        if let Some((followup_title, followup_body)) = followup {
            let followup_id = self.next_problem_id();
            let now = Utc::now();
            let followup_problem = Problem {
                id: followup_id.clone(),
                title: followup_title,
                body: followup_body,
                status: ProblemStatus::Todo,
                owner_thread_id: self.context.thread_id.clone(),
                parent_id: Some(problem_id.to_string()),
                created_from_ticket_id: None,
                created_from_ticket_mode: None,
                created_from_check_id: Some(check_id.clone()),
                created_from_passive_event_id: None,
                created_from_inbox_item_id: None,
                created_from_user_task_kind: None,
                ticket_id: None,
                child_problem_ids: Vec::new(),
                followup_problem_ids: Vec::new(),
                result_ids: Vec::new(),
                check_ids: Vec::new(),
                created_at: now,
                updated_at: now,
            };
            self.state
                .problems
                .insert(followup_id.clone(), followup_problem);
            self.problem_mut(problem_id)?
                .followup_problem_ids
                .push(followup_id.clone());
            followup_problem_id = Some(followup_id);
        }

        let check = Check {
            id: check_id.clone(),
            problem_id: problem_id.to_string(),
            status,
            title: title.into(),
            body: body.into(),
            result_ids,
            followup_problem_id: followup_problem_id.clone(),
            created_at: Utc::now(),
        };
        self.state.checks.insert(check_id.clone(), check);
        {
            let problem = self.problem_mut(problem_id)?;
            problem.check_ids.push(check_id.clone());
            problem.status = match status {
                CheckStatus::Success => ProblemStatus::Done,
                CheckStatus::NotSuccess => ProblemStatus::Followup,
            };
            problem.updated_at = Utc::now();
        }
        if status == CheckStatus::Success {
            let current_root = self.current_thread()?.root_problem_id.clone();
            if current_root == problem_id {
                let current_thread_id = self.context.thread_id.clone();
                let has_parent = {
                    let thread = self.current_thread_mut()?;
                    thread.status = ThreadStatus::Done;
                    thread.updated_at = Utc::now();
                    thread.parent_thread_id.is_some()
                };
                if has_parent {
                    self.mark_child_thread_ready_in_state(
                        &current_thread_id,
                        "child Qunux thread root problem is done",
                    )?;
                }
            }
        }
        self.event("check_recorded", &check_id, format!("{status:?}"));
        self.save()?;
        Ok(check_id)
    }

    pub fn spawn_thread(
        &mut self,
        problem_id: impl AsRef<str>,
        policy: ContextForkPolicy,
        bootstrap_instruction: impl Into<String>,
        inherited_cwd: Option<String>,
        inherited_model: Option<String>,
        inherited_tools: Vec<String>,
    ) -> Result<SpawnedThread> {
        let problem_id = problem_id.as_ref();
        let problem = self.require_problem_writable(problem_id)?.clone();
        if problem.id == self.current_thread()?.root_problem_id {
            return Err(QunuxError::InvalidState(format!(
                "thread {} cannot spawn itself for root problem {problem_id}",
                self.context.thread_id
            )));
        }
        if self.thread_for_root_problem(problem_id).is_some() {
            return Err(QunuxError::InvalidState(format!(
                "problem {problem_id} already has a bound thread"
            )));
        }
        let parent_thread_id = self.context.thread_id.clone();
        let thread_id = self.next_thread_id();
        let handle_id = self.next_handle_id();
        let wait_id = self.next_wait_id();
        let bootstrap_instruction = bootstrap_instruction.into();
        let now = Utc::now();
        let context_fork = ContextFork {
            policy,
            parent_thread_id: Some(parent_thread_id.clone()),
            parent_actor_session_id: self.context.actor_session_id.clone(),
            fork_turn_id: None,
            bootstrap_instruction: bootstrap_instruction.clone(),
            inherited_cwd,
            inherited_model,
            inherited_tools,
        };
        let thread = Thread {
            id: thread_id.clone(),
            process_id: self.state.process_id.clone(),
            parent_thread_id: Some(parent_thread_id.clone()),
            root_problem_id: problem_id.to_string(),
            status: ThreadStatus::Running,
            context_fork,
            child_thread_ids: Vec::new(),
            created_from_problem_id: problem_id.to_string(),
            created_from_ticket_id: problem.created_from_ticket_id.clone(),
            created_from_check_id: problem.created_from_check_id.clone(),
            actor_session_id: None,
            codex_thread_id: None,
            joined_at: None,
            created_at: now,
            updated_at: now,
        };
        let handle = IoHandle {
            id: handle_id.clone(),
            kind: IoHandleKind::ChildThread,
            owner_thread_id: parent_thread_id.clone(),
            target_thread_id: Some(thread_id.clone()),
            event_key: None,
            condition: None,
            source: None,
            payload_ref: None,
            dedupe_key: None,
            resolved_event_id: None,
            status: IoHandleStatus::Pending,
            created_at: now,
            updated_at: now,
        };
        let wait = ThreadWait {
            id: wait_id.clone(),
            thread_id: parent_thread_id.clone(),
            handle_ids: vec![handle_id.clone()],
            mode: WaitMode::All,
            status: WaitStatus::Waiting,
            created_at: now,
            updated_at: now,
        };
        self.state.threads.insert(thread_id.clone(), thread);
        self.state.handles.insert(handle_id.clone(), handle);
        self.state.waits.insert(wait_id.clone(), wait);
        self.thread_mut(&parent_thread_id)?
            .child_thread_ids
            .push(thread_id.clone());
        self.thread_mut(&parent_thread_id)?.status = ThreadStatus::WaitingChildren;
        self.transfer_subtree_owner(problem_id, &thread_id)?;
        self.event(
            "thread_spawned",
            &thread_id,
            format!("bound to problem {problem_id}"),
        );
        self.io_event(
            IoEventKind::ChildThreadSpawned,
            Some(&handle_id),
            Some(&thread_id),
            format!("child thread {thread_id} spawned for problem {problem_id}"),
        );
        self.save()?;
        Ok(SpawnedThread {
            thread_id,
            root_problem_id: problem_id.to_string(),
            handle_id,
            wait_id,
            bootstrap_instruction,
        })
    }

    pub fn bind_thread_actor(
        &mut self,
        thread_id: impl AsRef<str>,
        actor_session_id: impl Into<String>,
    ) -> Result<()> {
        let thread_id = thread_id.as_ref();
        let actor_session_id = actor_session_id.into();
        validate_id(&actor_session_id)?;
        let thread = self.thread_mut(thread_id)?;
        thread.actor_session_id = Some(actor_session_id.clone());
        thread.codex_thread_id = Some(actor_session_id);
        thread.updated_at = Utc::now();
        self.event(
            "thread_actor_bound",
            thread_id,
            "Codex child agent session bound",
        );
        self.save()
    }

    pub fn join_thread(&mut self, thread_id: impl AsRef<str>) -> Result<JoinedThread> {
        let thread_id = thread_id.as_ref();
        let parent_thread_id = self.context.thread_id.clone();
        let joined = self.join_child_thread_in_state(
            thread_id,
            &parent_thread_id,
            format!("joined into parent {parent_thread_id}"),
        )?;
        self.save()?;
        Ok(joined)
    }

    pub fn recover_thread(&mut self, thread_id: impl AsRef<str>) -> Result<RecoveredThread> {
        let thread_id = thread_id.as_ref();
        let parent_thread_id = self.context.thread_id.clone();
        let child = self.require_thread(thread_id)?.clone();
        if child.parent_thread_id.as_deref() != Some(parent_thread_id.as_str()) {
            return Err(QunuxError::InvalidState(format!(
                "thread {parent_thread_id} cannot recover non-child thread {thread_id}"
            )));
        }
        if !matches!(child.status, ThreadStatus::Failed | ThreadStatus::Cancelled) {
            return Err(QunuxError::InvalidState(format!(
                "cannot recover thread {thread_id} while status is {:?}",
                child.status
            )));
        }
        if child.joined_at.is_some() {
            return Err(QunuxError::InvalidState(format!(
                "thread {thread_id} is already joined"
            )));
        }

        let handle_id = self.child_thread_handle_id(thread_id).ok_or_else(|| {
            QunuxError::InvalidState(format!("thread {thread_id} has no child-thread handle"))
        })?;
        let wait_id = self.wait_id_for_handle(&handle_id).ok_or_else(|| {
            QunuxError::InvalidState(format!("handle {handle_id} has no parent wait"))
        })?;
        self.require_handle_owned_by_current_thread(self.require_handle(&handle_id)?)?;

        let now = Utc::now();
        self.transfer_subtree_owner(&child.root_problem_id, &parent_thread_id)?;
        {
            let child_thread = self.thread_mut(thread_id)?;
            child_thread.status = ThreadStatus::Recovered;
            child_thread.updated_at = now;
        }
        {
            let handle = self.handle_mut(&handle_id)?;
            if !matches!(
                handle.status,
                IoHandleStatus::Failed | IoHandleStatus::Cancelled
            ) {
                return Err(QunuxError::InvalidState(format!(
                    "cannot recover thread {thread_id}; handle {handle_id} is {:?}",
                    handle.status
                )));
            }
            handle.status = IoHandleStatus::Cancelled;
            handle.updated_at = now;
        }
        {
            let wait = self.wait_mut(&wait_id)?;
            wait.status = WaitStatus::Consumed;
            wait.updated_at = now;
        }
        if let Some(parent) = self.state.threads.get_mut(&parent_thread_id) {
            parent.status = ThreadStatus::Running;
            parent.updated_at = now;
        }

        let message = format!(
            "recovered failed child thread {thread_id}; subtree {} returned to parent {parent_thread_id}",
            child.root_problem_id
        );
        self.event("thread_recovered", thread_id, message.clone());
        self.io_event(
            IoEventKind::ChildThreadRecovered,
            Some(&handle_id),
            Some(thread_id),
            message,
        );
        self.save()?;
        Ok(RecoveredThread {
            thread_id: thread_id.to_string(),
            root_problem_id: child.root_problem_id,
            parent_thread_id,
            handle_id,
            wait_id,
        })
    }

    pub fn auto_join_child_thread(
        &mut self,
        thread_id: impl AsRef<str>,
        message: impl Into<String>,
    ) -> Result<JoinedThread> {
        let thread_id = thread_id.as_ref();
        let parent_thread_id = self
            .require_thread(thread_id)?
            .parent_thread_id
            .clone()
            .ok_or_else(|| {
                QunuxError::InvalidState(format!("main thread {thread_id} cannot be auto-joined"))
            })?;
        let joined = self.join_child_thread_in_state(thread_id, &parent_thread_id, message)?;
        self.save()?;
        Ok(joined)
    }

    fn join_child_thread_in_state(
        &mut self,
        thread_id: &str,
        parent_thread_id: &str,
        message: impl Into<String>,
    ) -> Result<JoinedThread> {
        let child = self.require_thread(thread_id)?.clone();
        if child.parent_thread_id.as_deref() != Some(parent_thread_id) {
            return Err(QunuxError::InvalidState(format!(
                "thread {parent_thread_id} cannot join non-child thread {thread_id}"
            )));
        }
        if child.status != ThreadStatus::Done {
            return Err(QunuxError::InvalidState(format!(
                "cannot join thread {thread_id} while status is {:?}",
                child.status
            )));
        }
        if child.joined_at.is_some() {
            return Err(QunuxError::InvalidState(format!(
                "thread {thread_id} is already joined"
            )));
        }
        let handle_id = self.child_thread_handle_id(thread_id).ok_or_else(|| {
            QunuxError::InvalidState(format!("thread {thread_id} has no child-thread handle"))
        })?;
        let wait_id = self.wait_id_for_handle(&handle_id).ok_or_else(|| {
            QunuxError::InvalidState(format!("handle {handle_id} has no parent wait"))
        })?;
        if self.require_handle(&handle_id)?.status == IoHandleStatus::Pending {
            self.mark_child_thread_ready_in_state(thread_id, "joining done child thread")?;
        }
        if self.require_handle(&handle_id)?.status != IoHandleStatus::Ready {
            return Err(QunuxError::InvalidState(format!(
                "cannot join thread {thread_id}; handle {handle_id} is {:?}",
                self.require_handle(&handle_id)?.status
            )));
        }
        let now = Utc::now();
        let child_mut = self.thread_mut(thread_id)?;
        child_mut.joined_at = Some(now);
        child_mut.updated_at = now;
        {
            let handle = self.handle_mut(&handle_id)?;
            handle.status = IoHandleStatus::Consumed;
            handle.updated_at = now;
        }
        {
            let wait = self.wait_mut(&wait_id)?;
            wait.status = WaitStatus::Consumed;
            wait.updated_at = now;
        }
        if self.open_child_thread_ids(parent_thread_id).is_empty()
            && let Some(parent) = self.state.threads.get_mut(parent_thread_id)
        {
            parent.status = ThreadStatus::Running;
            parent.updated_at = now;
        }
        let message = message.into();
        self.event("thread_joined", thread_id, message.clone());
        self.io_event(
            IoEventKind::ChildThreadJoined,
            Some(&handle_id),
            Some(thread_id),
            message,
        );
        Ok(JoinedThread {
            thread_id: thread_id.to_string(),
            root_problem_id: child.root_problem_id,
            parent_thread_id: parent_thread_id.to_string(),
            handle_id,
            wait_id,
        })
    }

    pub fn list_threads(&self) -> Vec<Thread> {
        self.state.threads.values().cloned().collect()
    }

    pub fn thread_status(&self, thread_id: impl AsRef<str>) -> Result<Thread> {
        Ok(self.require_thread(thread_id.as_ref())?.clone())
    }

    pub fn list_handles(&self) -> Vec<IoHandle> {
        self.state.handles.values().cloned().collect()
    }

    pub fn mark_child_thread_ready(&mut self, thread_id: impl AsRef<str>) -> Result<String> {
        let handle_id = self.mark_child_thread_ready_in_state(
            thread_id.as_ref(),
            "child Qunux thread root problem is done",
        )?;
        self.save()?;
        Ok(handle_id)
    }

    pub fn mark_child_thread_failed(
        &mut self,
        thread_id: impl AsRef<str>,
        message: impl Into<String>,
    ) -> Result<String> {
        let (_handle_id, event_id) = self.mark_child_thread_failed_in_state(
            thread_id.as_ref(),
            IoEventKind::ChildThreadFailed,
            message,
        )?;
        self.save()?;
        Ok(event_id)
    }

    pub fn record_child_thread_spawn_failed(
        &mut self,
        thread_id: impl AsRef<str>,
        message: impl Into<String>,
    ) -> Result<String> {
        let (_handle_id, event_id) = self.mark_child_thread_failed_in_state(
            thread_id.as_ref(),
            IoEventKind::ChildThreadSpawnFailed,
            message,
        )?;
        self.save()?;
        Ok(event_id)
    }

    pub fn record_actor_completed_without_thread_done(
        &mut self,
        thread_id: impl AsRef<str>,
        status: impl Into<String>,
    ) -> Result<String> {
        let thread_id = thread_id.as_ref();
        let status = status.into();
        let thread = self.require_thread(thread_id)?;
        if thread.status == ThreadStatus::Done {
            return Err(QunuxError::InvalidState(format!(
                "thread {thread_id} is already done; mark the child-thread handle ready instead"
            )));
        }
        let (handle_id, _failed_event_id) = self.mark_child_thread_failed_in_state(
            thread_id,
            IoEventKind::ChildThreadFailed,
            format!("child actor completed before Qunux thread was done: {status}"),
        )?;
        let event_id = self.io_event(
            IoEventKind::ActorCompletedWithoutThreadDone,
            Some(&handle_id),
            Some(thread_id),
            format!("Codex child actor completed before Qunux thread was done: {status}"),
        );
        self.save()?;
        Ok(event_id)
    }

    pub fn next(&self) -> NextStep {
        if let Some(inbox_step) = self.next_pending_inbox() {
            return inbox_step;
        }
        if let Some(frontier_step) = self.runnable_frontier_for_current_thread() {
            return frontier_step;
        }
        if let Some(wait_step) = self.next_passive_io_wait() {
            return wait_step;
        }
        self.next_for_current_thread_root()
            .unwrap_or_else(|| NextStep {
                action: NextAction::None,
                disposition: NextDisposition::Terminal,
                thread_id: self.context.thread_id.clone(),
                target_thread_id: None,
                problem_id: None,
                ticket_id: None,
                instruction:
                    "No runnable Qunux action remains; validate, render, status, and summarize."
                        .to_string(),
                reason: "current thread subtree is closed or waiting".to_string(),
            })
    }

    fn next_for_current_thread_root(&self) -> Option<NextStep> {
        let root_problem_id = self
            .current_thread()
            .map(|thread| thread.root_problem_id.clone())
            .unwrap_or_else(|_| self.state.root_problem_id.clone());
        self.next_for_problem(&root_problem_id)
    }

    fn runnable_frontier_for_current_thread(&self) -> Option<NextStep> {
        self.next_for_current_thread_root()
            .filter(|step| !self.is_bare_lifecycle_root_create_ticket(step))
    }

    fn blocking_frontier_for_wait(&self) -> Option<NextStep> {
        self.runnable_frontier_for_current_thread()
            .filter(|step| step.action != NextAction::RecordResult)
    }

    fn is_bare_lifecycle_root_create_ticket(&self, step: &NextStep) -> bool {
        if step.action != NextAction::CreateSolutionTicket {
            return false;
        }
        let Ok(thread) = self.current_thread() else {
            return false;
        };
        step.problem_id.as_deref() == Some(self.state.root_problem_id.as_str())
            && thread.root_problem_id == self.state.root_problem_id
            && self
                .state
                .problems
                .get(&self.state.root_problem_id)
                .is_some_and(|problem| problem.ticket_id.is_none())
    }

    fn next_pending_inbox(&self) -> Option<NextStep> {
        let thread = self.current_thread().ok()?;
        let item = self.state.inbox.iter().find(|item| {
            item.status == PassiveEventStatus::Inboxed
                && item
                    .target_thread_id
                    .as_deref()
                    .is_none_or(|target| target == thread.id)
        })?;
        let event = self
            .state
            .passive_events
            .iter()
            .find(|event| event.id == item.passive_event_id);
        let event_summary = event
            .map(|event| event.summary.as_str())
            .unwrap_or(item.summary.as_str());
        Some(self.step(
            NextAction::HandleInbox,
            None,
            Some(&thread.root_problem_id),
            None,
            format!(
                "Handle pending inbox item {} from passive event {}: {event_summary}. First triage the input. If it is actionable work (solve, implement, investigate, run, fix, design, review, or other durable task), prefer `qunux.scaffold_user_task` with this inbox item id, a child problem title/body, a default ticket title/body, and a handling note; then follow the next Qunux frontier. Use `qunux.ingest_user_task` only when the ticket must be authored separately. If it is pure small talk, acknowledgement, narrow meta question, clarification, or an explicit idle/wait instruction, emit any needed visible assistant reply in this turn, then call `qunux.ack_inbox` for {} so the same input is not dispatched again. `qunux.ack_inbox` is state-only and not visible to the user; do not claim a visible reply in the ack note unless one was actually emitted. If the right next step is waiting, call `qunux.wait` only after the inbox item is incorporated. Do not close the lifecycle root merely because the process is ready or idle.",
                item.id, item.passive_event_id, item.id
            ),
            format!("pending inbox item {} can create a scheduler frontier", item.id),
        ))
    }

    fn next_passive_io_wait(&self) -> Option<NextStep> {
        let thread = self.current_thread().ok()?;
        if thread.status != ThreadStatus::WaitingIo {
            return None;
        }
        let wait = self.state.waits.values().find(|wait| {
            wait.thread_id == thread.id
                && wait.status == WaitStatus::Waiting
                && wait.handle_ids.iter().any(|handle_id| {
                    self.state.handles.get(handle_id).is_some_and(|handle| {
                        matches!(
                            handle.kind,
                            IoHandleKind::UserInput
                                | IoHandleKind::Timer
                                | IoHandleKind::ExternalSignal
                        ) && handle.status == IoHandleStatus::Pending
                    })
                })
        })?;
        let wait_reason = wait
            .handle_ids
            .iter()
            .filter_map(|handle_id| self.state.handles.get(handle_id))
            .find(|handle| handle.status == IoHandleStatus::Pending)
            .map(|handle| format!("{:?}", handle.kind))
            .unwrap_or_else(|| "passive IO".to_string());
        Some(self.step(
            NextAction::WaitIo,
            None,
            Some(&thread.root_problem_id),
            None,
            format!(
                "Thread is parked on {wait_reason}; do not build LLM context until a passive event wakes this wait."
            ),
            format!("thread {} has pending passive wait {}", thread.id, wait.id),
        ))
    }

    pub fn status(&self) -> RuntimeStatus {
        let thread = self.current_thread().ok();
        RuntimeStatus {
            process_id: self.context.process_id.clone(),
            thread_id: self.context.thread_id.clone(),
            thread_status: thread
                .as_ref()
                .map(|thread| thread.status)
                .unwrap_or(ThreadStatus::Failed),
            root_problem_id: self.state.root_problem_id.clone(),
            thread_root_problem_id: thread
                .as_ref()
                .map(|thread| thread.root_problem_id.clone())
                .unwrap_or_else(|| self.state.root_problem_id.clone()),
            total_threads: self.state.threads.len(),
            open_threads: self
                .state
                .threads
                .values()
                .filter(|thread| {
                    !matches!(
                        thread.status,
                        ThreadStatus::Done
                            | ThreadStatus::Failed
                            | ThreadStatus::Cancelled
                            | ThreadStatus::Recovered
                    )
                })
                .count(),
            total_problems: self.state.problems.len(),
            open_problems: self
                .state
                .problems
                .values()
                .filter(|problem| problem.status != ProblemStatus::Done)
                .count(),
            total_tickets: self.state.tickets.len(),
            total_results: self.state.results.len(),
            total_checks: self.state.checks.len(),
            total_handles: self.state.handles.len(),
            pending_handles: self
                .state
                .handles
                .values()
                .filter(|handle| handle.status == IoHandleStatus::Pending)
                .count(),
            ready_handles: self
                .state
                .handles
                .values()
                .filter(|handle| handle.status == IoHandleStatus::Ready)
                .count(),
            failed_threads: self
                .state
                .threads
                .values()
                .filter(|thread| thread.status == ThreadStatus::Failed)
                .count(),
            failed_handles: self
                .state
                .handles
                .values()
                .filter(|handle| handle.status == IoHandleStatus::Failed)
                .count(),
            waiting_threads: self
                .state
                .threads
                .values()
                .filter(|thread| {
                    matches!(
                        thread.status,
                        ThreadStatus::WaitingChildren | ThreadStatus::WaitingIo
                    )
                })
                .count(),
            passive_events: self.state.passive_events.len(),
            inbox_items: self.state.inbox.len(),
            pending_inbox_items: self
                .state
                .inbox
                .iter()
                .filter(|item| item.status == PassiveEventStatus::Inboxed)
                .count(),
            valid: self.validate().is_ok(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.state.schema_version != SCHEMA_VERSION {
            return Err(QunuxError::InvalidState(format!(
                "unsupported schema version {}",
                self.state.schema_version
            )));
        }
        if self.state.process.id != self.state.process_id {
            return Err(QunuxError::InvalidState(format!(
                "process metadata id {} does not match state process id {}",
                self.state.process.id, self.state.process_id
            )));
        }
        if self.state.process.main_thread_id != self.state.main_thread_id {
            return Err(QunuxError::InvalidState(format!(
                "process metadata main thread {} does not match state main thread {}",
                self.state.process.main_thread_id, self.state.main_thread_id
            )));
        }
        self.require_problem(&self.state.root_problem_id)?;
        self.require_thread(&self.state.main_thread_id)?;
        for (thread_id, thread) in &self.state.threads {
            self.require_problem(&thread.root_problem_id)?;
            if thread.process_id != self.state.process_id {
                return Err(QunuxError::InvalidState(format!(
                    "thread {thread_id} belongs to process {}, not {}",
                    thread.process_id, self.state.process_id
                )));
            }
            if let Some(parent_thread_id) = &thread.parent_thread_id {
                let parent = self.require_thread(parent_thread_id)?;
                if !parent.child_thread_ids.contains(thread_id) {
                    return Err(QunuxError::InvalidState(format!(
                        "thread {thread_id} points to parent {parent_thread_id}, but parent does not list it"
                    )));
                }
            }
        }
        for (handle_id, handle) in &self.state.handles {
            self.require_thread(&handle.owner_thread_id)?;
            match handle.kind {
                IoHandleKind::ChildThread => {
                    let target_thread_id = handle.target_thread_id.as_ref().ok_or_else(|| {
                        QunuxError::InvalidState(format!(
                            "child-thread handle {handle_id} has no target thread"
                        ))
                    })?;
                    let target_thread = self.require_thread(target_thread_id)?;
                    if target_thread.parent_thread_id.as_deref()
                        != Some(handle.owner_thread_id.as_str())
                    {
                        return Err(QunuxError::InvalidState(format!(
                            "child-thread handle {handle_id} targets {target_thread_id}, but its parent is not {}",
                            handle.owner_thread_id
                        )));
                    }
                    match handle.status {
                        IoHandleStatus::Failed => {
                            if target_thread.status != ThreadStatus::Failed {
                                return Err(QunuxError::InvalidState(format!(
                                    "failed handle {handle_id} targets thread {target_thread_id} with status {:?}",
                                    target_thread.status
                                )));
                            }
                        }
                        IoHandleStatus::Ready => {
                            if target_thread.status != ThreadStatus::Done {
                                return Err(QunuxError::InvalidState(format!(
                                    "ready handle {handle_id} targets thread {target_thread_id} with status {:?}",
                                    target_thread.status
                                )));
                            }
                        }
                        IoHandleStatus::Consumed => {
                            if target_thread.joined_at.is_none() {
                                return Err(QunuxError::InvalidState(format!(
                                    "consumed handle {handle_id} targets unjoined thread {target_thread_id}"
                                )));
                            }
                        }
                        IoHandleStatus::Pending | IoHandleStatus::Cancelled => {}
                    }
                }
                IoHandleKind::UserInput | IoHandleKind::Timer | IoHandleKind::ExternalSignal => {
                    if handle.target_thread_id.is_some() {
                        return Err(QunuxError::InvalidState(format!(
                            "passive handle {handle_id} must not target a child thread"
                        )));
                    }
                    if let Some(event_key) = handle.event_key.as_ref()
                        && let Some(target_thread_id) = event_key.target_thread_id.as_deref()
                    {
                        self.require_thread(target_thread_id)?;
                    }
                }
            }
        }
        for event in &self.state.passive_events {
            if let Some(event_key) = event.event_key.as_ref()
                && let Some(target_thread_id) = event_key.target_thread_id.as_deref()
            {
                self.require_thread(target_thread_id)?;
                if event.target_thread_id.as_deref() != Some(target_thread_id) {
                    return Err(QunuxError::InvalidState(format!(
                        "passive event {} target {:?} does not match event key target {target_thread_id}",
                        event.id, event.target_thread_id
                    )));
                }
            }
        }
        for item in &self.state.inbox {
            if let Some(event_key) = item.event_key.as_ref()
                && let Some(target_thread_id) = event_key.target_thread_id.as_deref()
            {
                self.require_thread(target_thread_id)?;
                if item.target_thread_id.as_deref() != Some(target_thread_id) {
                    return Err(QunuxError::InvalidState(format!(
                        "inbox item {} target {:?} does not match event key target {target_thread_id}",
                        item.id, item.target_thread_id
                    )));
                }
            }
        }
        for (thread_id, thread) in &self.state.threads {
            if thread.parent_thread_id.is_none() {
                continue;
            }
            let handle_count = self
                .state
                .handles
                .values()
                .filter(|handle| {
                    handle.kind == IoHandleKind::ChildThread
                        && handle.target_thread_id.as_deref() == Some(thread_id.as_str())
                })
                .count();
            if handle_count != 1 {
                return Err(QunuxError::InvalidState(format!(
                    "child thread {thread_id} must have exactly one child-thread handle, found {handle_count}"
                )));
            }
        }
        for (wait_id, wait) in &self.state.waits {
            self.require_thread(&wait.thread_id)?;
            if wait.handle_ids.is_empty() {
                return Err(QunuxError::InvalidState(format!(
                    "wait {wait_id} has no handles"
                )));
            }
            for handle_id in &wait.handle_ids {
                self.require_handle(handle_id)?;
            }
            if wait.status == WaitStatus::Ready && !self.wait_is_ready(wait) {
                return Err(QunuxError::InvalidState(format!(
                    "wait {wait_id} is ready but its handles are not ready"
                )));
            }
        }
        for (problem_id, problem) in &self.state.problems {
            self.require_thread(&problem.owner_thread_id)?;
            let owner = self.require_thread(&problem.owner_thread_id)?;
            if !self.is_descendant_or_self(problem_id, &owner.root_problem_id)? {
                return Err(QunuxError::InvalidState(format!(
                    "problem {problem_id} is owned by thread {}, but is not inside that thread subtree rooted at {}",
                    owner.id, owner.root_problem_id
                )));
            }
            if let Some(parent_id) = &problem.parent_id {
                self.require_problem(parent_id)?;
            }
            let has_user_task_provenance = problem.created_from_passive_event_id.is_some()
                || problem.created_from_inbox_item_id.is_some()
                || problem.created_from_user_task_kind.is_some();
            if let Some(ticket_id) = &problem.created_from_ticket_id {
                let ticket = self.require_ticket(ticket_id)?;
                if problem.created_from_check_id.is_some() {
                    return Err(QunuxError::InvalidState(format!(
                        "problem {problem_id} has both ticket and check provenance"
                    )));
                }
                if has_user_task_provenance {
                    return Err(QunuxError::InvalidState(format!(
                        "problem {problem_id} has both ticket and user-task provenance"
                    )));
                }
                if problem.parent_id.as_deref() != Some(ticket.problem_id.as_str()) {
                    return Err(QunuxError::InvalidState(format!(
                        "problem {problem_id} source ticket {ticket_id} belongs to {}, not parent {:?}",
                        ticket.problem_id, problem.parent_id
                    )));
                }
                match problem.created_from_ticket_mode {
                    Some(TicketChildMode::Split) | None => {
                        if ticket.classification != Some(TicketClassification::Split) {
                            return Err(QunuxError::InvalidState(format!(
                                "problem {problem_id} source ticket {ticket_id} is not split"
                            )));
                        }
                        if !matches!(ticket.status, TicketStatus::Splitting | TicketStatus::Done) {
                            return Err(QunuxError::InvalidState(format!(
                                "problem {problem_id} source split ticket {ticket_id} is not splitting or done"
                            )));
                        }
                    }
                    Some(TicketChildMode::Spawn) => {
                        if ticket.classification != Some(TicketClassification::OneGo) {
                            return Err(QunuxError::InvalidState(format!(
                                "problem {problem_id} source ticket {ticket_id} is not one_go"
                            )));
                        }
                        if !matches!(ticket.status, TicketStatus::Executing | TicketStatus::Done) {
                            return Err(QunuxError::InvalidState(format!(
                                "problem {problem_id} source spawn ticket {ticket_id} is not executing or done"
                            )));
                        }
                    }
                }
            } else if problem.parent_id.is_some()
                && problem.created_from_check_id.is_none()
                && !has_user_task_provenance
            {
                return Err(QunuxError::InvalidState(format!(
                    "problem {problem_id} has no ticket, check, or user-task provenance"
                )));
            }
            if let Some(check_id) = &problem.created_from_check_id {
                if has_user_task_provenance {
                    return Err(QunuxError::InvalidState(format!(
                        "problem {problem_id} has both check and user-task provenance"
                    )));
                }
                let check = self.require_check(check_id)?;
                if check.status != CheckStatus::NotSuccess {
                    return Err(QunuxError::InvalidState(format!(
                        "problem {problem_id} source check {check_id} is not not_success"
                    )));
                }
                if check.followup_problem_id.as_deref() != Some(problem_id.as_str()) {
                    return Err(QunuxError::InvalidState(format!(
                        "problem {problem_id} source check {check_id} does not point to it"
                    )));
                }
                if problem.parent_id.as_deref() != Some(check.problem_id.as_str()) {
                    return Err(QunuxError::InvalidState(format!(
                        "problem {problem_id} source check {check_id} belongs to {}, not parent {:?}",
                        check.problem_id, problem.parent_id
                    )));
                }
            }
            if has_user_task_provenance {
                self.validate_user_task_problem_provenance(problem_id, problem)?;
            }
            if let Some(ticket_id) = &problem.ticket_id {
                let ticket = self.require_ticket(ticket_id)?;
                if ticket.problem_id != *problem_id {
                    return Err(QunuxError::InvalidState(format!(
                        "{problem_id} points to ticket {ticket_id}, but the ticket belongs to {}",
                        ticket.problem_id
                    )));
                }
            }
            for child_id in &problem.child_problem_ids {
                let child = self.require_problem(child_id)?;
                if child.parent_id.as_deref() != Some(problem_id.as_str()) {
                    return Err(QunuxError::InvalidState(format!(
                        "child {child_id} does not point back to {problem_id}"
                    )));
                }
            }
            for followup_id in &problem.followup_problem_ids {
                let followup = self.require_problem(followup_id)?;
                if followup.parent_id.as_deref() != Some(problem_id.as_str()) {
                    return Err(QunuxError::InvalidState(format!(
                        "follow-up {followup_id} does not point back to {problem_id}"
                    )));
                }
            }
            for result_id in &problem.result_ids {
                let result = self.require_result(result_id)?;
                if result.problem_id != *problem_id {
                    return Err(QunuxError::InvalidState(format!(
                        "result {result_id} does not belong to {problem_id}"
                    )));
                }
            }
            for check_id in &problem.check_ids {
                let check = self.require_check(check_id)?;
                if check.problem_id != *problem_id {
                    return Err(QunuxError::InvalidState(format!(
                        "check {check_id} does not belong to {problem_id}"
                    )));
                }
                match check.status {
                    CheckStatus::Success => {
                        if check.followup_problem_id.is_some() {
                            return Err(QunuxError::InvalidState(format!(
                                "success check {check_id} has a follow-up problem"
                            )));
                        }
                    }
                    CheckStatus::NotSuccess => {
                        let followup_id = check.followup_problem_id.as_ref().ok_or_else(|| {
                            QunuxError::InvalidState(format!(
                                "not_success check {check_id} has no follow-up problem"
                            ))
                        })?;
                        let followup = self.require_problem(followup_id)?;
                        if followup.created_from_check_id.as_deref() != Some(check_id.as_str()) {
                            return Err(QunuxError::InvalidState(format!(
                                "check {check_id} points to follow-up {followup_id}, but the follow-up does not point back"
                            )));
                        }
                        if followup.parent_id.as_deref() != Some(problem_id.as_str()) {
                            return Err(QunuxError::InvalidState(format!(
                                "check {check_id} follow-up {followup_id} does not belong to parent {problem_id}"
                            )));
                        }
                        if !problem.followup_problem_ids.contains(followup_id) {
                            return Err(QunuxError::InvalidState(format!(
                                "check {check_id} follow-up {followup_id} is missing from parent {problem_id}"
                            )));
                        }
                    }
                }
            }
            if problem.status == ProblemStatus::Done {
                self.require_problem_closable(problem_id)?;
                let has_success = problem.check_ids.iter().any(|check_id| {
                    self.state
                        .checks
                        .get(check_id)
                        .is_some_and(|check| check.status == CheckStatus::Success)
                });
                if !has_success {
                    return Err(QunuxError::InvalidState(format!(
                        "done problem {problem_id} has no success check"
                    )));
                }
            }
        }
        for (ticket_id, ticket) in &self.state.tickets {
            self.require_problem(&ticket.problem_id)?;
            if matches!(
                ticket.status,
                TicketStatus::Classified
                    | TicketStatus::Executing
                    | TicketStatus::Splitting
                    | TicketStatus::Done
            ) && ticket.classification.is_none()
            {
                return Err(QunuxError::InvalidState(format!(
                    "ticket {ticket_id} has status {:?} without classification",
                    ticket.status
                )));
            }
            if ticket.status == TicketStatus::Executing
                && ticket.classification != Some(TicketClassification::OneGo)
            {
                return Err(QunuxError::InvalidState(format!(
                    "ticket {ticket_id} is executing without one_go classification"
                )));
            }
            if ticket.status == TicketStatus::Splitting
                && ticket.classification != Some(TicketClassification::Split)
            {
                return Err(QunuxError::InvalidState(format!(
                    "ticket {ticket_id} is splitting without split classification"
                )));
            }
            if ticket.status == TicketStatus::Done && ticket.result_id.is_none() {
                return Err(QunuxError::InvalidState(format!(
                    "done ticket {ticket_id} has no result"
                )));
            }
            if ticket.status == TicketStatus::Done {
                let open_children: Vec<_> = self
                    .child_problem_ids_from_ticket(ticket_id)
                    .into_iter()
                    .filter(|child_id| {
                        self.state
                            .problems
                            .get(child_id)
                            .is_some_and(|child| child.status != ProblemStatus::Done)
                    })
                    .collect();
                if !open_children.is_empty() {
                    return Err(QunuxError::InvalidState(format!(
                        "done ticket {ticket_id} has open child problems: {}",
                        open_children.join(", ")
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn render(&self) -> String {
        let mut output = String::new();
        self.render_problem(&self.state.root_problem_id, 0, &mut output);
        output
    }

    fn next_for_problem(&self, problem_id: &str) -> Option<NextStep> {
        let problem = self.state.problems.get(problem_id)?;
        if problem.owner_thread_id != self.context.thread_id {
            return None;
        }
        if problem.status == ProblemStatus::Done {
            return None;
        }

        for child_id in &problem.child_problem_ids {
            let Some(child) = self.state.problems.get(child_id) else {
                continue;
            };
            if let Some(child_thread) = self.thread_for_root_problem(child_id)
                && child_thread.id != self.context.thread_id
            {
                if child_thread.status == ThreadStatus::Done && child_thread.joined_at.is_some() {
                    continue;
                }
                if child_thread.status == ThreadStatus::Done {
                    return Some(self.step(
                        NextAction::JoinThread,
                        Some(&child_thread.id),
                        Some(child_id),
                        None,
                        "Join the completed child thread before summarizing the parent ticket.",
                        "child thread is done but not joined",
                    ));
                }
                if matches!(
                    child_thread.status,
                    ThreadStatus::Failed | ThreadStatus::Cancelled
                ) {
                    return Some(self.step(
                        NextAction::RecoverThread,
                        Some(&child_thread.id),
                        Some(child_id),
                        None,
                        "Recover the failed child thread before the parent ticket can be summarized.",
                        "child thread failed before closing its subtree",
                    ));
                }
                return Some(self.step(
                    NextAction::WaitThread,
                    Some(&child_thread.id),
                    Some(child_id),
                    None,
                    "Wait for the child thread to close its bound subtree, then join it.",
                    "child thread is still running",
                ));
            }
            if child.status == ProblemStatus::Done {
                continue;
            }
            if child.owner_thread_id == self.context.thread_id {
                return Some(self.step(
                    NextAction::SpawnThread,
                    None,
                    Some(child_id),
                    None,
                    "Spawn a child Qunux thread bound to this child problem subtree.",
                    "child problem is not assigned to a child thread",
                ));
            }
        }

        for followup_id in &problem.followup_problem_ids {
            let Some(followup) = self.state.problems.get(followup_id) else {
                continue;
            };
            if followup.owner_thread_id == self.context.thread_id {
                if followup.status == ProblemStatus::Done {
                    continue;
                }
                if let Some(next) = self.next_for_problem(followup_id) {
                    return Some(next);
                }
            } else if let Some(followup_thread) = self.thread_for_root_problem(followup_id) {
                if followup_thread.status == ThreadStatus::Done
                    && followup_thread.joined_at.is_some()
                {
                    continue;
                }
                if followup_thread.status == ThreadStatus::Done {
                    return Some(self.step(
                        NextAction::JoinThread,
                        Some(&followup_thread.id),
                        Some(followup_id),
                        None,
                        "Join the completed follow-up thread before re-checking the parent problem.",
                        "follow-up thread is done but not joined",
                    ));
                }
                if matches!(
                    followup_thread.status,
                    ThreadStatus::Failed | ThreadStatus::Cancelled
                ) {
                    return Some(self.step(
                        NextAction::RecoverThread,
                        Some(&followup_thread.id),
                        Some(followup_id),
                        None,
                        "Recover the failed follow-up thread before re-checking the parent problem.",
                        "follow-up thread failed before closing its subtree",
                    ));
                }
                return Some(self.step(
                    NextAction::WaitThread,
                    Some(&followup_thread.id),
                    Some(followup_id),
                    None,
                    "Wait for the follow-up thread to close its bound subtree, then join it.",
                    "follow-up thread is still running",
                ));
            }
        }

        let Some(ticket_id) = &problem.ticket_id else {
            return Some(self.step(
                NextAction::CreateSolutionTicket,
                None,
                Some(problem_id),
                None,
                "Create exactly one solution ticket for this problem.",
                "problem has no solution ticket",
            ));
        };
        let ticket = self.state.tickets.get(ticket_id)?;
        match ticket.status {
            TicketStatus::Created => Some(self.step(
                NextAction::DefineTicket,
                None,
                Some(problem_id),
                Some(ticket_id),
                "Complete the current ticket definition.",
                "ticket is created but not defined",
            )),
            TicketStatus::Defined => Some(self.step(
                NextAction::ClassifyTicket,
                None,
                Some(problem_id),
                Some(ticket_id),
                "Classify the ticket as one_go or split; prefer split unless the work is clearly bounded.",
                "ticket is defined but not classified",
            )),
            TicketStatus::Classified => match ticket.classification {
                Some(TicketClassification::OneGo) => Some(self.step(
                    NextAction::ExecuteTicket,
                    None,
                    Some(problem_id),
                    Some(ticket_id),
                    "Execute the ticket; record the actual result, or create a runtime-spawned child problem if execution discovers a blocking subprogram.",
                    "ticket is classified one_go",
                )),
                Some(TicketClassification::Split) => Some(self.step(
                    NextAction::SplitTicket,
                    None,
                    Some(problem_id),
                    Some(ticket_id),
                    "Move the ticket to splitting and create child problems.",
                    "ticket is classified split",
                )),
                None => Some(self.step(
                    NextAction::ClassifyTicket,
                    None,
                    Some(problem_id),
                    Some(ticket_id),
                    "Repair the missing ticket classification.",
                    "ticket classification is missing",
                )),
            },
            TicketStatus::Executing => Some(self.step(
                NextAction::RecordResult,
                None,
                    Some(problem_id),
                    Some(ticket_id),
                    "Record the result for the current executing ticket, unless it still needs a runtime-spawned child problem.",
                    "ticket is executing",
                )),
            TicketStatus::Splitting => {
                let children = self.child_problem_ids_from_ticket(ticket_id);
                if children.is_empty() {
                    return Some(self.step(
                        NextAction::SplitTicket,
                        None,
                        Some(problem_id),
                        Some(ticket_id),
                        "Create at least one child problem from this splitting ticket.",
                        "split ticket has no child problems",
                    ));
                }
                Some(self.step(
                    NextAction::RecordResult,
                    None,
                    Some(problem_id),
                    Some(ticket_id),
                    "Record the parent ticket summary result after all children created from this ticket are done.",
                    "ticket children are closed",
                ))
            }
            TicketStatus::Done => Some(self.step(
                NextAction::CheckSuccess,
                None,
                Some(problem_id),
                Some(ticket_id),
                "Run strict problem-level check_success using recorded result IDs.",
                "ticket is done and problem needs success check",
            )),
        }
    }

    fn step(
        &self,
        action: NextAction,
        target_thread_id: Option<&str>,
        problem_id: Option<&str>,
        ticket_id: Option<&str>,
        instruction: impl Into<String>,
        reason: impl Into<String>,
    ) -> NextStep {
        NextStep {
            action,
            disposition: NextDisposition::for_action(action),
            thread_id: self.context.thread_id.clone(),
            target_thread_id: target_thread_id.map(ToString::to_string),
            problem_id: problem_id.map(ToString::to_string),
            ticket_id: ticket_id.map(ToString::to_string),
            instruction: instruction.into(),
            reason: reason.into(),
        }
    }

    fn current_thread(&self) -> Result<&Thread> {
        self.require_thread(&self.context.thread_id)
    }

    fn current_thread_mut(&mut self) -> Result<&mut Thread> {
        let thread_id = self.context.thread_id.clone();
        self.thread_mut(&thread_id)
    }

    fn require_thread(&self, thread_id: &str) -> Result<&Thread> {
        self.state
            .threads
            .get(thread_id)
            .ok_or_else(|| QunuxError::InvalidState(format!("unknown thread {thread_id}")))
    }

    fn thread_mut(&mut self, thread_id: &str) -> Result<&mut Thread> {
        self.state
            .threads
            .get_mut(thread_id)
            .ok_or_else(|| QunuxError::InvalidState(format!("unknown thread {thread_id}")))
    }

    fn require_problem(&self, problem_id: &str) -> Result<&Problem> {
        self.state
            .problems
            .get(problem_id)
            .ok_or_else(|| QunuxError::InvalidState(format!("unknown problem {problem_id}")))
    }

    fn require_problem_writable(&self, problem_id: &str) -> Result<&Problem> {
        let problem = self.require_problem(problem_id)?;
        let thread = self.current_thread()?;
        if problem.owner_thread_id != self.context.thread_id {
            return Err(QunuxError::InvalidState(format!(
                "thread {} cannot mutate problem {problem_id} owned by {}",
                self.context.thread_id, problem.owner_thread_id
            )));
        }
        if !self.is_descendant_or_self(problem_id, &thread.root_problem_id)? {
            return Err(QunuxError::InvalidState(format!(
                "problem {problem_id} is not inside thread {} subtree rooted at {}",
                thread.id, thread.root_problem_id
            )));
        }
        Ok(problem)
    }

    fn problem_mut(&mut self, problem_id: &str) -> Result<&mut Problem> {
        self.state
            .problems
            .get_mut(problem_id)
            .ok_or_else(|| QunuxError::InvalidState(format!("unknown problem {problem_id}")))
    }

    fn require_ticket(&self, ticket_id: &str) -> Result<&Ticket> {
        self.state
            .tickets
            .get(ticket_id)
            .ok_or_else(|| QunuxError::InvalidState(format!("unknown ticket {ticket_id}")))
    }

    fn ticket_mut(&mut self, ticket_id: &str) -> Result<&mut Ticket> {
        self.state
            .tickets
            .get_mut(ticket_id)
            .ok_or_else(|| QunuxError::InvalidState(format!("unknown ticket {ticket_id}")))
    }

    fn require_result(&self, result_id: &str) -> Result<&ExecutionResult> {
        self.state
            .results
            .get(result_id)
            .ok_or_else(|| QunuxError::InvalidState(format!("unknown result {result_id}")))
    }

    fn require_check(&self, check_id: &str) -> Result<&Check> {
        self.state
            .checks
            .get(check_id)
            .ok_or_else(|| QunuxError::InvalidState(format!("unknown check {check_id}")))
    }

    fn validate_user_task_problem_provenance(
        &self,
        problem_id: &str,
        problem: &Problem,
    ) -> Result<()> {
        let passive_event_id = problem
            .created_from_passive_event_id
            .as_deref()
            .ok_or_else(|| {
                QunuxError::InvalidState(format!(
                    "problem {problem_id} has incomplete user-task provenance: missing passive event"
                ))
            })?;
        let inbox_item_id = problem
            .created_from_inbox_item_id
            .as_deref()
            .ok_or_else(|| {
                QunuxError::InvalidState(format!(
                    "problem {problem_id} has incomplete user-task provenance: missing inbox item"
                ))
            })?;
        let user_task_kind = problem.created_from_user_task_kind.ok_or_else(|| {
            QunuxError::InvalidState(format!(
                "problem {problem_id} has incomplete user-task provenance: missing source kind"
            ))
        })?;
        let event = self
            .state
            .passive_events
            .iter()
            .find(|event| event.id == passive_event_id)
            .ok_or_else(|| {
                QunuxError::InvalidState(format!(
                    "problem {problem_id} source passive event {passive_event_id} is missing"
                ))
            })?;
        let item = self
            .state
            .inbox
            .iter()
            .find(|item| item.id == inbox_item_id)
            .ok_or_else(|| {
                QunuxError::InvalidState(format!(
                    "problem {problem_id} source inbox item {inbox_item_id} is missing"
                ))
            })?;
        if item.passive_event_id != event.id {
            return Err(QunuxError::InvalidState(format!(
                "problem {problem_id} source inbox item {inbox_item_id} points to {}, not passive event {}",
                item.passive_event_id, event.id
            )));
        }
        if user_task_kind != event.kind {
            return Err(QunuxError::InvalidState(format!(
                "problem {problem_id} user-task kind {:?} does not match passive event kind {:?}",
                user_task_kind, event.kind
            )));
        }
        if item.event_key != event.event_key
            || item.target_thread_id != event.target_thread_id
            || item.condition != event.condition
            || item.source != event.source
            || item.dedupe_key != event.dedupe_key
        {
            return Err(QunuxError::InvalidState(format!(
                "problem {problem_id} source inbox item {inbox_item_id} does not match passive event {}",
                event.id
            )));
        }
        if item.status != PassiveEventStatus::Handled || event.status != PassiveEventStatus::Handled
        {
            return Err(QunuxError::InvalidState(format!(
                "problem {problem_id} user-task source must be handled, got inbox {:?} and event {:?}",
                item.status, event.status
            )));
        }
        if let Some(target_thread_id) = item.target_thread_id.as_deref()
            && target_thread_id != problem.owner_thread_id
        {
            return Err(QunuxError::InvalidState(format!(
                "problem {problem_id} source inbox item {inbox_item_id} targets {target_thread_id}, not owner {}",
                problem.owner_thread_id
            )));
        }
        Ok(())
    }

    fn require_handle(&self, handle_id: &str) -> Result<&IoHandle> {
        self.state
            .handles
            .get(handle_id)
            .ok_or_else(|| QunuxError::InvalidState(format!("unknown handle {handle_id}")))
    }

    fn require_handle_owned_by_current_thread(&self, handle: &IoHandle) -> Result<()> {
        if handle.owner_thread_id != self.context.thread_id {
            return Err(QunuxError::InvalidState(format!(
                "thread {} cannot access handle {} owned by {}",
                self.context.thread_id, handle.id, handle.owner_thread_id
            )));
        }
        Ok(())
    }

    fn handle_mut(&mut self, handle_id: &str) -> Result<&mut IoHandle> {
        self.state
            .handles
            .get_mut(handle_id)
            .ok_or_else(|| QunuxError::InvalidState(format!("unknown handle {handle_id}")))
    }

    fn wait_mut(&mut self, wait_id: &str) -> Result<&mut ThreadWait> {
        self.state
            .waits
            .get_mut(wait_id)
            .ok_or_else(|| QunuxError::InvalidState(format!("unknown wait {wait_id}")))
    }

    fn next_problem_id(&mut self) -> String {
        let id = format!("P{:03}", self.state.next_problem_seq);
        self.state.next_problem_seq += 1;
        id
    }

    fn next_thread_id(&mut self) -> String {
        let id = format!("QT{:03}", self.state.next_thread_seq);
        self.state.next_thread_seq += 1;
        id
    }

    fn next_handle_id(&mut self) -> String {
        let id = format!("H{:03}", self.state.next_handle_seq);
        self.state.next_handle_seq += 1;
        id
    }

    fn next_wait_id(&mut self) -> String {
        let id = format!("W{:03}", self.state.next_wait_seq);
        self.state.next_wait_seq += 1;
        id
    }

    fn next_io_event_id(&mut self) -> String {
        let id = format!("IO{:03}", self.state.next_io_event_seq);
        self.state.next_io_event_seq += 1;
        id
    }

    fn next_passive_event_id(&mut self) -> String {
        let id = format!("PE{:03}", self.state.next_passive_event_seq);
        self.state.next_passive_event_seq += 1;
        id
    }

    fn next_inbox_item_id(&mut self) -> String {
        let id = format!("IN{:03}", self.state.next_inbox_item_seq);
        self.state.next_inbox_item_seq += 1;
        id
    }

    fn next_ticket_id(&mut self) -> String {
        let id = format!("T{:03}", self.state.next_ticket_seq);
        self.state.next_ticket_seq += 1;
        id
    }

    fn next_result_id(&mut self) -> String {
        let id = format!("R{:03}", self.state.next_result_seq);
        self.state.next_result_seq += 1;
        id
    }

    fn next_check_id(&mut self) -> String {
        let id = format!("C{:03}", self.state.next_check_seq);
        self.state.next_check_seq += 1;
        id
    }

    fn transition_ticket_to_active_status(
        &mut self,
        ticket_id: &str,
        status: TicketStatus,
    ) -> Result<()> {
        let ticket = self.require_ticket(ticket_id)?;
        if ticket.status != TicketStatus::Classified {
            return Err(QunuxError::InvalidState(format!(
                "ticket {ticket_id} must be classified before entering {status:?}"
            )));
        }
        match (status, ticket.classification) {
            (TicketStatus::Executing, Some(TicketClassification::OneGo))
            | (TicketStatus::Splitting, Some(TicketClassification::Split)) => {}
            _ => {
                return Err(QunuxError::InvalidState(format!(
                    "ticket {ticket_id} classification {:?} cannot enter {status:?}",
                    ticket.classification
                )));
            }
        }
        let ticket = self.ticket_mut(ticket_id)?;
        ticket.status = status;
        ticket.updated_at = Utc::now();
        self.event("ticket_status_changed", ticket_id, format!("{status:?}"));
        Ok(())
    }

    fn transfer_subtree_owner(&mut self, problem_id: &str, thread_id: &str) -> Result<()> {
        self.require_thread(thread_id)?;
        let (children, followups) = {
            let problem = self.problem_mut(problem_id)?;
            problem.owner_thread_id = thread_id.to_string();
            problem.updated_at = Utc::now();
            (
                problem.child_problem_ids.clone(),
                problem.followup_problem_ids.clone(),
            )
        };
        for child_id in children.iter().chain(followups.iter()) {
            self.transfer_subtree_owner(child_id, thread_id)?;
        }
        Ok(())
    }

    fn require_problem_closable(&self, problem_id: &str) -> Result<()> {
        let problem = self.require_problem(problem_id)?;
        let ticket_id = problem.ticket_id.as_ref().ok_or_else(|| {
            QunuxError::InvalidState(format!("problem {problem_id} has no ticket"))
        })?;
        let ticket = self.require_ticket(ticket_id)?;
        if ticket.status != TicketStatus::Done {
            return Err(QunuxError::InvalidState(format!(
                "problem {problem_id} ticket {ticket_id} is not done"
            )));
        }
        let open_children: Vec<_> = problem
            .child_problem_ids
            .iter()
            .filter(|child_id| {
                self.state
                    .problems
                    .get(*child_id)
                    .is_some_and(|child| child.status != ProblemStatus::Done)
            })
            .cloned()
            .collect();
        if !open_children.is_empty() {
            return Err(QunuxError::InvalidState(format!(
                "child problems still open: {}",
                open_children.join(", ")
            )));
        }
        let open_followups = self.open_followup_ids(problem_id);
        if !open_followups.is_empty() {
            return Err(QunuxError::InvalidState(format!(
                "follow-up problems still open: {}",
                open_followups.join(", ")
            )));
        }
        Ok(())
    }

    fn child_problem_ids_from_ticket(&self, ticket_id: &str) -> Vec<String> {
        self.state
            .problems
            .values()
            .filter(|problem| problem.created_from_ticket_id.as_deref() == Some(ticket_id))
            .map(|problem| problem.id.clone())
            .collect()
    }

    fn is_descendant_or_self(&self, problem_id: &str, root_problem_id: &str) -> Result<bool> {
        let mut cursor = Some(problem_id.to_string());
        while let Some(current_id) = cursor {
            if current_id == root_problem_id {
                return Ok(true);
            }
            cursor = self.require_problem(&current_id)?.parent_id.clone();
        }
        Ok(false)
    }

    fn thread_for_root_problem(&self, problem_id: &str) -> Option<&Thread> {
        self.state.threads.values().find(|thread| {
            thread.root_problem_id == problem_id && thread.status != ThreadStatus::Recovered
        })
    }

    fn open_child_thread_ids(&self, thread_id: &str) -> Vec<String> {
        self.state
            .threads
            .get(thread_id)
            .into_iter()
            .flat_map(|thread| thread.child_thread_ids.iter())
            .filter(|child_thread_id| {
                self.state
                    .threads
                    .get(*child_thread_id)
                    .is_some_and(|child| {
                        !matches!(
                            child.status,
                            ThreadStatus::Done
                                | ThreadStatus::Failed
                                | ThreadStatus::Cancelled
                                | ThreadStatus::Recovered
                        )
                    })
            })
            .cloned()
            .collect()
    }

    fn child_thread_handle_id(&self, thread_id: &str) -> Option<String> {
        self.state
            .handles
            .values()
            .find(|handle| {
                handle.kind == IoHandleKind::ChildThread
                    && handle.target_thread_id.as_deref() == Some(thread_id)
            })
            .map(|handle| handle.id.clone())
    }

    fn find_pending_passive_handle_thread(
        &self,
        thread_id: &str,
        handle_kind: IoHandleKind,
        visited: &mut Vec<String>,
    ) -> Option<String> {
        if visited.iter().any(|visited_id| visited_id == thread_id) {
            return None;
        }
        visited.push(thread_id.to_string());

        if self.thread_has_pending_passive_handle(thread_id, handle_kind) {
            return Some(thread_id.to_string());
        }

        let thread = self.state.threads.get(thread_id)?;
        for child_thread_id in &thread.child_thread_ids {
            let Some(child_thread) = self.state.threads.get(child_thread_id) else {
                continue;
            };
            if matches!(
                child_thread.status,
                ThreadStatus::Done
                    | ThreadStatus::Failed
                    | ThreadStatus::Cancelled
                    | ThreadStatus::Recovered
            ) {
                continue;
            }
            if let Some(found) =
                self.find_pending_passive_handle_thread(child_thread_id, handle_kind, visited)
            {
                return Some(found);
            }
        }

        None
    }

    fn thread_has_pending_passive_handle(
        &self,
        thread_id: &str,
        handle_kind: IoHandleKind,
    ) -> bool {
        self.state.waits.values().any(|wait| {
            wait.thread_id == thread_id
                && wait.status == WaitStatus::Waiting
                && wait.handle_ids.iter().any(|handle_id| {
                    self.state.handles.get(handle_id).is_some_and(|handle| {
                        handle.kind == handle_kind && handle.status == IoHandleStatus::Pending
                    })
                })
        })
    }

    fn wait_id_for_handle(&self, handle_id: &str) -> Option<String> {
        self.state
            .waits
            .values()
            .find(|wait| wait.handle_ids.iter().any(|id| id == handle_id))
            .map(|wait| wait.id.clone())
    }

    fn wait_is_ready(&self, wait: &ThreadWait) -> bool {
        match wait.mode {
            WaitMode::All => wait.handle_ids.iter().all(|handle_id| {
                self.state.handles.get(handle_id).is_some_and(|handle| {
                    matches!(
                        handle.status,
                        IoHandleStatus::Ready
                            | IoHandleStatus::Consumed
                            | IoHandleStatus::Failed
                            | IoHandleStatus::Cancelled
                    )
                })
            }),
            WaitMode::Any => wait.handle_ids.iter().any(|handle_id| {
                self.state.handles.get(handle_id).is_some_and(|handle| {
                    matches!(
                        handle.status,
                        IoHandleStatus::Ready
                            | IoHandleStatus::Consumed
                            | IoHandleStatus::Failed
                            | IoHandleStatus::Cancelled
                    )
                })
            }),
        }
    }

    fn mark_wait_consumed_if_all_handles_consumed(&mut self, wait_id: &str) -> Result<()> {
        let Some(wait) = self.state.waits.get(wait_id).cloned() else {
            return Err(QunuxError::InvalidState(format!("unknown wait {wait_id}")));
        };
        if wait.handle_ids.iter().all(|handle_id| {
            self.state
                .handles
                .get(handle_id)
                .is_some_and(|handle| handle.status == IoHandleStatus::Consumed)
        }) {
            let wait = self.wait_mut(wait_id)?;
            wait.status = WaitStatus::Consumed;
            wait.updated_at = Utc::now();
        }
        Ok(())
    }

    fn refresh_wait_statuses_for_handle(&mut self, handle_id: &str) {
        let wait_ids: Vec<_> = self
            .state
            .waits
            .values()
            .filter(|wait| wait.handle_ids.iter().any(|id| id == handle_id))
            .map(|wait| wait.id.clone())
            .collect();
        for wait_id in wait_ids {
            let Some(wait) = self.state.waits.get(&wait_id).cloned() else {
                continue;
            };
            if wait.status == WaitStatus::Consumed {
                continue;
            }
            let next_status = if self.wait_is_ready(&wait) {
                WaitStatus::Ready
            } else {
                WaitStatus::Waiting
            };
            if let Some(wait) = self.state.waits.get_mut(&wait_id) {
                wait.status = next_status;
                wait.updated_at = Utc::now();
            }
        }
    }

    fn mark_ready_wait_threads_running(&mut self, handle_id: &str) -> Vec<RunnableThread> {
        let wait_thread_ids: Vec<_> = self
            .state
            .waits
            .values()
            .filter(|wait| {
                wait.status == WaitStatus::Ready && wait.handle_ids.iter().any(|id| id == handle_id)
            })
            .map(|wait| wait.thread_id.clone())
            .collect();
        let mut runnable_threads = Vec::new();
        for thread_id in wait_thread_ids {
            if let Some(thread) = self.state.threads.get_mut(&thread_id)
                && thread.status == ThreadStatus::WaitingIo
            {
                thread.status = ThreadStatus::Running;
                thread.updated_at = Utc::now();
                runnable_threads.push(RunnableThread {
                    thread_id: thread.id.clone(),
                    root_problem_id: thread.root_problem_id.clone(),
                    wake_reason: format!("handle {handle_id} ready"),
                });
            }
        }
        runnable_threads
    }

    fn match_inboxed_event_for_handle(&mut self, handle_id: &str) -> Result<()> {
        let handle = self.require_handle(handle_id)?.clone();
        let Some((event_id, payload_ref)) = self
            .state
            .passive_events
            .iter()
            .find(|event| {
                event.status == PassiveEventStatus::Inboxed
                    && passive_event_matches_handle(event, &handle)
            })
            .map(|event| (event.id.clone(), event.payload_ref.clone()))
        else {
            return Ok(());
        };
        let now = Utc::now();
        {
            let handle = self.handle_mut(handle_id)?;
            handle.status = IoHandleStatus::Ready;
            handle.resolved_event_id = Some(event_id.clone());
            handle.payload_ref = payload_ref;
            handle.updated_at = now;
        }
        if let Some(event) = self
            .state
            .passive_events
            .iter_mut()
            .find(|event| event.id == event_id)
        {
            event.status = PassiveEventStatus::Matched;
            if !event.matched_handle_ids.iter().any(|id| id == handle_id) {
                event.matched_handle_ids.push(handle_id.to_string());
            }
        }
        for item in self
            .state
            .inbox
            .iter_mut()
            .filter(|item| item.passive_event_id == event_id)
        {
            item.status = PassiveEventStatus::Matched;
        }
        self.refresh_wait_statuses_for_handle(handle_id);
        self.mark_ready_wait_threads_running(handle_id);
        self.io_event(
            IoEventKind::PassiveEventMatched,
            Some(handle_id),
            Some(&handle.owner_thread_id),
            format!("inboxed passive event {event_id} matched newly created handle {handle_id}"),
        );
        self.io_event(
            IoEventKind::PassiveHandleReady,
            Some(handle_id),
            Some(&handle.owner_thread_id),
            "passive handle is ready from inboxed event",
        );
        Ok(())
    }

    fn mark_child_thread_ready_in_state(
        &mut self,
        thread_id: &str,
        message: impl Into<String>,
    ) -> Result<String> {
        let thread = self.require_thread(thread_id)?;
        if thread.status != ThreadStatus::Done {
            return Err(QunuxError::InvalidState(format!(
                "cannot mark child thread {thread_id} ready while status is {:?}",
                thread.status
            )));
        }
        let handle_id = self.child_thread_handle_id(thread_id).ok_or_else(|| {
            QunuxError::InvalidState(format!("thread {thread_id} has no child-thread handle"))
        })?;
        let now = Utc::now();
        {
            let handle = self.handle_mut(&handle_id)?;
            match handle.status {
                IoHandleStatus::Pending | IoHandleStatus::Ready => {
                    handle.status = IoHandleStatus::Ready;
                    handle.updated_at = now;
                }
                IoHandleStatus::Consumed => {
                    return Ok(handle_id);
                }
                IoHandleStatus::Failed | IoHandleStatus::Cancelled => {
                    return Err(QunuxError::InvalidState(format!(
                        "cannot mark handle {handle_id} ready from status {:?}",
                        handle.status
                    )));
                }
            }
        }
        self.refresh_wait_statuses_for_handle(&handle_id);
        self.io_event(
            IoEventKind::ChildThreadDone,
            Some(&handle_id),
            Some(thread_id),
            message,
        );
        self.io_event(
            IoEventKind::HandleReady,
            Some(&handle_id),
            Some(thread_id),
            "child-thread handle is ready",
        );
        Ok(handle_id)
    }

    fn mark_child_thread_failed_in_state(
        &mut self,
        thread_id: &str,
        kind: IoEventKind,
        message: impl Into<String>,
    ) -> Result<(String, String)> {
        let thread = self.require_thread(thread_id)?.clone();
        if thread.parent_thread_id.is_none() {
            return Err(QunuxError::InvalidState(format!(
                "main thread {thread_id} cannot be marked as failed child thread"
            )));
        }
        if thread.status == ThreadStatus::Done {
            return Err(QunuxError::InvalidState(format!(
                "thread {thread_id} is already done; it must be joined rather than failed"
            )));
        }
        if thread.joined_at.is_some() {
            return Err(QunuxError::InvalidState(format!(
                "thread {thread_id} is already joined"
            )));
        }
        let handle_id = self.child_thread_handle_id(thread_id).ok_or_else(|| {
            QunuxError::InvalidState(format!("thread {thread_id} has no child-thread handle"))
        })?;
        let message = message.into();
        let now = Utc::now();
        {
            let thread = self.thread_mut(thread_id)?;
            thread.status = ThreadStatus::Failed;
            thread.updated_at = now;
        }
        {
            let handle = self.handle_mut(&handle_id)?;
            match handle.status {
                IoHandleStatus::Pending | IoHandleStatus::Ready | IoHandleStatus::Failed => {
                    handle.status = IoHandleStatus::Failed;
                    handle.updated_at = now;
                }
                IoHandleStatus::Consumed => {
                    return Err(QunuxError::InvalidState(format!(
                        "cannot fail child thread {thread_id}; handle {handle_id} is consumed"
                    )));
                }
                IoHandleStatus::Cancelled => {
                    return Err(QunuxError::InvalidState(format!(
                        "cannot fail child thread {thread_id}; handle {handle_id} is cancelled"
                    )));
                }
            }
        }
        self.refresh_wait_statuses_for_handle(&handle_id);
        if let Some(parent_thread_id) = thread.parent_thread_id.as_deref()
            && self.open_child_thread_ids(parent_thread_id).is_empty()
            && let Some(parent) = self.state.threads.get_mut(parent_thread_id)
        {
            parent.status = ThreadStatus::Running;
            parent.updated_at = now;
        }
        self.event("thread_failed", thread_id, message.clone());
        let event_id = self.io_event(kind, Some(&handle_id), Some(thread_id), message);
        Ok((handle_id, event_id))
    }

    fn open_followup_ids(&self, problem_id: &str) -> Vec<String> {
        self.state
            .problems
            .get(problem_id)
            .into_iter()
            .flat_map(|problem| problem.followup_problem_ids.iter())
            .filter(|followup_id| {
                self.state
                    .problems
                    .get(*followup_id)
                    .is_some_and(|followup| followup.status != ProblemStatus::Done)
            })
            .cloned()
            .collect()
    }

    fn touch_problem(&mut self, problem_id: &str) -> Result<()> {
        self.problem_mut(problem_id)?.updated_at = Utc::now();
        Ok(())
    }

    fn event(
        &mut self,
        kind: impl Into<String>,
        entity_id: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.state.events.push(Event {
            kind: kind.into(),
            entity_id: entity_id.into(),
            message: message.into(),
            actor_thread_id: self.context.thread_id.clone(),
            actor_session_id: self.context.actor_session_id.clone(),
            created_at: Utc::now(),
        });
    }

    fn io_event(
        &mut self,
        kind: IoEventKind,
        handle_id: Option<&str>,
        thread_id: Option<&str>,
        message: impl Into<String>,
    ) -> String {
        let event_id = self.next_io_event_id();
        self.state.io_events.push(IoEvent {
            id: event_id.clone(),
            kind,
            handle_id: handle_id.map(ToString::to_string),
            thread_id: thread_id.map(ToString::to_string),
            message: message.into(),
            actor_thread_id: self.context.thread_id.clone(),
            actor_session_id: self.context.actor_session_id.clone(),
            created_at: Utc::now(),
        });
        event_id
    }

    fn render_problem(&self, problem_id: &str, depth: usize, output: &mut String) {
        let Some(problem) = self.state.problems.get(problem_id) else {
            return;
        };
        let indent = "  ".repeat(depth);
        output.push_str(&format!(
            "{indent}- {} [{} owner={}] {}\n",
            problem.id,
            status_name(problem.status),
            problem.owner_thread_id,
            problem.title
        ));
        if let Some(ticket_id) = &problem.ticket_id
            && let Some(ticket) = self.state.tickets.get(ticket_id)
        {
            output.push_str(&format!(
                "{indent}  ticket {} [{}] {:?}\n",
                ticket.id,
                ticket_status_name(ticket.status),
                ticket.classification
            ));
        }
        for child_id in problem
            .child_problem_ids
            .iter()
            .chain(problem.followup_problem_ids.iter())
        {
            self.render_problem(child_id, depth + 1, output);
        }
    }
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id == "."
        || id == ".."
        || id.contains('/')
        || id.contains('\\')
        || !id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.')
    {
        return Err(QunuxError::InvalidId(id.to_string()));
    }
    Ok(())
}

pub fn process_id_for_session_id(session_id: &str) -> Result<String> {
    validate_id(session_id)?;
    let process_id = format!("QP-{session_id}");
    validate_id(&process_id)?;
    Ok(process_id)
}

fn status_name(status: ProblemStatus) -> &'static str {
    match status {
        ProblemStatus::Todo => "todo",
        ProblemStatus::Doing => "doing",
        ProblemStatus::Checking => "checking",
        ProblemStatus::Followup => "followup",
        ProblemStatus::Done => "done",
    }
}

fn ticket_status_name(status: TicketStatus) -> &'static str {
    match status {
        TicketStatus::Created => "created",
        TicketStatus::Defined => "defined",
        TicketStatus::Classified => "classified",
        TicketStatus::Executing => "executing",
        TicketStatus::Splitting => "splitting",
        TicketStatus::Done => "done",
    }
}

fn event_key_from_passive_event(event: &PassiveEvent) -> EventKey {
    event.event_key.clone().unwrap_or_else(|| {
        EventKey::passive(
            event.kind,
            event.condition.clone(),
            event.source.clone(),
            event.target_thread_id.clone(),
        )
    })
}

fn dedupe_runnable_threads(runnable_threads: &mut Vec<RunnableThread>) {
    runnable_threads.sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
    runnable_threads.dedup_by(|left, right| left.thread_id == right.thread_id);
}

fn passive_input_matches_handle(
    input: &PassiveEventInput,
    event_key: &EventKey,
    handle: &IoHandle,
) -> bool {
    if let Some(handle_key) = handle.event_key.as_ref() {
        if handle_key != event_key {
            return false;
        }
        if let Some(dedupe_key) = handle.dedupe_key.as_deref()
            && input.dedupe_key.as_deref() != Some(dedupe_key)
        {
            return false;
        }
        return true;
    }
    if let Some(target_thread_id) = input.target_thread_id.as_deref()
        && handle.owner_thread_id != target_thread_id
    {
        return false;
    }
    if let Some(condition) = handle.condition.as_deref()
        && input.condition.as_deref() != Some(condition)
    {
        return false;
    }
    if let Some(source) = handle.source.as_deref()
        && input.source.as_deref() != Some(source)
    {
        return false;
    }
    if let Some(dedupe_key) = handle.dedupe_key.as_deref()
        && input.dedupe_key.as_deref() != Some(dedupe_key)
    {
        return false;
    }
    true
}

fn passive_event_matches_handle(event: &PassiveEvent, handle: &IoHandle) -> bool {
    if handle.kind != event.kind.handle_kind() || handle.status != IoHandleStatus::Pending {
        return false;
    }
    if let (Some(event_key), Some(handle_key)) =
        (event.event_key.as_ref(), handle.event_key.as_ref())
    {
        if event_key != handle_key {
            return false;
        }
        if let Some(dedupe_key) = handle.dedupe_key.as_deref()
            && event.dedupe_key.as_deref() != Some(dedupe_key)
        {
            return false;
        }
        return true;
    }
    if let Some(target_thread_id) = event.target_thread_id.as_deref()
        && handle.owner_thread_id != target_thread_id
    {
        return false;
    }
    if let Some(condition) = handle.condition.as_deref()
        && event.condition.as_deref() != Some(condition)
    {
        return false;
    }
    if let Some(source) = handle.source.as_deref()
        && event.source.as_deref() != Some(source)
    {
        return false;
    }
    if let Some(dedupe_key) = handle.dedupe_key.as_deref()
        && event.dedupe_key.as_deref() != Some(dedupe_key)
    {
        return false;
    }
    true
}

pub fn title_from_body(body: &str, fallback: &str) -> String {
    body.lines()
        .find_map(|line| line.strip_prefix("# ").map(str::trim))
        .filter(|title| !title.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

pub fn require_section(body: &str, section: &str) -> Result<()> {
    let heading = format!("## {section}");
    if body.lines().any(|line| line.trim() == heading) {
        Ok(())
    } else {
        Err(QunuxError::InvalidState(format!(
            "body missing required section `{heading}`"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> QunuxRuntime {
        let dir = tempfile::tempdir().expect("tempdir");
        let context = RuntimeContext::new(dir.path());
        QunuxRuntime::load_or_init(context, "Root task", "# Root task\n\nBody").expect("init")
    }

    fn solve_one_go(runtime: &mut QunuxRuntime, problem_id: &str) -> (String, String, String) {
        let ticket_id = runtime
            .create_ticket(problem_id, "Ticket", "# Ticket\n\nBody")
            .expect("ticket");
        runtime
            .classify_ticket(&ticket_id, TicketClassification::OneGo, "bounded")
            .expect("classify");
        runtime
            .set_status(EntityKind::Problem, problem_id, "doing")
            .expect("problem doing");
        let result_id = runtime
            .record_result(&ticket_id, "Result", "# Result\n\nDone")
            .expect("result");
        let check_id = runtime
            .check(
                problem_id,
                CheckStatus::Success,
                vec![result_id.clone()],
                "Check",
                "# Check\n\nSuccess",
                None,
            )
            .expect("check");
        (ticket_id, result_id, check_id)
    }

    fn runtime_with_failed_check_followup() -> (QunuxRuntime, String, String) {
        let mut runtime = runtime();
        let ticket = runtime
            .create_ticket("P000", "Ticket", "# Ticket")
            .expect("ticket");
        runtime
            .classify_ticket(&ticket, TicketClassification::OneGo, "bounded")
            .expect("classify");
        runtime
            .set_status(EntityKind::Problem, "P000", "doing")
            .expect("doing");
        let result = runtime
            .record_result(&ticket, "Result", "# Result")
            .expect("result");
        let check = runtime
            .check(
                "P000",
                CheckStatus::NotSuccess,
                vec![result],
                "Check",
                "# Check",
                Some(("Follow up".to_string(), "# Follow up".to_string())),
            )
            .expect("not success");
        let followup = runtime.state().checks[&check]
            .followup_problem_id
            .clone()
            .expect("follow-up");
        (runtime, check, followup)
    }

    fn runtime_with_user_task_child() -> QunuxRuntime {
        let mut runtime = runtime();
        runtime
            .receive_passive_event(PassiveEventInput {
                kind: PassiveEventKind::UserInput,
                event_key: None,
                target_thread_id: Some(DEFAULT_THREAD_ID.to_string()),
                condition: Some("task".to_string()),
                source: Some("chat".to_string()),
                summary: "user gave a task".to_string(),
                payload_ref: Some("turn:task".to_string()),
                dedupe_key: Some("msg-task".to_string()),
            })
            .expect("receive passive event");
        runtime
            .create_user_task_from_inbox(
                "IN000",
                "User task",
                "# User task\n\n## Problem\n\nDo the task.\n\n## Success Criteria\n\n- Done.",
                "actionable user input converted to child problem",
            )
            .expect("create user task");
        runtime.validate().expect("valid user-task child");
        runtime
    }

    #[test]
    fn runtime_persists_root_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let context = RuntimeContext::new(dir.path());
        let runtime =
            QunuxRuntime::load_or_init(context.clone(), "Root task", "# Root task\n\nBody")
                .expect("init");
        assert_eq!(runtime.state().root_problem_id, "P000");
        assert!(context.state_path().exists());

        let loaded = QunuxRuntime::load(context).expect("load");
        assert_eq!(loaded.state().problems["P000"].title, "Root task");
    }

    #[test]
    fn default_root_problem_is_process_mission_without_precreated_work() {
        let dir = tempfile::tempdir().expect("tempdir");
        let context = RuntimeContext::new(dir.path());
        let runtime = QunuxRuntime::load_or_init(
            context,
            DEFAULT_ROOT_PROBLEM_TITLE,
            DEFAULT_ROOT_PROBLEM_BODY,
        )
        .expect("init");

        let state = runtime.state();
        assert_eq!(state.problems.len(), 1);
        assert_eq!(state.root_problem_id, "P000");
        let root = &state.problems["P000"];
        assert_eq!(root.title, DEFAULT_ROOT_PROBLEM_TITLE);
        assert!(root.body.contains("## Problem"));
        assert!(root.body.contains("## Success Criteria"));
        assert!(root.body.contains("exactly this root problem"));
        assert!(root.body.contains("does not pre-create a ticket"));
        assert!(root.body.contains("headless-first Agent OS process"));
        assert!(
            root.body
                .contains("TUI cockpit, chat transcript, shell, files, tools, timers")
        );
        assert!(
            root.body
                .contains("lifecycle mission, not an initialization task")
        );
        assert!(root.body.contains("park the thread with `qunux.wait`"));
        assert!(
            root.body
                .contains("Keep visible user replies separate from Qunux state mutation")
        );
        assert!(
            root.body
                .contains("Close this root only for an explicit shutdown/end-of-process request")
        );
        assert!(root.ticket_id.is_none());
        assert!(root.child_problem_ids.is_empty());
        assert!(root.followup_problem_ids.is_empty());
        assert!(root.result_ids.is_empty());
        assert!(root.check_ids.is_empty());

        assert!(state.tickets.is_empty());
        assert!(state.results.is_empty());
        assert!(state.checks.is_empty());
        assert!(state.waits.is_empty());
        assert!(state.handles.is_empty());
        assert!(state.passive_events.is_empty());
        assert!(state.inbox.is_empty());
        assert!(state.io_events.is_empty());

        let next = runtime.next();
        assert_eq!(next.action, NextAction::CreateSolutionTicket);
        assert_eq!(next.disposition, NextDisposition::Runnable);
        assert_eq!(runtime.state().tickets.len(), 0);
        assert_eq!(runtime.state().waits.len(), 0);
    }

    #[test]
    fn load_or_init_upgrades_legacy_pristine_root_problem() {
        let dir = tempfile::tempdir().expect("tempdir");
        let context = RuntimeContext::new(dir.path());
        let first = QunuxRuntime::load_or_init(
            context.clone(),
            LEGACY_DEFAULT_ROOT_PROBLEM_TITLE,
            "# Qunux root task\n\n## Problem\n\nOld placeholder.",
        )
        .expect("legacy init");
        assert_eq!(
            first.state().problems["P000"].title,
            LEGACY_DEFAULT_ROOT_PROBLEM_TITLE
        );

        let upgraded = QunuxRuntime::load_or_init(
            context,
            DEFAULT_ROOT_PROBLEM_TITLE,
            DEFAULT_ROOT_PROBLEM_BODY,
        )
        .expect("upgrade");

        let root = &upgraded.state().problems["P000"];
        assert_eq!(root.title, DEFAULT_ROOT_PROBLEM_TITLE);
        assert!(root.body.contains("This Qunux process exists"));
        assert!(
            upgraded
                .state()
                .events
                .iter()
                .any(|event| event.kind == "root_problem_upgraded")
        );
    }

    #[test]
    fn load_or_init_does_not_upgrade_non_pristine_legacy_root_problem() {
        let dir = tempfile::tempdir().expect("tempdir");
        let context = RuntimeContext::new(dir.path());
        let mut first = QunuxRuntime::load_or_init(
            context.clone(),
            LEGACY_DEFAULT_ROOT_PROBLEM_TITLE,
            "# Qunux root task\n\n## Problem\n\nOld placeholder.",
        )
        .expect("legacy init");
        first
            .create_ticket("P000", "Existing work", "# Existing work")
            .expect("ticket");

        let reloaded = QunuxRuntime::load_or_init(
            context,
            DEFAULT_ROOT_PROBLEM_TITLE,
            DEFAULT_ROOT_PROBLEM_BODY,
        )
        .expect("reload");

        assert_eq!(
            reloaded.state().problems["P000"].title,
            LEGACY_DEFAULT_ROOT_PROBLEM_TITLE
        );
    }

    #[test]
    fn rejects_path_like_ids() {
        let err = RuntimeContext::with_ids("/tmp", "../escape", "QT000").expect_err("invalid");
        assert!(matches!(err, QunuxError::InvalidId(_)));
    }

    #[test]
    fn rejects_invalid_session_ids_before_process_path_derivation() {
        for invalid in ["", ".", "..", "../escape", "bad/session", "bad\\session"] {
            let err = RuntimeContext::for_session("/tmp/workspace", invalid)
                .expect_err("invalid session id");
            assert!(
                matches!(err, QunuxError::InvalidId(_)),
                "expected InvalidId for {invalid:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn session_context_derives_stable_safe_process_id() {
        let context =
            RuntimeContext::for_session("/tmp/workspace", "codex-session-001").expect("context");
        assert_eq!(context.process_id, "QP-codex-session-001");
        assert_eq!(context.thread_id, DEFAULT_THREAD_ID);
        assert_eq!(
            context.actor_session_id.as_deref(),
            Some("codex-session-001")
        );

        let second =
            RuntimeContext::for_session("/tmp/workspace", "codex-session-001").expect("context");
        assert_eq!(context.process_id, second.process_id);

        let other =
            RuntimeContext::for_session("/tmp/workspace", "codex-session-002").expect("context");
        assert_ne!(context.process_id, other.process_id);
    }

    #[test]
    fn session_bound_runtime_separates_sessions_in_same_workspace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = QunuxRuntime::load_or_init_for_session(
            dir.path(),
            "codex-session-001",
            "First root",
            "# First root",
        )
        .expect("first init");
        let second = QunuxRuntime::load_or_init_for_session(
            dir.path(),
            "codex-session-002",
            "Second root",
            "# Second root",
        )
        .expect("second init");

        assert_ne!(first.context().process_id, second.context().process_id);
        assert_ne!(first.context().state_path(), second.context().state_path());
        assert_eq!(first.state().problems["P000"].title, "First root");
        assert_eq!(second.state().problems["P000"].title, "Second root");

        let reloaded_first = QunuxRuntime::load_or_init_for_session(
            dir.path(),
            "codex-session-001",
            "Ignored root",
            "# Ignored root",
        )
        .expect("reload first");
        assert_eq!(reloaded_first.state().problems["P000"].title, "First root");
        assert_eq!(
            reloaded_first.context().process_id,
            first.context().process_id
        );
    }

    #[test]
    fn session_bound_runtime_persists_process_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let runtime = QunuxRuntime::load_or_init_for_session(
            dir.path(),
            "codex-session-001",
            "Root task",
            "# Root task",
        )
        .expect("init");

        assert_eq!(runtime.context().process_id, "QP-codex-session-001");
        assert_eq!(runtime.context().thread_id, DEFAULT_THREAD_ID);
        assert_eq!(runtime.state().process.id, runtime.state().process_id);
        assert_eq!(
            runtime.state().process.root_actor_session_id.as_deref(),
            Some("codex-session-001")
        );
        assert_eq!(
            runtime.state().threads[DEFAULT_THREAD_ID]
                .actor_session_id
                .as_deref(),
            Some("codex-session-001")
        );

        let reloaded = QunuxRuntime::load_or_init_for_session(
            dir.path(),
            "codex-session-001",
            "Ignored",
            "# Ignored",
        )
        .expect("reload");
        assert_eq!(reloaded.state().root_problem_id, "P000");
        assert_eq!(
            reloaded.state().problems["P000"].title,
            "Root task",
            "existing session-bound process should be loaded, not reinitialized"
        );
        reloaded.validate().expect("valid");
    }

    #[test]
    fn validate_rejects_process_metadata_mismatch() {
        let mut runtime = runtime();
        runtime.state_mut().process.id = "QP-other".to_string();
        let err = runtime.validate().expect_err("metadata mismatch");
        assert!(err.to_string().contains("process metadata id"));
    }

    #[test]
    fn validate_rejects_process_metadata_main_thread_mismatch() {
        let mut runtime = runtime();
        runtime.state_mut().process.main_thread_id = "QT999".to_string();
        let err = runtime.validate().expect_err("main thread mismatch");
        assert!(err.to_string().contains("process metadata main thread"));
    }

    #[test]
    fn one_go_path_closes_root() {
        let mut runtime = runtime();
        let (_ticket_id, _result_id, _check_id) = solve_one_go(&mut runtime, "P000");

        assert_eq!(runtime.state().problems["P000"].status, ProblemStatus::Done);
        assert_eq!(runtime.next().action, NextAction::None);
        assert_eq!(runtime.next().disposition, NextDisposition::Terminal);
        runtime.validate().expect("valid");
    }

    #[test]
    fn next_disposition_tracks_scheduler_state() {
        let mut runtime_under_test = runtime();
        let initial_next = runtime_under_test.next();
        assert_eq!(initial_next.action, NextAction::CreateSolutionTicket);
        assert_eq!(initial_next.disposition, NextDisposition::Runnable);

        let parent_ticket = runtime_under_test
            .create_ticket("P000", "Parent ticket", "# Parent ticket")
            .expect("ticket");
        runtime_under_test
            .classify_ticket(&parent_ticket, TicketClassification::Split, "needs child")
            .expect("classify");
        runtime_under_test
            .set_status(EntityKind::Ticket, &parent_ticket, "splitting")
            .expect("splitting");
        let child_id = runtime_under_test
            .create_problem_from_ticket("P000", &parent_ticket, "Child", "# Child")
            .expect("child");

        let spawn_next = runtime_under_test.next();
        assert_eq!(spawn_next.action, NextAction::SpawnThread);
        assert_eq!(spawn_next.disposition, NextDisposition::Runnable);
        assert_eq!(spawn_next.problem_id.as_deref(), Some(child_id.as_str()));

        let spawned = runtime_under_test
            .spawn_thread(
                &child_id,
                ContextForkPolicy::FullContext,
                "Solve child",
                None,
                None,
                Vec::new(),
            )
            .expect("spawn thread");
        let wait_next = runtime_under_test.next();
        assert_eq!(wait_next.action, NextAction::WaitThread);
        assert_eq!(wait_next.disposition, NextDisposition::IoWait);
        assert_eq!(
            wait_next.target_thread_id.as_deref(),
            Some(spawned.thread_id.as_str())
        );

        let mut terminal_runtime = runtime();
        solve_one_go(&mut terminal_runtime, "P000");
        let terminal_next = terminal_runtime.next();
        assert_eq!(terminal_next.action, NextAction::None);
        assert_eq!(terminal_next.disposition, NextDisposition::Terminal);
    }

    #[test]
    fn split_parent_waits_for_child_result() {
        let mut runtime = runtime();
        let parent_ticket = runtime
            .create_ticket("P000", "Parent ticket", "# Parent ticket")
            .expect("ticket");
        runtime
            .classify_ticket(&parent_ticket, TicketClassification::Split, "needs child")
            .expect("classify");
        runtime
            .set_status(EntityKind::Ticket, &parent_ticket, "splitting")
            .expect("splitting");
        let child_id = runtime
            .create_problem_from_ticket("P000", &parent_ticket, "Child", "# Child")
            .expect("child");

        runtime
            .set_status(EntityKind::Problem, "P000", "doing")
            .expect("parent doing");
        let early = runtime
            .record_result(&parent_ticket, "Parent result", "# Parent result")
            .expect_err("child still open");
        assert!(early.to_string().contains("child problems still open"));

        solve_one_go(&mut runtime, &child_id);
        let parent_result = runtime
            .record_result(&parent_ticket, "Parent result", "# Parent result")
            .expect("parent result");
        runtime
            .check(
                "P000",
                CheckStatus::Success,
                vec![parent_result],
                "Parent check",
                "# Parent check",
                None,
            )
            .expect("parent check");

        assert_eq!(runtime.state().problems["P000"].status, ProblemStatus::Done);
        runtime.validate().expect("valid");
    }

    #[test]
    fn runtime_spawn_child_from_executing_one_go_ticket() {
        let mut runtime = runtime();
        let parent_ticket = runtime
            .create_ticket("P000", "Parent ticket", "# Parent ticket")
            .expect("ticket");
        runtime
            .classify_ticket(&parent_ticket, TicketClassification::OneGo, "bounded")
            .expect("classify");
        runtime
            .set_status(EntityKind::Problem, "P000", "doing")
            .expect("parent doing");
        runtime
            .set_status(EntityKind::Ticket, &parent_ticket, "executing")
            .expect("executing");
        let child_id = runtime
            .create_problem_from_ticket_with_mode(
                "P000",
                &parent_ticket,
                TicketChildMode::Spawn,
                "Runtime child",
                "# Runtime child",
            )
            .expect("spawned child problem");

        let child = &runtime.state().problems[&child_id];
        assert_eq!(child.created_from_ticket_mode, Some(TicketChildMode::Spawn));
        let next = runtime.next();
        assert_eq!(next.action, NextAction::SpawnThread);
        assert_eq!(next.problem_id.as_deref(), Some(child_id.as_str()));

        let early = runtime
            .record_result(&parent_ticket, "Parent result", "# Parent result")
            .expect_err("spawned child still open");
        assert!(early.to_string().contains("child problems still open"));

        solve_one_go(&mut runtime, &child_id);
        let parent_result = runtime
            .record_result(&parent_ticket, "Parent result", "# Parent result")
            .expect("parent result");
        runtime
            .check(
                "P000",
                CheckStatus::Success,
                vec![parent_result],
                "Parent check",
                "# Parent check",
                None,
            )
            .expect("parent check");
        runtime.validate().expect("valid");
    }

    #[test]
    fn split_and_spawn_child_modes_reject_wrong_ticket_state() {
        let mut one_go_runtime = runtime();
        let ticket = one_go_runtime
            .create_ticket("P000", "Parent ticket", "# Parent ticket")
            .expect("ticket");
        one_go_runtime
            .classify_ticket(&ticket, TicketClassification::OneGo, "bounded")
            .expect("classify");
        one_go_runtime
            .set_status(EntityKind::Problem, "P000", "doing")
            .expect("parent doing");
        one_go_runtime
            .set_status(EntityKind::Ticket, &ticket, "executing")
            .expect("executing");

        let wrong_split = one_go_runtime
            .create_problem_from_ticket_with_mode(
                "P000",
                &ticket,
                TicketChildMode::Split,
                "Wrong split",
                "# Wrong split",
            )
            .expect_err("one_go ticket cannot create split child");
        assert!(wrong_split.to_string().contains("not classified as split"));

        let mut split_runtime = runtime();
        let split_ticket = split_runtime
            .create_ticket("P000", "Split ticket", "# Split ticket")
            .expect("ticket");
        split_runtime
            .classify_ticket(&split_ticket, TicketClassification::Split, "needs split")
            .expect("classify");
        split_runtime
            .set_status(EntityKind::Problem, "P000", "doing")
            .expect("parent doing");
        split_runtime
            .set_status(EntityKind::Ticket, &split_ticket, "splitting")
            .expect("splitting");
        let wrong_spawn = split_runtime
            .create_problem_from_ticket_with_mode(
                "P000",
                &split_ticket,
                TicketChildMode::Spawn,
                "Wrong spawn",
                "# Wrong spawn",
            )
            .expect_err("split ticket cannot runtime-spawn child");
        assert!(wrong_spawn.to_string().contains("not classified as one_go"));

        let child_id = split_runtime
            .create_problem_from_ticket_with_mode(
                "P000",
                &split_ticket,
                TicketChildMode::Split,
                "Right split",
                "# Right split",
            )
            .expect("split child");
        assert_eq!(
            split_runtime.state().problems[&child_id].created_from_ticket_mode,
            Some(TicketChildMode::Split)
        );
        split_runtime.validate().expect("valid");
    }

    #[test]
    fn ticket_child_creation_rejects_ticket_from_different_problem() {
        let mut runtime = runtime();
        let parent_ticket = runtime
            .create_ticket("P000", "Parent ticket", "# Parent ticket")
            .expect("parent ticket");
        runtime
            .classify_ticket(&parent_ticket, TicketClassification::Split, "needs child")
            .expect("classify parent ticket");
        runtime
            .set_status(EntityKind::Ticket, &parent_ticket, "splitting")
            .expect("parent splitting");
        let child_id = runtime
            .create_problem_from_ticket("P000", &parent_ticket, "Child", "# Child")
            .expect("child problem");

        let child_ticket = runtime
            .create_ticket(&child_id, "Child ticket", "# Child ticket")
            .expect("child ticket");
        runtime
            .classify_ticket(&child_ticket, TicketClassification::Split, "nested")
            .expect("classify child ticket");
        runtime
            .set_status(EntityKind::Ticket, &child_ticket, "splitting")
            .expect("child splitting");

        let wrong_parent = runtime
            .create_problem_from_ticket("P000", &child_ticket, "Wrong", "# Wrong")
            .expect_err("source ticket must belong to requested parent");
        assert!(
            wrong_parent
                .to_string()
                .contains("does not belong to problem P000")
        );
        runtime.validate().expect("valid after rejected mismatch");
    }

    #[test]
    fn spawned_child_thread_gets_scoped_next_and_parent_joins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root_context = RuntimeContext::new(dir.path());
        let mut runtime =
            QunuxRuntime::load_or_init(root_context.clone(), "Root task", "# Root task")
                .expect("init");
        let parent_ticket = runtime
            .create_ticket("P000", "Parent ticket", "# Parent ticket")
            .expect("ticket");
        runtime
            .classify_ticket(
                &parent_ticket,
                TicketClassification::Split,
                "parallel child",
            )
            .expect("classify");
        runtime
            .set_status(EntityKind::Ticket, &parent_ticket, "splitting")
            .expect("splitting");
        let child_id = runtime
            .create_problem_from_ticket("P000", &parent_ticket, "Child", "# Child")
            .expect("child");

        let next = runtime.next();
        assert_eq!(next.action, NextAction::SpawnThread);
        assert_eq!(next.problem_id.as_deref(), Some(child_id.as_str()));

        let spawned = runtime
            .spawn_thread(
                &child_id,
                ContextForkPolicy::FullContext,
                "Solve child",
                None,
                None,
                Vec::new(),
            )
            .expect("spawn thread");
        assert_eq!(spawned.thread_id, "QT001");
        assert_eq!(spawned.handle_id, "H000");
        assert_eq!(spawned.wait_id, "W000");
        assert_eq!(
            runtime.state().handles[&spawned.handle_id].status,
            IoHandleStatus::Pending
        );
        assert_eq!(
            runtime.state().waits[&spawned.wait_id].status,
            WaitStatus::Waiting
        );
        assert_eq!(runtime.next().action, NextAction::WaitThread);

        let child_context =
            RuntimeContext::with_ids(dir.path(), DEFAULT_PROCESS_ID, &spawned.thread_id)
                .expect("child context");
        let mut child_runtime = QunuxRuntime::load(child_context).expect("child load");
        let child_next = child_runtime.next();
        assert_eq!(child_next.problem_id.as_deref(), Some(child_id.as_str()));
        assert_eq!(child_next.action, NextAction::CreateSolutionTicket);

        let parent_write = child_runtime
            .create_ticket("P000", "Wrong", "# Wrong")
            .expect_err("child cannot mutate parent");
        assert!(
            parent_write
                .to_string()
                .contains("cannot mutate problem P000")
        );

        let (_ticket, _result, _check) = solve_one_go(&mut child_runtime, &child_id);
        assert_eq!(
            child_runtime.state().handles[&spawned.handle_id].status,
            IoHandleStatus::Ready
        );
        assert_eq!(
            child_runtime.state().waits[&spawned.wait_id].status,
            WaitStatus::Ready
        );

        let mut parent_runtime = QunuxRuntime::load(root_context).expect("parent reload");
        assert_eq!(parent_runtime.next().action, NextAction::JoinThread);
        parent_runtime
            .join_thread(&spawned.thread_id)
            .expect("join child");
        assert_eq!(
            parent_runtime.state().handles[&spawned.handle_id].status,
            IoHandleStatus::Consumed
        );
        assert_eq!(
            parent_runtime.state().waits[&spawned.wait_id].status,
            WaitStatus::Consumed
        );
        assert_eq!(parent_runtime.next().action, NextAction::RecordResult);
        parent_runtime.validate().expect("valid");
    }

    #[test]
    fn parent_cannot_mutate_child_problem_after_spawn_transfer() {
        let mut runtime = runtime();
        let parent_ticket = runtime
            .create_ticket("P000", "Parent ticket", "# Parent ticket")
            .expect("ticket");
        runtime
            .classify_ticket(&parent_ticket, TicketClassification::Split, "child")
            .expect("classify");
        runtime
            .set_status(EntityKind::Ticket, &parent_ticket, "splitting")
            .expect("splitting");
        let child_id = runtime
            .create_problem_from_ticket("P000", &parent_ticket, "Child", "# Child")
            .expect("child");
        let spawned = runtime
            .spawn_thread(
                &child_id,
                ContextForkPolicy::FullContext,
                "Solve child",
                None,
                None,
                Vec::new(),
            )
            .expect("spawn thread");

        assert_eq!(
            runtime.state().problems[&child_id].owner_thread_id,
            spawned.thread_id
        );
        let parent_write = runtime
            .create_ticket(&child_id, "Parent write", "# Parent write")
            .expect_err("parent no longer owns child subtree");
        assert!(parent_write.to_string().contains("cannot mutate problem"));
        runtime
            .validate()
            .expect("valid after rejected parent write");
    }

    #[test]
    fn auto_join_consumes_done_child_handle_and_wakes_parent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root_context = RuntimeContext::new(dir.path());
        let mut runtime =
            QunuxRuntime::load_or_init(root_context.clone(), "Root task", "# Root task")
                .expect("init");
        let parent_ticket = runtime
            .create_ticket("P000", "Parent ticket", "# Parent ticket")
            .expect("ticket");
        runtime
            .classify_ticket(
                &parent_ticket,
                TicketClassification::Split,
                "parallel child",
            )
            .expect("classify");
        runtime
            .set_status(EntityKind::Ticket, &parent_ticket, "splitting")
            .expect("splitting");
        let child_id = runtime
            .create_problem_from_ticket("P000", &parent_ticket, "Child", "# Child")
            .expect("child");
        let spawned = runtime
            .spawn_thread(
                &child_id,
                ContextForkPolicy::FullContext,
                "Solve child",
                None,
                None,
                Vec::new(),
            )
            .expect("spawn thread");
        let child_context =
            RuntimeContext::with_ids(dir.path(), DEFAULT_PROCESS_ID, &spawned.thread_id)
                .expect("child context");
        let mut child_runtime = QunuxRuntime::load(child_context).expect("child load");

        solve_one_go(&mut child_runtime, &child_id);
        child_runtime
            .auto_join_child_thread(&spawned.thread_id, "auto join done child")
            .expect("auto join");

        assert!(
            child_runtime.state().threads[&spawned.thread_id]
                .joined_at
                .is_some()
        );
        assert_eq!(
            child_runtime.state().handles[&spawned.handle_id].status,
            IoHandleStatus::Consumed
        );
        assert_eq!(
            child_runtime.state().waits[&spawned.wait_id].status,
            WaitStatus::Consumed
        );
        assert!(child_runtime.state().io_events.iter().any(|event| {
            event.kind == IoEventKind::ChildThreadJoined
                && event.thread_id.as_deref() == Some(spawned.thread_id.as_str())
        }));

        let parent_runtime = QunuxRuntime::load(root_context).expect("parent reload");
        assert_eq!(parent_runtime.next().action, NextAction::RecordResult);
        parent_runtime.validate().expect("valid");
    }

    #[test]
    fn actor_completion_without_qunux_done_marks_failed_and_wakes_parent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root_context = RuntimeContext::new(dir.path());
        let mut runtime =
            QunuxRuntime::load_or_init(root_context.clone(), "Root task", "# Root task")
                .expect("init");
        let parent_ticket = runtime
            .create_ticket("P000", "Parent ticket", "# Parent ticket")
            .expect("ticket");
        runtime
            .classify_ticket(
                &parent_ticket,
                TicketClassification::Split,
                "parallel child",
            )
            .expect("classify");
        runtime
            .set_status(EntityKind::Ticket, &parent_ticket, "splitting")
            .expect("splitting");
        let child_id = runtime
            .create_problem_from_ticket("P000", &parent_ticket, "Child", "# Child")
            .expect("child");
        let spawned = runtime
            .spawn_thread(
                &child_id,
                ContextForkPolicy::FullContext,
                "Solve child",
                None,
                None,
                Vec::new(),
            )
            .expect("spawn thread");

        let event_id = runtime
            .record_actor_completed_without_thread_done(&spawned.thread_id, "completed")
            .expect("record incomplete actor completion");

        assert_eq!(
            runtime.state().handles[&spawned.handle_id].status,
            IoHandleStatus::Failed
        );
        assert_eq!(
            runtime.state().threads[&spawned.thread_id].status,
            ThreadStatus::Failed
        );
        assert_eq!(
            runtime.state().waits[&spawned.wait_id].status,
            WaitStatus::Ready
        );
        assert!(
            runtime.state().threads[&spawned.thread_id]
                .joined_at
                .is_none()
        );
        assert_eq!(runtime.status().failed_threads, 1);
        assert_eq!(runtime.status().failed_handles, 1);
        let next = runtime.next();
        assert_eq!(next.action, NextAction::RecoverThread);
        assert_eq!(
            next.target_thread_id.as_deref(),
            Some(spawned.thread_id.as_str())
        );
        assert!(runtime.state().io_events.iter().any(|event| {
            event.id == event_id
                && event.kind == IoEventKind::ActorCompletedWithoutThreadDone
                && event.thread_id.as_deref() == Some(spawned.thread_id.as_str())
        }));
        assert!(runtime.state().io_events.iter().any(|event| {
            event.kind == IoEventKind::ChildThreadFailed
                && event.thread_id.as_deref() == Some(spawned.thread_id.as_str())
                && event.handle_id.as_deref() == Some(spawned.handle_id.as_str())
        }));
        runtime.validate().expect("valid");
    }

    #[test]
    fn recover_failed_child_thread_returns_subtree_to_parent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root_context = RuntimeContext::new(dir.path());
        let mut runtime =
            QunuxRuntime::load_or_init(root_context.clone(), "Root task", "# Root task")
                .expect("init");
        let parent_ticket = runtime
            .create_ticket("P000", "Parent ticket", "# Parent ticket")
            .expect("ticket");
        runtime
            .classify_ticket(
                &parent_ticket,
                TicketClassification::Split,
                "parallel child",
            )
            .expect("classify");
        runtime
            .set_status(EntityKind::Ticket, &parent_ticket, "splitting")
            .expect("splitting");
        let child_id = runtime
            .create_problem_from_ticket("P000", &parent_ticket, "Child", "# Child")
            .expect("child");
        let spawned = runtime
            .spawn_thread(
                &child_id,
                ContextForkPolicy::FullContext,
                "Solve child",
                None,
                None,
                Vec::new(),
            )
            .expect("spawn thread");
        runtime
            .record_actor_completed_without_thread_done(&spawned.thread_id, "completed")
            .expect("record incomplete actor completion");
        assert_eq!(runtime.next().action, NextAction::RecoverThread);

        let recovered = runtime
            .recover_thread(&spawned.thread_id)
            .expect("recover failed child thread");

        assert_eq!(recovered.thread_id, spawned.thread_id);
        assert_eq!(recovered.root_problem_id, child_id);
        assert_eq!(
            runtime.state().threads[&spawned.thread_id].status,
            ThreadStatus::Recovered
        );
        assert_eq!(
            runtime.state().handles[&spawned.handle_id].status,
            IoHandleStatus::Cancelled
        );
        assert_eq!(
            runtime.state().waits[&spawned.wait_id].status,
            WaitStatus::Consumed
        );
        assert_eq!(
            runtime.state().problems[&recovered.root_problem_id].owner_thread_id,
            DEFAULT_THREAD_ID
        );
        let next = runtime.next();
        assert_eq!(next.action, NextAction::SpawnThread);
        assert_eq!(
            next.problem_id.as_deref(),
            Some(recovered.root_problem_id.as_str())
        );
        assert!(runtime.state().io_events.iter().any(|event| {
            event.kind == IoEventKind::ChildThreadRecovered
                && event.thread_id.as_deref() == Some(spawned.thread_id.as_str())
                && event.handle_id.as_deref() == Some(spawned.handle_id.as_str())
        }));
        runtime.validate().expect("valid");
    }

    #[test]
    fn watcher_thread_uses_ordinary_child_thread_wait() {
        let mut runtime = runtime();
        let parent_ticket = runtime
            .create_ticket("P000", "Parent ticket", "# Parent ticket")
            .expect("ticket");
        runtime
            .classify_ticket(
                &parent_ticket,
                TicketClassification::Split,
                "watch condition",
            )
            .expect("classify");
        runtime
            .set_status(EntityKind::Ticket, &parent_ticket, "splitting")
            .expect("splitting");
        let child_id = runtime
            .create_problem_from_ticket(
                "P000",
                &parent_ticket,
                "Watch PR readiness",
                "# Watch PR readiness",
            )
            .expect("child");
        let watcher_bootstrap = "Watcher goal: inspect PR CI and comments every 30 minutes; close only with evidence when criteria are met.";

        let spawned = runtime
            .spawn_thread(
                &child_id,
                ContextForkPolicy::FullContext,
                watcher_bootstrap,
                None,
                None,
                Vec::new(),
            )
            .expect("spawn watcher thread");

        let child_thread = &runtime.state().threads[&spawned.thread_id];
        assert_eq!(child_thread.root_problem_id, child_id);
        assert_eq!(
            child_thread.context_fork.bootstrap_instruction,
            watcher_bootstrap
        );

        let handle = &runtime.state().handles[&spawned.handle_id];
        assert_eq!(handle.kind, IoHandleKind::ChildThread);
        assert_eq!(handle.owner_thread_id, DEFAULT_THREAD_ID);
        assert_eq!(
            handle.target_thread_id.as_deref(),
            Some(spawned.thread_id.as_str())
        );
        assert_eq!(handle.status, IoHandleStatus::Pending);

        let wait = &runtime.state().waits[&spawned.wait_id];
        assert_eq!(wait.thread_id, DEFAULT_THREAD_ID);
        assert_eq!(wait.handle_ids, vec![spawned.handle_id.clone()]);
        assert_eq!(wait.mode, WaitMode::All);
        assert_eq!(wait.status, WaitStatus::Waiting);

        let next = runtime.next();
        assert_eq!(next.action, NextAction::WaitThread);
        assert_eq!(next.disposition, NextDisposition::IoWait);
        assert_eq!(
            next.target_thread_id.as_deref(),
            Some(spawned.thread_id.as_str())
        );
        runtime.validate().expect("valid");
    }

    #[test]
    fn child_spawn_failure_records_failed_terminal_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root_context = RuntimeContext::new(dir.path());
        let mut runtime =
            QunuxRuntime::load_or_init(root_context.clone(), "Root task", "# Root task")
                .expect("init");
        let parent_ticket = runtime
            .create_ticket("P000", "Parent ticket", "# Parent ticket")
            .expect("ticket");
        runtime
            .classify_ticket(&parent_ticket, TicketClassification::Split, "spawn child")
            .expect("classify");
        runtime
            .set_status(EntityKind::Ticket, &parent_ticket, "splitting")
            .expect("splitting");
        let child_id = runtime
            .create_problem_from_ticket("P000", &parent_ticket, "Child", "# Child")
            .expect("child");
        let spawned = runtime
            .spawn_thread(
                &child_id,
                ContextForkPolicy::FullContext,
                "Solve child",
                None,
                None,
                Vec::new(),
            )
            .expect("spawn thread");

        let event_id = runtime
            .record_child_thread_spawn_failed(&spawned.thread_id, "Codex child spawn failed")
            .expect("record spawn failure");

        assert_eq!(
            runtime.state().threads[&spawned.thread_id].status,
            ThreadStatus::Failed
        );
        assert_eq!(
            runtime.state().handles[&spawned.handle_id].status,
            IoHandleStatus::Failed
        );
        assert_eq!(
            runtime.state().waits[&spawned.wait_id].status,
            WaitStatus::Ready
        );
        assert_eq!(runtime.next().action, NextAction::RecoverThread);
        assert!(runtime.state().io_events.iter().any(|event| {
            event.id == event_id
                && event.kind == IoEventKind::ChildThreadSpawnFailed
                && event.handle_id.as_deref() == Some(spawned.handle_id.as_str())
                && event.thread_id.as_deref() == Some(spawned.thread_id.as_str())
        }));
        runtime.validate().expect("valid");
    }

    #[test]
    fn bound_child_actor_session_resolves_to_child_thread() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root_context =
            RuntimeContext::for_session(dir.path(), "parent-session").expect("root context");
        let mut runtime =
            QunuxRuntime::load_or_init(root_context.clone(), "Root task", "# Root task")
                .expect("init");
        let parent_ticket = runtime
            .create_ticket("P000", "Parent ticket", "# Parent ticket")
            .expect("ticket");
        runtime
            .classify_ticket(&parent_ticket, TicketClassification::Split, "child actor")
            .expect("classify");
        runtime
            .set_status(EntityKind::Ticket, &parent_ticket, "splitting")
            .expect("splitting");
        let child_id = runtime
            .create_problem_from_ticket("P000", &parent_ticket, "Child", "# Child")
            .expect("child");
        let spawned = runtime
            .spawn_thread(
                &child_id,
                ContextForkPolicy::FullContext,
                "Solve child",
                None,
                None,
                Vec::new(),
            )
            .expect("spawn thread");
        runtime
            .bind_thread_actor(&spawned.thread_id, "child-session")
            .expect("bind child actor");

        let child_context = RuntimeContext::with_ids(
            dir.path(),
            root_context.process_id.clone(),
            DEFAULT_THREAD_ID,
        )
        .expect("child context")
        .with_actor_session_id("child-session")
        .expect("child actor")
        .with_parent_actor_session_id("parent-session")
        .expect("parent actor");
        let mut child_runtime = QunuxRuntime::load(child_context).expect("child load");

        assert_eq!(child_runtime.context().thread_id, spawned.thread_id);
        assert_eq!(
            child_runtime.current().status.thread_root_problem_id,
            child_id
        );
        assert_eq!(
            child_runtime
                .state()
                .process
                .root_actor_session_id
                .as_deref(),
            Some("parent-session"),
            "loading as a child actor must not overwrite process root actor"
        );

        let parent_write = child_runtime
            .create_ticket("P000", "Wrong", "# Wrong")
            .expect_err("child cannot mutate parent root");
        assert!(
            parent_write
                .to_string()
                .contains("cannot mutate problem P000")
        );
    }

    #[test]
    fn thread_actor_route_reports_current_and_bound_child_actor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root_context =
            RuntimeContext::for_session(dir.path(), "parent-session").expect("root context");
        let mut runtime =
            QunuxRuntime::load_or_init(root_context.clone(), "Root task", "# Root task")
                .expect("init");

        let current_route = runtime
            .thread_actor_route(DEFAULT_THREAD_ID)
            .expect("current route");
        assert!(current_route.is_current_thread);
        assert_eq!(current_route.bound_actor_id(), Some("parent-session"));

        let parent_ticket = runtime
            .create_ticket("P000", "Parent ticket", "# Parent ticket")
            .expect("ticket");
        runtime
            .classify_ticket(&parent_ticket, TicketClassification::Split, "child actor")
            .expect("classify");
        runtime
            .set_status(EntityKind::Ticket, &parent_ticket, "splitting")
            .expect("splitting");
        let child_id = runtime
            .create_problem_from_ticket("P000", &parent_ticket, "Child", "# Child")
            .expect("child");
        let spawned = runtime
            .spawn_thread(
                &child_id,
                ContextForkPolicy::FullContext,
                "Solve child",
                None,
                None,
                Vec::new(),
            )
            .expect("spawn thread");

        let unbound_child_route = runtime
            .thread_actor_route(&spawned.thread_id)
            .expect("unbound child route");
        assert!(!unbound_child_route.is_current_thread);
        assert_eq!(unbound_child_route.bound_actor_id(), None);

        runtime
            .bind_thread_actor(&spawned.thread_id, "child-session")
            .expect("bind child actor");
        let child_route = runtime
            .thread_actor_route(&spawned.thread_id)
            .expect("bound child route");
        assert!(!child_route.is_current_thread);
        assert_eq!(
            child_route.actor_session_id.as_deref(),
            Some("child-session")
        );
        assert_eq!(
            child_route.codex_thread_id.as_deref(),
            Some("child-session")
        );
        assert_eq!(child_route.bound_actor_id(), Some("child-session"));

        let missing = runtime
            .thread_actor_route("QT999")
            .expect_err("missing thread");
        assert!(missing.to_string().contains("unknown thread QT999"));
    }

    #[test]
    fn followup_must_close_before_parent_success() {
        let mut runtime = runtime();
        let (_ticket_id, root_result, _check_id) = {
            let ticket = runtime
                .create_ticket("P000", "Ticket", "# Ticket")
                .expect("ticket");
            runtime
                .classify_ticket(&ticket, TicketClassification::OneGo, "bounded")
                .expect("classify");
            runtime
                .set_status(EntityKind::Problem, "P000", "doing")
                .expect("doing");
            let result = runtime
                .record_result(&ticket, "Result", "# Result")
                .expect("result");
            let check = runtime
                .check(
                    "P000",
                    CheckStatus::NotSuccess,
                    vec![result.clone()],
                    "Check",
                    "# Check",
                    Some(("Follow up".to_string(), "# Follow up".to_string())),
                )
                .expect("not success");
            (ticket, result, check)
        };

        let second_followup = runtime
            .check(
                "P000",
                CheckStatus::NotSuccess,
                vec![root_result.clone()],
                "Check again",
                "# Check again",
                Some(("Another".to_string(), "# Another".to_string())),
            )
            .expect_err("open followup blocks fan-out");
        assert!(second_followup.to_string().contains("open follow-ups"));

        let followup_id = runtime.state().problems["P000"].followup_problem_ids[0].clone();
        let (_fticket, followup_result, _fcheck) = solve_one_go(&mut runtime, &followup_id);
        runtime
            .check(
                "P000",
                CheckStatus::Success,
                vec![root_result, followup_result],
                "Final check",
                "# Final check",
                None,
            )
            .expect("final check");

        assert_eq!(runtime.state().problems["P000"].status, ProblemStatus::Done);
        runtime.validate().expect("valid");
    }

    #[test]
    fn check_status_enforces_followup_payload_rules() {
        let mut runtime = runtime();
        let ticket = runtime
            .create_ticket("P000", "Ticket", "# Ticket")
            .expect("ticket");
        runtime
            .classify_ticket(&ticket, TicketClassification::OneGo, "bounded")
            .expect("classify");
        runtime
            .set_status(EntityKind::Problem, "P000", "doing")
            .expect("doing");
        let result = runtime
            .record_result(&ticket, "Result", "# Result")
            .expect("result");

        let success_with_followup = runtime
            .check(
                "P000",
                CheckStatus::Success,
                vec![result.clone()],
                "Check",
                "# Check",
                Some((
                    "Impossible follow-up".to_string(),
                    "# Follow-up".to_string(),
                )),
            )
            .expect_err("success cannot create follow-up");
        assert!(
            success_with_followup
                .to_string()
                .contains("success check cannot include a follow-up")
        );

        let missing_followup = runtime
            .check(
                "P000",
                CheckStatus::NotSuccess,
                vec![result],
                "Check",
                "# Check",
                None,
            )
            .expect_err("not_success requires follow-up");
        assert!(
            missing_followup
                .to_string()
                .contains("not_success check requires a follow-up problem")
        );
        assert!(runtime.state().problems["P000"].check_ids.is_empty());
        runtime.validate().expect("valid after rejected checks");
    }

    #[test]
    fn validate_rejects_broken_followup_check_provenance() {
        let (runtime, _check, followup) = runtime_with_failed_check_followup();
        assert_eq!(
            runtime.state().problems[&followup]
                .created_from_check_id
                .as_deref(),
            Some("C000")
        );
        runtime.validate().expect("valid follow-up provenance");

        let (mut missing_backlink, _check, followup) = runtime_with_failed_check_followup();
        missing_backlink
            .state_mut()
            .problems
            .get_mut(&followup)
            .expect("followup")
            .created_from_check_id = None;
        let missing_backlink_error = missing_backlink
            .validate()
            .expect_err("missing follow-up backlink");
        assert!(
            missing_backlink_error
                .to_string()
                .contains("the follow-up does not point back")
        );

        let (mut missing_followup, check, _followup) = runtime_with_failed_check_followup();
        missing_followup
            .state_mut()
            .checks
            .get_mut(&check)
            .expect("check")
            .followup_problem_id = None;
        let missing_followup_error = missing_followup
            .validate()
            .expect_err("not_success must point to follow-up");
        assert!(
            missing_followup_error
                .to_string()
                .contains("has no follow-up problem")
        );

        let (mut wrong_status, check, _followup) = runtime_with_failed_check_followup();
        wrong_status
            .state_mut()
            .checks
            .get_mut(&check)
            .expect("check")
            .status = CheckStatus::Success;
        let wrong_status_error = wrong_status
            .validate()
            .expect_err("follow-up source check must be not_success");
        assert!(wrong_status_error.to_string().contains("success check"));
    }

    #[test]
    fn guards_duplicate_ticket_and_done_reopen() {
        let mut runtime = runtime();
        let (ticket_id, _result_id, _check_id) = solve_one_go(&mut runtime, "P000");

        let duplicate = runtime
            .create_ticket("P000", "Another ticket", "# Another")
            .expect_err("duplicate or done ticket");
        assert!(duplicate.to_string().contains("done problem"));

        let reopen = runtime
            .set_status(EntityKind::Problem, "P000", "doing")
            .expect_err("done cannot reopen");
        assert!(reopen.to_string().contains("done problem"));

        let second_result = runtime
            .record_result(&ticket_id, "Again", "# Again")
            .expect_err("ticket already done");
        assert!(second_result.to_string().contains("done problem"));
    }

    #[test]
    fn passive_unmatched_event_is_preserved_in_inbox() {
        let mut runtime = runtime();

        let receipt = runtime
            .receive_passive_event(PassiveEventInput {
                kind: PassiveEventKind::UserInput,
                event_key: None,
                target_thread_id: Some(DEFAULT_THREAD_ID.to_string()),
                condition: Some("reply".to_string()),
                source: Some("chat".to_string()),
                summary: "user replied".to_string(),
                payload_ref: Some("turn:1".to_string()),
                dedupe_key: Some("msg-1".to_string()),
            })
            .expect("receive passive event");

        assert_eq!(receipt.status, PassiveEventStatus::Inboxed);
        assert_eq!(receipt.wake_decision.status, PassiveEventStatus::Inboxed);
        assert!(receipt.wake_decision.runnable_threads.is_empty());
        assert_eq!(runtime.state().passive_events.len(), 1);
        assert_eq!(
            runtime.state().passive_events[0].event_key.as_ref(),
            Some(&EventKey::passive(
                PassiveEventKind::UserInput,
                Some("reply".to_string()),
                Some("chat".to_string()),
                Some(DEFAULT_THREAD_ID.to_string())
            ))
        );
        assert_eq!(runtime.state().inbox.len(), 1);
        assert_eq!(
            runtime.state().inbox[0].event_key,
            runtime.state().passive_events[0].event_key
        );
        assert_eq!(runtime.state().inbox[0].condition.as_deref(), Some("reply"));
        assert_eq!(runtime.state().inbox[0].source.as_deref(), Some("chat"));
        assert_eq!(
            runtime.state().inbox[0].payload_ref.as_deref(),
            Some("turn:1")
        );
        assert_eq!(
            runtime.state().inbox[0].dedupe_key.as_deref(),
            Some("msg-1")
        );
        assert_eq!(runtime.status().pending_inbox_items, 1);
        let next = runtime.next();
        assert_eq!(next.action, NextAction::HandleInbox);
        assert_eq!(next.disposition, NextDisposition::Runnable);
        assert!(next.instruction.contains("IN000"));
        assert!(next.instruction.contains("user replied"));
        assert!(next.instruction.contains("qunux.scaffold_user_task"));
        assert!(next.instruction.contains("qunux.ingest_user_task"));
        assert!(
            next.instruction
                .contains("ticket must be authored separately")
        );
        assert!(next.instruction.contains("actionable work"));
        assert!(next.instruction.contains("pure small talk"));
        assert!(next.instruction.contains("state-only"));
        assert!(next.instruction.contains("not visible to the user"));
        assert!(
            next.instruction
                .contains("emit any needed visible assistant reply")
        );
        runtime.validate().expect("valid");
    }

    #[test]
    fn acknowledged_inbox_item_is_not_runnable_again() {
        let mut runtime = runtime();
        runtime
            .receive_passive_event(PassiveEventInput {
                kind: PassiveEventKind::UserInput,
                event_key: None,
                target_thread_id: Some(DEFAULT_THREAD_ID.to_string()),
                condition: None,
                source: None,
                summary: "user gave a new task".to_string(),
                payload_ref: Some("turn:1".to_string()),
                dedupe_key: Some("msg-1".to_string()),
            })
            .expect("receive passive event");
        assert_eq!(runtime.next().action, NextAction::HandleInbox);

        let item = runtime
            .acknowledge_inbox_item("IN000", "converted into current response")
            .expect("ack inbox");
        assert_eq!(item.status, PassiveEventStatus::Handled);
        assert_eq!(item.payload_ref.as_deref(), Some("turn:1"));
        assert_eq!(item.dedupe_key.as_deref(), Some("msg-1"));
        assert_eq!(
            runtime.state().passive_events[0].status,
            PassiveEventStatus::Handled
        );
        assert_eq!(runtime.status().pending_inbox_items, 0);
        assert_eq!(runtime.next().action, NextAction::CreateSolutionTicket);
        runtime.validate().expect("valid");
    }

    #[test]
    fn create_user_task_from_inbox_creates_child_problem() {
        let mut runtime = runtime();
        runtime
            .receive_passive_event(PassiveEventInput {
                kind: PassiveEventKind::UserInput,
                event_key: None,
                target_thread_id: Some(DEFAULT_THREAD_ID.to_string()),
                condition: None,
                source: Some("chat".to_string()),
                summary: "user asked for code review".to_string(),
                payload_ref: Some("turn:review".to_string()),
                dedupe_key: Some("msg-review".to_string()),
            })
            .expect("receive passive event");

        let created = runtime
            .create_user_task_from_inbox(
                "IN000",
                "Review requested code",
                "# Review requested code\n\n## Problem\n\nReview the code.\n\n## Success Criteria\n\n- Findings are clear.",
                "actionable user input converted to a child problem",
            )
            .expect("create user task");

        assert_eq!(created.problem_id, "P001");
        assert_eq!(created.parent_problem_id, "P000");
        assert_eq!(created.inbox_item_id, "IN000");
        assert_eq!(created.passive_event_id, "PE000");
        assert_eq!(created.source_kind, PassiveEventKind::UserInput);
        let problem = &runtime.state().problems["P001"];
        assert_eq!(problem.parent_id.as_deref(), Some("P000"));
        assert_eq!(problem.owner_thread_id, DEFAULT_THREAD_ID);
        assert_eq!(
            problem.created_from_passive_event_id.as_deref(),
            Some("PE000")
        );
        assert_eq!(problem.created_from_inbox_item_id.as_deref(), Some("IN000"));
        assert_eq!(
            problem.created_from_user_task_kind,
            Some(PassiveEventKind::UserInput)
        );
        assert!(
            runtime.state().problems["P000"]
                .child_problem_ids
                .contains(&"P001".to_string())
        );
        assert_eq!(runtime.state().inbox[0].status, PassiveEventStatus::Handled);
        assert_eq!(
            runtime.state().passive_events[0].status,
            PassiveEventStatus::Handled
        );
        assert_eq!(runtime.next().action, NextAction::SpawnThread);
        assert_eq!(runtime.next().problem_id.as_deref(), Some("P001"));
    }

    #[test]
    fn create_user_task_from_inbox_creates_valid_child_problem() {
        let mut runtime = runtime();
        runtime
            .receive_passive_event(PassiveEventInput {
                kind: PassiveEventKind::UserInput,
                event_key: None,
                target_thread_id: Some(DEFAULT_THREAD_ID.to_string()),
                condition: None,
                source: Some("chat".to_string()),
                summary: "user asked for implementation".to_string(),
                payload_ref: Some("turn:impl".to_string()),
                dedupe_key: Some("msg-impl".to_string()),
            })
            .expect("receive passive event");

        runtime
            .create_user_task_from_inbox(
                "IN000",
                "Implement requested change",
                "# Implement requested change\n\n## Problem\n\nImplement it.\n\n## Success Criteria\n\n- Change is done.",
                "actionable user input converted to child problem",
            )
            .expect("create user task");

        runtime.validate().expect("valid user-task provenance");
    }

    #[test]
    fn scaffold_user_task_from_inbox_creates_problem_and_ticket() {
        let mut runtime = runtime();
        runtime
            .receive_passive_event(PassiveEventInput {
                kind: PassiveEventKind::UserInput,
                event_key: None,
                target_thread_id: Some(DEFAULT_THREAD_ID.to_string()),
                condition: None,
                source: Some("chat".to_string()),
                summary: "user asked for implementation".to_string(),
                payload_ref: Some("turn:impl".to_string()),
                dedupe_key: Some("msg-impl".to_string()),
            })
            .expect("receive passive event");

        let scaffold = runtime
            .scaffold_user_task_from_inbox(
                "IN000",
                "Implement requested change",
                "# Implement requested change\n\n## Problem\n\nImplement it.\n\n## Success Criteria\n\n- Change is done.",
                "Solve requested change",
                "# Solve requested change\n\n## Problem Definition\n\nImplement the requested change.\n\n## Proposed Solution\n\nInvestigate, edit, and verify.\n\n## Acceptance Criteria\n\n- Criteria are satisfied.\n\n## Verification Plan\n\nRun focused checks.\n\n## Risks\n\n- Hidden gaps.\n\n## Assumptions\n\n- Request is actionable.",
                "actionable user input scaffolded into ledger-backed task",
            )
            .expect("scaffold user task");

        assert_eq!(scaffold.problem_id, "P001");
        assert_eq!(scaffold.parent_problem_id, "P000");
        assert_eq!(scaffold.ticket_id, "T000");
        assert_eq!(scaffold.inbox_item_id.as_deref(), Some("IN000"));
        assert_eq!(scaffold.passive_event_id.as_deref(), Some("PE000"));
        assert_eq!(scaffold.source_kind, Some(PassiveEventKind::UserInput));

        let problem = &runtime.state().problems["P001"];
        assert_eq!(problem.ticket_id.as_deref(), Some("T000"));
        assert_eq!(
            problem.created_from_passive_event_id.as_deref(),
            Some("PE000")
        );
        assert_eq!(problem.created_from_inbox_item_id.as_deref(), Some("IN000"));
        assert_eq!(
            problem.created_from_user_task_kind,
            Some(PassiveEventKind::UserInput)
        );

        let ticket = &runtime.state().tickets["T000"];
        assert_eq!(ticket.problem_id, "P001");
        assert_eq!(ticket.status, TicketStatus::Defined);
        assert_eq!(ticket.classification, None);
        assert_eq!(ticket.result_id, None);
        assert_eq!(runtime.state().results.len(), 0);
        assert_eq!(runtime.state().checks.len(), 0);
        assert_eq!(runtime.state().inbox[0].status, PassiveEventStatus::Handled);
        assert_eq!(runtime.next().action, NextAction::SpawnThread);
        assert_eq!(runtime.next().problem_id.as_deref(), Some("P001"));
        runtime.validate().expect("valid scaffolded task");
    }

    #[test]
    fn scaffold_user_task_from_inbox_rejects_already_handled_item() {
        let mut runtime = runtime();
        runtime
            .receive_passive_event(PassiveEventInput {
                kind: PassiveEventKind::UserInput,
                event_key: None,
                target_thread_id: Some(DEFAULT_THREAD_ID.to_string()),
                condition: None,
                source: None,
                summary: "user gave a task".to_string(),
                payload_ref: Some("turn:task".to_string()),
                dedupe_key: Some("msg-task".to_string()),
            })
            .expect("receive passive event");
        runtime
            .acknowledge_inbox_item("IN000", "handled conversationally")
            .expect("ack inbox");

        let err = runtime
            .scaffold_user_task_from_inbox(
                "IN000",
                "Duplicate task",
                "# Duplicate task",
                "Duplicate ticket",
                "# Duplicate ticket",
                "duplicate",
            )
            .expect_err("handled inbox item should not scaffold task");

        assert!(err.to_string().contains("not inboxed"));
        assert_eq!(runtime.state().problems.len(), 1);
        assert_eq!(runtime.state().tickets.len(), 0);
        runtime.validate().expect("valid after rejected scaffold");
    }

    #[test]
    fn create_user_task_from_inbox_rejects_already_handled_item() {
        let mut runtime = runtime();
        runtime
            .receive_passive_event(PassiveEventInput {
                kind: PassiveEventKind::UserInput,
                event_key: None,
                target_thread_id: Some(DEFAULT_THREAD_ID.to_string()),
                condition: None,
                source: None,
                summary: "user gave a task".to_string(),
                payload_ref: Some("turn:task".to_string()),
                dedupe_key: Some("msg-task".to_string()),
            })
            .expect("receive passive event");
        runtime
            .acknowledge_inbox_item("IN000", "handled conversationally")
            .expect("ack inbox");
        let before_problem_count = runtime.state().problems.len();

        let err = runtime
            .create_user_task_from_inbox("IN000", "Duplicate task", "# Duplicate", "duplicate")
            .expect_err("handled inbox item should not create task");

        assert!(err.to_string().contains("not inboxed"));
        assert_eq!(runtime.state().problems.len(), before_problem_count);
        assert!(
            runtime.state().problems["P000"]
                .child_problem_ids
                .is_empty()
        );
    }

    #[test]
    fn create_user_task_from_inbox_rejects_unknown_inbox_item() {
        let mut runtime = runtime();
        let before_problem_count = runtime.state().problems.len();

        let err = runtime
            .create_user_task_from_inbox("IN404", "Unknown inbox task", "# Unknown", "unknown")
            .expect_err("unknown inbox item should not create task");

        assert!(err.to_string().contains("unknown inbox item IN404"));
        assert_eq!(runtime.state().problems.len(), before_problem_count);
        assert!(
            runtime.state().problems["P000"]
                .child_problem_ids
                .is_empty()
        );
    }

    #[test]
    fn create_user_task_from_inbox_rejects_mismatched_passive_event() {
        let mut runtime = runtime();
        runtime
            .receive_passive_event(PassiveEventInput {
                kind: PassiveEventKind::UserInput,
                event_key: None,
                target_thread_id: Some(DEFAULT_THREAD_ID.to_string()),
                condition: Some("task".to_string()),
                source: Some("chat".to_string()),
                summary: "user gave a task".to_string(),
                payload_ref: Some("turn:task".to_string()),
                dedupe_key: Some("msg-task".to_string()),
            })
            .expect("receive passive event");
        runtime.state.inbox[0].source = Some("other-channel".to_string());
        let before_problem_count = runtime.state().problems.len();

        let err = runtime
            .create_user_task_from_inbox("IN000", "Mismatched task", "# Mismatched", "mismatch")
            .expect_err("mismatched inbox item should not create task");

        assert!(
            err.to_string()
                .contains("does not match passive event PE000")
        );
        assert_eq!(runtime.state().problems.len(), before_problem_count);
        assert!(
            runtime.state().problems["P000"]
                .child_problem_ids
                .is_empty()
        );
    }

    #[test]
    fn create_user_task_from_inbox_rejects_missing_passive_event() {
        let mut runtime = runtime();
        runtime
            .receive_passive_event(PassiveEventInput {
                kind: PassiveEventKind::UserInput,
                event_key: None,
                target_thread_id: Some(DEFAULT_THREAD_ID.to_string()),
                condition: None,
                source: None,
                summary: "user gave a task".to_string(),
                payload_ref: Some("turn:task".to_string()),
                dedupe_key: Some("msg-task".to_string()),
            })
            .expect("receive passive event");
        runtime.state.inbox[0].passive_event_id = "PE999".to_string();
        let before_problem_count = runtime.state().problems.len();

        let err = runtime
            .create_user_task_from_inbox("IN000", "Missing event task", "# Missing", "missing")
            .expect_err("missing passive event should not create task");

        assert!(err.to_string().contains("unknown passive event PE999"));
        assert_eq!(runtime.state().problems.len(), before_problem_count);
        assert!(
            runtime.state().problems["P000"]
                .child_problem_ids
                .is_empty()
        );
    }

    #[test]
    fn validate_rejects_user_task_missing_passive_event_provenance() {
        let mut runtime = runtime_with_user_task_child();
        runtime
            .state
            .problems
            .get_mut("P001")
            .expect("child")
            .created_from_passive_event_id = None;

        let err = runtime.validate().expect_err("missing passive event");

        assert!(err.to_string().contains("missing passive event"));
    }

    #[test]
    fn validate_rejects_user_task_missing_inbox_item_provenance() {
        let mut runtime = runtime_with_user_task_child();
        runtime
            .state
            .problems
            .get_mut("P001")
            .expect("child")
            .created_from_inbox_item_id = None;

        let err = runtime.validate().expect_err("missing inbox item");

        assert!(err.to_string().contains("missing inbox item"));
    }

    #[test]
    fn validate_rejects_user_task_mismatched_source_kind() {
        let mut runtime = runtime_with_user_task_child();
        runtime
            .state
            .problems
            .get_mut("P001")
            .expect("child")
            .created_from_user_task_kind = Some(PassiveEventKind::Timer);

        let err = runtime.validate().expect_err("mismatched source kind");

        assert!(
            err.to_string()
                .contains("does not match passive event kind")
        );
    }

    #[test]
    fn validate_rejects_user_task_mismatched_inbox_event_pair() {
        let mut runtime = runtime_with_user_task_child();
        runtime.state.inbox[0].passive_event_id = "PE999".to_string();

        let err = runtime.validate().expect_err("mismatched inbox event");

        assert!(err.to_string().contains("points to PE999"));
    }

    #[test]
    fn validate_rejects_unprovenanced_child_problem() {
        let mut runtime = runtime_with_user_task_child();
        let child = runtime.state.problems.get_mut("P001").expect("child");
        child.created_from_passive_event_id = None;
        child.created_from_inbox_item_id = None;
        child.created_from_user_task_kind = None;

        let err = runtime.validate().expect_err("unprovenanced child");

        assert!(
            err.to_string()
                .contains("has no ticket, check, or user-task provenance")
        );
    }

    #[test]
    fn terminal_thread_with_pending_inbox_is_not_reported_as_none() {
        let mut runtime = runtime();
        let ticket = runtime
            .create_ticket("P000", "Root ticket", "# Ticket")
            .expect("ticket");
        runtime
            .classify_ticket(&ticket, TicketClassification::OneGo, "small")
            .expect("classify");
        runtime
            .set_status(EntityKind::Problem, "P000", "doing")
            .expect("problem doing");
        runtime
            .set_status(EntityKind::Ticket, &ticket, "executing")
            .expect("ticket executing");
        let result = runtime
            .record_result(&ticket, "Root result", "# Result")
            .expect("result");
        runtime
            .check(
                "P000",
                CheckStatus::Success,
                vec![result],
                "Root check",
                "# Check",
                None,
            )
            .expect("check");
        assert_eq!(runtime.next().action, NextAction::None);

        runtime
            .receive_passive_event(PassiveEventInput {
                kind: PassiveEventKind::UserInput,
                event_key: None,
                target_thread_id: Some(DEFAULT_THREAD_ID.to_string()),
                condition: None,
                source: None,
                summary: "user returned after terminal root".to_string(),
                payload_ref: Some("turn:terminal".to_string()),
                dedupe_key: Some("terminal-msg".to_string()),
            })
            .expect("receive passive event");

        let next = runtime.next();
        assert_eq!(next.action, NextAction::HandleInbox);
        assert_eq!(next.disposition, NextDisposition::Runnable);
        assert!(
            next.instruction
                .contains("user returned after terminal root")
        );
        runtime.validate().expect("valid");
    }

    #[test]
    fn passive_duplicate_wake_is_idempotent() {
        let mut runtime = runtime();
        let input = PassiveEventInput {
            kind: PassiveEventKind::ExternalSignal,
            event_key: None,
            target_thread_id: Some(DEFAULT_THREAD_ID.to_string()),
            condition: Some("deploy".to_string()),
            source: Some("webhook".to_string()),
            summary: "deploy finished".to_string(),
            payload_ref: None,
            dedupe_key: Some("signal-1".to_string()),
        };

        let first = runtime
            .receive_passive_event(input.clone())
            .expect("first event");
        let second = runtime.receive_passive_event(input).expect("duplicate");

        assert_eq!(first.status, PassiveEventStatus::Inboxed);
        assert_eq!(second.status, PassiveEventStatus::Duplicate);
        assert_eq!(second.wake_decision.status, PassiveEventStatus::Duplicate);
        assert_eq!(
            second.wake_decision.duplicate_of_event_id.as_deref(),
            Some(first.event_id.as_str())
        );
        assert_eq!(
            second.duplicate_of_event_id.as_deref(),
            Some(first.event_id.as_str())
        );
        assert_eq!(runtime.state().passive_events.len(), 1);
        assert_eq!(runtime.state().inbox.len(), 1);
        runtime.validate().expect("valid");
    }

    #[test]
    fn passive_matched_event_wakes_waiting_thread() {
        let mut runtime = runtime();
        let wait = runtime
            .wait_for_user_input(
                Some("reply".to_string()),
                Some("chat".to_string()),
                None,
                "need user reply",
            )
            .expect("wait for input");

        assert_eq!(wait.status, WaitStatus::Waiting);
        assert_eq!(
            wait.event_key,
            EventKey::passive(
                PassiveEventKind::UserInput,
                Some("reply".to_string()),
                Some("chat".to_string()),
                Some(DEFAULT_THREAD_ID.to_string())
            )
        );
        assert_eq!(runtime.next().action, NextAction::WaitIo);
        assert_eq!(
            runtime.state().threads[DEFAULT_THREAD_ID].status,
            ThreadStatus::WaitingIo
        );

        let receipt = runtime
            .receive_passive_event(PassiveEventInput {
                kind: PassiveEventKind::UserInput,
                event_key: None,
                target_thread_id: Some(DEFAULT_THREAD_ID.to_string()),
                condition: Some("reply".to_string()),
                source: Some("chat".to_string()),
                summary: "user replied".to_string(),
                payload_ref: Some("turn:2".to_string()),
                dedupe_key: Some("msg-2".to_string()),
            })
            .expect("receive matching event");

        assert_eq!(receipt.status, PassiveEventStatus::Matched);
        assert_eq!(receipt.matched_handle_ids, vec![wait.handle_id.clone()]);
        assert_eq!(receipt.wake_decision.status, PassiveEventStatus::Matched);
        assert_eq!(
            receipt.wake_decision.runnable_threads,
            vec![RunnableThread {
                thread_id: DEFAULT_THREAD_ID.to_string(),
                root_problem_id: "P000".to_string(),
                wake_reason: format!("handle {} ready", wait.handle_id),
            }]
        );
        assert_eq!(
            runtime.state().handles[&wait.handle_id].status,
            IoHandleStatus::Ready
        );
        assert_eq!(
            runtime.state().waits[&wait.wait_id].status,
            WaitStatus::Ready
        );
        assert_eq!(
            runtime.state().threads[DEFAULT_THREAD_ID].status,
            ThreadStatus::Running
        );
        assert_ne!(runtime.next().action, NextAction::WaitIo);
        runtime.validate().expect("valid");
    }

    #[test]
    fn bare_lifecycle_root_can_wait_for_user_input() {
        let mut runtime = runtime();
        let wait = runtime
            .wait_for_user_input(None, Some("chat".to_string()), None, "idle for user input")
            .expect("bare lifecycle root should be allowed to park");

        assert_eq!(wait.status, WaitStatus::Waiting);
        assert_eq!(runtime.next().action, NextAction::WaitIo);
        assert_eq!(
            runtime.state().threads[DEFAULT_THREAD_ID].status,
            ThreadStatus::WaitingIo
        );
        runtime.validate().expect("valid");
    }

    #[test]
    fn wait_rejects_when_ticket_is_defined() {
        let mut runtime = runtime();
        let ticket_id = runtime
            .create_ticket("P000", "Root ticket", "# Ticket\n\nBody")
            .expect("ticket");

        let err = runtime
            .wait_for_user_input(None, Some("chat".to_string()), None, "idle despite work")
            .expect_err("runnable ticket should block wait");

        assert!(
            err.to_string()
                .contains("cannot wait while runnable action")
                && err.to_string().contains("ClassifyTicket"),
            "unexpected error: {err}"
        );
        assert_eq!(
            runtime.state().threads[DEFAULT_THREAD_ID].status,
            ThreadStatus::Running
        );
        assert_eq!(runtime.next().action, NextAction::ClassifyTicket);
        assert_eq!(
            runtime.next().ticket_id.as_deref(),
            Some(ticket_id.as_str())
        );
        runtime.validate().expect("valid");
    }

    #[test]
    fn next_surfaces_defined_ticket_before_stale_wait_io() {
        let mut runtime = runtime();
        let wait = runtime
            .wait_for_user_input(None, Some("chat".to_string()), None, "idle for user input")
            .expect("bare lifecycle root should be allowed to park");
        assert_eq!(runtime.next().action, NextAction::WaitIo);

        let ticket_id = runtime
            .create_ticket("P000", "Root ticket", "# Ticket\n\nBody")
            .expect("ticket");

        let next = runtime.next();
        assert_eq!(next.action, NextAction::ClassifyTicket);
        assert_eq!(next.ticket_id.as_deref(), Some(ticket_id.as_str()));
        assert_eq!(
            runtime.state().waits[&wait.wait_id].status,
            WaitStatus::Waiting
        );
        runtime.validate().expect("valid");
    }

    #[test]
    fn unified_wait_parks_wakes_and_consumes_passive_handle() {
        let mut runtime = runtime();
        let parked = runtime
            .wait(WaitCommand::Park {
                mode: WaitMode::All,
                reason: "need chat reply".to_string(),
                specs: vec![WaitSpec {
                    kind: WaitSpecKind::UserInput,
                    event_key: None,
                    target_thread_id: None,
                    condition: Some("reply".to_string()),
                    source: Some("chat".to_string()),
                    dedupe_key: Some("wait-reply".to_string()),
                }],
            })
            .expect("park wait");
        let WaitResult::Parked { wait } = parked else {
            panic!("expected parked wait");
        };
        assert_eq!(wait.status, WaitStatus::Waiting);
        let handle_id = wait.handle_ids[0].clone();
        assert_eq!(runtime.next().action, NextAction::WaitIo);

        let woke = runtime
            .wait(WaitCommand::Wake {
                event: PassiveEventInput {
                    kind: PassiveEventKind::UserInput,
                    event_key: None,
                    target_thread_id: Some(DEFAULT_THREAD_ID.to_string()),
                    condition: Some("reply".to_string()),
                    source: Some("chat".to_string()),
                    summary: "user replied".to_string(),
                    payload_ref: Some("turn:42".to_string()),
                    dedupe_key: Some("wait-reply".to_string()),
                },
            })
            .expect("wake wait");
        let WaitResult::Woke { receipt } = woke else {
            panic!("expected wake receipt");
        };
        assert_eq!(receipt.status, PassiveEventStatus::Matched);
        assert_eq!(receipt.matched_handle_ids, vec![handle_id.clone()]);

        let consumed = runtime
            .wait(WaitCommand::Consume {
                handle_id: handle_id.clone(),
            })
            .expect("consume wait");
        let WaitResult::Consumed { consumed } = consumed else {
            panic!("expected consumed wait");
        };
        assert_eq!(consumed.handle_id, handle_id);
        assert_eq!(consumed.payload_ref.as_deref(), Some("turn:42"));
        assert_eq!(consumed.joined_thread, None);
        assert_eq!(
            runtime.state().handles[&consumed.handle_id].status,
            IoHandleStatus::Consumed
        );
        assert_eq!(
            runtime.state().waits[&consumed.wait_id].status,
            WaitStatus::Consumed
        );
        runtime.validate().expect("valid");
    }

    #[test]
    fn unified_wait_cancels_pending_passive_handle() {
        let mut runtime = runtime();
        let parked = runtime
            .wait(WaitCommand::Park {
                mode: WaitMode::All,
                reason: "wait for timer".to_string(),
                specs: vec![WaitSpec {
                    kind: WaitSpecKind::Timer,
                    event_key: None,
                    target_thread_id: None,
                    condition: Some("poll-window".to_string()),
                    source: Some("timer".to_string()),
                    dedupe_key: None,
                }],
            })
            .expect("park wait");
        let WaitResult::Parked { wait } = parked else {
            panic!("expected parked wait");
        };
        let handle_id = wait.handle_ids[0].clone();

        let cancelled = runtime
            .wait(WaitCommand::Cancel {
                handle_id: handle_id.clone(),
                reason: "no longer needed".to_string(),
            })
            .expect("cancel wait");
        let WaitResult::Cancelled { cancelled } = cancelled else {
            panic!("expected cancelled wait");
        };
        assert_eq!(cancelled.handle_id, handle_id);
        assert_eq!(
            runtime.state().handles[&cancelled.handle_id].status,
            IoHandleStatus::Cancelled
        );
        assert_eq!(
            runtime.state().threads[DEFAULT_THREAD_ID].status,
            ThreadStatus::Running
        );
        runtime.validate().expect("valid");
    }

    #[test]
    fn unified_wait_any_consume_cancels_sibling_handles() {
        let mut runtime = runtime();
        let parked = runtime
            .wait(WaitCommand::Park {
                mode: WaitMode::Any,
                reason: "wait for either signal".to_string(),
                specs: vec![
                    WaitSpec {
                        kind: WaitSpecKind::Timer,
                        event_key: None,
                        target_thread_id: None,
                        condition: Some("timer-window".to_string()),
                        source: Some("timer".to_string()),
                        dedupe_key: None,
                    },
                    WaitSpec {
                        kind: WaitSpecKind::ExternalSignal,
                        event_key: None,
                        target_thread_id: None,
                        condition: Some("webhook-ready".to_string()),
                        source: Some("webhook".to_string()),
                        dedupe_key: None,
                    },
                ],
            })
            .expect("park any wait");
        let WaitResult::Parked { wait } = parked else {
            panic!("expected parked wait");
        };
        let timer_handle = wait.handle_ids[0].clone();
        let webhook_handle = wait.handle_ids[1].clone();
        runtime
            .wait(WaitCommand::Wake {
                event: PassiveEventInput {
                    kind: PassiveEventKind::Timer,
                    event_key: None,
                    target_thread_id: Some(DEFAULT_THREAD_ID.to_string()),
                    condition: Some("timer-window".to_string()),
                    source: Some("timer".to_string()),
                    summary: "timer fired".to_string(),
                    payload_ref: None,
                    dedupe_key: None,
                },
            })
            .expect("wake timer handle");

        runtime
            .wait(WaitCommand::Consume {
                handle_id: timer_handle.clone(),
            })
            .expect("consume any winner");

        assert_eq!(
            runtime.state().handles[&timer_handle].status,
            IoHandleStatus::Consumed
        );
        assert_eq!(
            runtime.state().handles[&webhook_handle].status,
            IoHandleStatus::Cancelled
        );
        assert_eq!(
            runtime.state().waits[&wait.wait_id].status,
            WaitStatus::Consumed
        );
        runtime.validate().expect("valid");
    }

    #[test]
    fn passive_generic_event_key_wait_matches_exact_key() {
        let mut runtime = runtime();
        let event_key = EventKey::new(
            "github.pr.checks",
            Some("123".to_string()),
            Some("webhook".to_string()),
            None,
        );
        let wait = runtime
            .wait_for_event_key(
                event_key.clone(),
                Some("github-pr-123-checks".to_string()),
                "wait for PR checks",
            )
            .expect("wait for event key");

        assert_eq!(
            runtime.state().handles[&wait.handle_id].event_key.as_ref(),
            Some(&event_key)
        );

        let receipt = runtime
            .receive_passive_event(PassiveEventInput {
                kind: PassiveEventKind::ExternalSignal,
                event_key: Some(event_key.clone()),
                target_thread_id: None,
                condition: None,
                source: None,
                summary: "PR checks passed".to_string(),
                payload_ref: Some("github:pr:123".to_string()),
                dedupe_key: Some("github-pr-123-checks".to_string()),
            })
            .expect("receive matching event key");

        assert_eq!(receipt.status, PassiveEventStatus::Matched);
        assert_eq!(receipt.matched_handle_ids, vec![wait.handle_id.clone()]);
        assert_eq!(receipt.wake_decision.event_key, event_key);
        assert_eq!(
            receipt.wake_decision.runnable_threads[0].thread_id,
            DEFAULT_THREAD_ID
        );
        assert_eq!(
            runtime.state().handles[&wait.handle_id].status,
            IoHandleStatus::Ready
        );
        runtime.validate().expect("valid");
    }

    #[test]
    fn unified_wait_consumes_child_thread_handle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root_context = RuntimeContext::new(dir.path());
        let mut runtime =
            QunuxRuntime::load_or_init(root_context.clone(), "Root task", "# Root task")
                .expect("init");
        let parent_ticket = runtime
            .create_ticket("P000", "Parent ticket", "# Parent ticket")
            .expect("ticket");
        runtime
            .classify_ticket(&parent_ticket, TicketClassification::Split, "needs child")
            .expect("classify");
        runtime
            .set_status(EntityKind::Ticket, &parent_ticket, "splitting")
            .expect("splitting");
        let child_id = runtime
            .create_problem_from_ticket("P000", &parent_ticket, "Child", "# Child")
            .expect("child");
        let spawned = runtime
            .spawn_thread(
                &child_id,
                ContextForkPolicy::FullContext,
                "Solve child",
                None,
                None,
                Vec::new(),
            )
            .expect("spawn");

        let child_context =
            RuntimeContext::with_ids(dir.path(), DEFAULT_PROCESS_ID, &spawned.thread_id)
                .expect("child context");
        let mut child_runtime = QunuxRuntime::load(child_context).expect("child load");
        solve_one_go(&mut child_runtime, &child_id);

        let mut parent_runtime = QunuxRuntime::load(root_context).expect("parent reload");
        let consumed = parent_runtime
            .wait(WaitCommand::Consume {
                handle_id: spawned.handle_id.clone(),
            })
            .expect("consume child wait");
        let WaitResult::Consumed { consumed } = consumed else {
            panic!("expected consumed child wait");
        };
        let joined = consumed.joined_thread.expect("joined child thread");
        assert_eq!(joined.thread_id, spawned.thread_id);
        assert_eq!(joined.handle_id, spawned.handle_id);
        assert_eq!(
            parent_runtime.state().threads[&spawned.thread_id]
                .joined_at
                .is_some(),
            true
        );
        assert_eq!(
            parent_runtime.state().handles[&spawned.handle_id].status,
            IoHandleStatus::Consumed
        );
        parent_runtime.validate().expect("valid");
    }

    #[test]
    fn passive_broad_event_does_not_wake_child_scoped_wait() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root_context = RuntimeContext::new(dir.path());
        let mut runtime =
            QunuxRuntime::load_or_init(root_context.clone(), "Root task", "# Root task")
                .expect("init");
        let parent_ticket = runtime
            .create_ticket("P000", "Parent ticket", "# Parent ticket")
            .expect("ticket");
        runtime
            .classify_ticket(&parent_ticket, TicketClassification::Split, "needs child")
            .expect("classify");
        runtime
            .set_status(EntityKind::Ticket, &parent_ticket, "splitting")
            .expect("splitting");
        let child_id = runtime
            .create_problem_from_ticket("P000", &parent_ticket, "Child", "# Child")
            .expect("child");
        let spawned = runtime
            .spawn_thread(
                &child_id,
                ContextForkPolicy::FullContext,
                "Solve child",
                None,
                None,
                Vec::new(),
            )
            .expect("spawn");
        let child_context =
            RuntimeContext::with_ids(dir.path(), DEFAULT_PROCESS_ID, &spawned.thread_id)
                .expect("child context");
        let mut child_runtime = QunuxRuntime::load(child_context).expect("child load");
        let child_ticket = child_runtime
            .create_ticket(&child_id, "Child ticket", "# Child ticket")
            .expect("child ticket");
        child_runtime
            .classify_ticket(&child_ticket, TicketClassification::OneGo, "wait for reply")
            .expect("child classify");
        child_runtime
            .set_status(EntityKind::Problem, &child_id, "doing")
            .expect("child doing");
        child_runtime
            .set_status(EntityKind::Ticket, &child_ticket, "executing")
            .expect("child executing");
        let wait = child_runtime
            .wait_for_user_input(
                Some("reply".to_string()),
                Some("chat".to_string()),
                None,
                "child needs addressed reply",
            )
            .expect("child wait");

        assert_eq!(
            wait.event_key.target_thread_id.as_deref(),
            Some(spawned.thread_id.as_str())
        );

        let mut root_runtime = QunuxRuntime::load(root_context.clone()).expect("root reload");
        let broad = root_runtime
            .receive_passive_event(PassiveEventInput {
                kind: PassiveEventKind::UserInput,
                event_key: None,
                target_thread_id: None,
                condition: Some("reply".to_string()),
                source: Some("chat".to_string()),
                summary: "broad user reply".to_string(),
                payload_ref: Some("turn:broad".to_string()),
                dedupe_key: Some("broad-reply".to_string()),
            })
            .expect("broad event");

        assert_eq!(broad.status, PassiveEventStatus::Inboxed);
        assert!(broad.matched_handle_ids.is_empty());
        assert!(broad.wake_decision.runnable_threads.is_empty());
        assert_eq!(
            root_runtime.state().handles[&wait.handle_id].status,
            IoHandleStatus::Pending
        );
        assert_eq!(
            root_runtime.state().threads[&spawned.thread_id].status,
            ThreadStatus::WaitingIo
        );

        let targeted = root_runtime
            .receive_passive_event(PassiveEventInput {
                kind: PassiveEventKind::UserInput,
                event_key: None,
                target_thread_id: Some(spawned.thread_id.clone()),
                condition: Some("reply".to_string()),
                source: Some("chat".to_string()),
                summary: "addressed child reply".to_string(),
                payload_ref: Some("turn:child".to_string()),
                dedupe_key: Some("child-reply".to_string()),
            })
            .expect("targeted event");

        assert_eq!(targeted.status, PassiveEventStatus::Matched);
        assert_eq!(targeted.matched_handle_ids, vec![wait.handle_id.clone()]);
        assert_eq!(
            targeted.wake_decision.runnable_threads,
            vec![RunnableThread {
                thread_id: spawned.thread_id.clone(),
                root_problem_id: child_id.clone(),
                wake_reason: format!("handle {} ready", wait.handle_id),
            }]
        );
        assert_eq!(
            root_runtime.state().handles[&wait.handle_id].status,
            IoHandleStatus::Ready
        );
        assert_eq!(
            root_runtime.state().threads[&spawned.thread_id].status,
            ThreadStatus::Running
        );
        root_runtime.validate().expect("valid");
    }

    #[test]
    fn passive_lost_wake_matches_new_wait_from_inbox() {
        let mut runtime = runtime();
        runtime
            .receive_passive_event(PassiveEventInput {
                kind: PassiveEventKind::Timer,
                event_key: None,
                target_thread_id: Some(DEFAULT_THREAD_ID.to_string()),
                condition: Some("poll-window".to_string()),
                source: Some("timer".to_string()),
                summary: "poll window opened".to_string(),
                payload_ref: Some("timer:poll-window".to_string()),
                dedupe_key: Some("timer-1".to_string()),
            })
            .expect("early timer event");

        let wait = runtime
            .wait_for_timer(
                Some("poll-window".to_string()),
                Some("timer".to_string()),
                Some("timer-1".to_string()),
                "wait for poll window",
            )
            .expect("timer wait");

        assert_eq!(wait.status, WaitStatus::Ready);
        assert_eq!(
            runtime.state().passive_events[0].status,
            PassiveEventStatus::Matched
        );
        assert_eq!(runtime.state().inbox[0].status, PassiveEventStatus::Matched);
        assert_eq!(
            runtime.state().handles[&wait.handle_id].status,
            IoHandleStatus::Ready
        );
        assert_eq!(
            runtime.state().handles[&wait.handle_id]
                .payload_ref
                .as_deref(),
            Some("timer:poll-window")
        );
        assert_eq!(
            runtime.state().threads[DEFAULT_THREAD_ID].status,
            ThreadStatus::Running
        );
        runtime.validate().expect("valid");
    }
}
