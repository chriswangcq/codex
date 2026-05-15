use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use thiserror::Error;

pub const SCHEMA_VERSION: u32 = 4;
pub const DEFAULT_PROCESS_ID: &str = "QP000";
pub const DEFAULT_THREAD_ID: &str = "QT000";

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
            NextAction::WaitThread => Self::IoWait,
            NextAction::None => Self::Terminal,
            NextAction::CreateSolutionTicket
            | NextAction::DefineTicket
            | NextAction::ClassifyTicket
            | NextAction::ExecuteTicket
            | NextAction::SplitTicket
            | NextAction::SpawnThread
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
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IoHandleKind {
    ChildThread,
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
    HandleReady,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IoHandle {
    pub id: String,
    pub kind: IoHandleKind,
    pub owner_thread_id: String,
    pub target_thread_id: Option<String>,
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
pub struct Problem {
    pub id: String,
    pub title: String,
    pub body: String,
    pub status: ProblemStatus,
    pub owner_thread_id: String,
    pub parent_id: Option<String>,
    pub created_from_ticket_id: Option<String>,
    pub created_from_check_id: Option<String>,
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
                created_from_check_id: None,
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
        let path = context.state_path();
        if path.exists() {
            return Self::load(context);
        }

        let state = ClosureState::new_root(
            context.process_id.clone(),
            context.thread_id.clone(),
            context.actor_session_id.clone(),
            title.into(),
            body.into(),
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
        let state: ClosureState =
            serde_json::from_str(&raw).map_err(|source| QunuxError::Json {
                path: path.clone(),
                source,
            })?;
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

    pub fn create_problem_from_ticket(
        &mut self,
        parent_id: impl AsRef<str>,
        ticket_id: impl AsRef<str>,
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
        if ticket.classification != Some(TicketClassification::Split) {
            return Err(QunuxError::InvalidState(format!(
                "ticket {ticket_id} is not classified as split"
            )));
        }
        if ticket.status != TicketStatus::Splitting {
            return Err(QunuxError::InvalidState(format!(
                "ticket {ticket_id} must be splitting before creating child problems"
            )));
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
            created_from_check_id: None,
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
            format!("created from split ticket {ticket_id}"),
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
        if active_status == TicketStatus::Splitting {
            let children = self.child_problem_ids_from_ticket(ticket_id);
            if children.is_empty() {
                return Err(QunuxError::InvalidState(format!(
                    "cannot finish split ticket {ticket_id}; create at least one child problem first"
                )));
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
                    "cannot finish split ticket {ticket_id}; child problems still open: {}",
                    open_children.join(", ")
                )));
            }
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
                created_from_check_id: Some(check_id.clone()),
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
        let root_problem_id = self
            .current_thread()
            .map(|thread| thread.root_problem_id.clone())
            .unwrap_or_else(|_| self.state.root_problem_id.clone());
        self.next_for_problem(&root_problem_id)
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
                        ThreadStatus::Done | ThreadStatus::Failed | ThreadStatus::Cancelled
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
                        "Join the completed child thread before summarizing the parent split ticket.",
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
                        "Recover the failed child thread before the parent split ticket can be summarized.",
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
                    "split child problem is not assigned to a child thread",
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
                    "Execute the ticket, then record the actual result.",
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
                "Record the result for the current executing ticket.",
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
                    "Record the parent ticket summary result after all split children are done.",
                    "split children are closed",
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

    fn require_handle(&self, handle_id: &str) -> Result<&IoHandle> {
        self.state
            .handles
            .get(handle_id)
            .ok_or_else(|| QunuxError::InvalidState(format!("unknown handle {handle_id}")))
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
        self.state
            .threads
            .values()
            .find(|thread| thread.root_problem_id == problem_id)
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
                            ThreadStatus::Done | ThreadStatus::Failed | ThreadStatus::Cancelled
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
}
