use crate::agent::control::SpawnAgentForkMode;
use crate::agent::control::SpawnAgentOptions;
use crate::agent::exceeds_thread_spawn_depth_limit;
use crate::agent::next_thread_spawn_depth;
use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::multi_agents_common::apply_spawn_agent_overrides;
use crate::tools::handlers::multi_agents_common::build_agent_spawn_config;
use crate::tools::handlers::multi_agents_common::collab_spawn_error;
use crate::tools::handlers::multi_agents_common::thread_spawn_source;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::qunux_spec::QUNUX_NAMESPACE;
use crate::tools::handlers::qunux_spec::create_qunux_tool;
use crate::tools::registry::ToolHandler;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use codex_qunux::CheckStatus;
use codex_qunux::ContextForkPolicy;
use codex_qunux::DEFAULT_ROOT_PROBLEM_BODY;
use codex_qunux::DEFAULT_ROOT_PROBLEM_TITLE;
use codex_qunux::EntityKind;
use codex_qunux::QunuxRuntime;
use codex_qunux::RuntimeContext;
use codex_qunux::SpawnedThread;
use codex_qunux::TicketChildMode;
use codex_qunux::TicketClassification;
use codex_qunux::WaitCommand;
use codex_qunux::WaitMode;
use codex_qunux::WaitResult;
use codex_qunux::WaitSpec;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QunuxOperation {
    Current,
    Next,
    CreateProblem,
    CreateTicket,
    ClassifyTicket,
    SetStatus,
    Result,
    Check,
    SpawnThread,
    Wait,
    AckInbox,
    IngestUserTask,
    ScaffoldUserTask,
    JoinThread,
    RecoverThread,
    ListThreads,
    ThreadStatus,
    Status,
    Validate,
    Render,
}

impl QunuxOperation {
    pub fn all() -> [Self; 20] {
        [
            Self::Current,
            Self::Next,
            Self::CreateProblem,
            Self::CreateTicket,
            Self::ClassifyTicket,
            Self::SetStatus,
            Self::Result,
            Self::Check,
            Self::SpawnThread,
            Self::Wait,
            Self::AckInbox,
            Self::IngestUserTask,
            Self::ScaffoldUserTask,
            Self::JoinThread,
            Self::RecoverThread,
            Self::ListThreads,
            Self::ThreadStatus,
            Self::Status,
            Self::Validate,
            Self::Render,
        ]
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Next => "next",
            Self::CreateProblem => "create_problem",
            Self::CreateTicket => "create_ticket",
            Self::ClassifyTicket => "classify_ticket",
            Self::SetStatus => "set_status",
            Self::Result => "result",
            Self::Check => "check",
            Self::SpawnThread => "spawn_thread",
            Self::Wait => "wait",
            Self::AckInbox => "ack_inbox",
            Self::IngestUserTask => "ingest_user_task",
            Self::ScaffoldUserTask => "scaffold_user_task",
            Self::JoinThread => "join_thread",
            Self::RecoverThread => "recover_thread",
            Self::ListThreads => "list_threads",
            Self::ThreadStatus => "thread_status",
            Self::Status => "status",
            Self::Validate => "validate",
            Self::Render => "render",
        }
    }
}

pub struct QunuxHandler {
    operation: QunuxOperation,
}

impl QunuxHandler {
    pub fn new(operation: QunuxOperation) -> Self {
        Self { operation }
    }
}

impl ToolHandler for QunuxHandler {
    type Output = FunctionToolOutput;

    fn tool_name(&self) -> ToolName {
        ToolName::namespaced(QUNUX_NAMESPACE, self.operation.name())
    }

    fn spec(&self) -> Option<ToolSpec> {
        Some(create_qunux_tool(self.operation))
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<Self::Output, FunctionCallError> {
        let ToolPayload::Function { arguments } = invocation.payload.clone() else {
            return Err(FunctionCallError::RespondToModel(
                "qunux handler received unsupported payload".to_string(),
            ));
        };

        let mut runtime = runtime_for_invocation(&invocation, None, None)?;
        let (value, message) = match self.operation {
            QunuxOperation::Current => {
                let args: CurrentArgs = parse_arguments(&arguments)?;
                runtime = runtime_for_invocation(&invocation, args.title, args.body)?;
                let current = runtime.current();
                let message = format!(
                    "Qunux: process {} thread {} next {:?}/{:?}",
                    current.context.process_id,
                    current.context.thread_id,
                    current.next.disposition,
                    current.next.action
                );
                tool_value(current, message)
            }
            QunuxOperation::Next => {
                let next = runtime.next();
                let message = format!(
                    "Qunux: next {:?}/{:?} for thread {}",
                    next.disposition, next.action, next.thread_id
                );
                tool_value(next, message)
            }
            QunuxOperation::CreateProblem => {
                let args: CreateProblemArgs = parse_arguments(&arguments)?;
                let id = runtime
                    .create_problem_from_ticket_with_mode(
                        args.parent_id,
                        args.from_ticket_id,
                        args.mode,
                        args.title,
                        args.body,
                    )
                    .map_err(qunux_error)?;
                let message = format!("Qunux: created child problem {id}");
                tool_value(IdResponse { id }, message)
            }
            QunuxOperation::CreateTicket => {
                let args: CreateTicketArgs = parse_arguments(&arguments)?;
                let problem_id = args.problem_id.clone();
                let id = runtime
                    .create_ticket(args.problem_id, args.title, args.body)
                    .map_err(qunux_error)?;
                let message = format!("Qunux: created ticket {id} for problem {problem_id}");
                tool_value(IdResponse { id }, message)
            }
            QunuxOperation::ClassifyTicket => {
                let args: ClassifyTicketArgs = parse_arguments(&arguments)?;
                let ticket_id = args.ticket_id.clone();
                let classification = args.classification;
                runtime
                    .classify_ticket(args.ticket_id, args.classification, args.reason)
                    .map_err(qunux_error)?;
                tool_value(
                    OkResponse::ok(),
                    format!("Qunux: classified ticket {ticket_id} as {classification:?}"),
                )
            }
            QunuxOperation::SetStatus => {
                let args: SetStatusArgs = parse_arguments(&arguments)?;
                let entity_id = args.id.clone();
                let status = args.status.clone();
                runtime
                    .set_status(args.kind, args.id, args.status)
                    .map_err(qunux_error)?;
                tool_value(OkResponse::ok(), format!("Qunux: set {entity_id} to {status}"))
            }
            QunuxOperation::Result => {
                let args: ResultArgs = parse_arguments(&arguments)?;
                let ticket_id = args.ticket_id.clone();
                let id = runtime
                    .record_result(args.ticket_id, args.title, args.body)
                    .map_err(qunux_error)?;
                let message = format!("Qunux: recorded result {id} for ticket {ticket_id}");
                tool_value(IdResponse { id }, message)
            }
            QunuxOperation::Check => {
                let args: CheckArgs = parse_arguments(&arguments)?;
                let problem_id = args.problem_id.clone();
                let status = args.status;
                let followup = match (args.followup_title, args.followup_body) {
                    (Some(title), Some(body)) => Some((title, body)),
                    (None, None) => None,
                    _ => {
                        return Err(FunctionCallError::RespondToModel(
                            "followup_title and followup_body must be provided together"
                                .to_string(),
                        ));
                    }
                };
                let id = runtime
                    .check(
                        args.problem_id,
                        args.status,
                        args.result_ids,
                        args.title,
                        args.body,
                        followup,
                    )
                    .map_err(qunux_error)?;
                let message = format!("Qunux: recorded {status:?} check {id} for {problem_id}");
                tool_value(IdResponse { id }, message)
            }
            QunuxOperation::SpawnThread => {
                let args: SpawnThreadArgs = parse_arguments(&arguments)?;
                let inherited_tools = vec![QUNUX_NAMESPACE.to_string()];
                let spawned = runtime
                    .spawn_thread(
                        args.problem_id,
                        args.context_policy
                            .unwrap_or(ContextForkPolicy::FullContext),
                        args.bootstrap_instruction
                            .unwrap_or_else(default_thread_bootstrap),
                        Some(invocation.turn.cwd.to_string_lossy().to_string()),
                        Some(invocation.turn.model_info.slug.clone()),
                        inherited_tools,
                    )
                    .map_err(qunux_error)?;
                let parent_context = runtime.context().clone();
                let codex_agent_id =
                    match spawn_codex_child_agent(&invocation, parent_context, &spawned, args.agent_type)
                        .await
                    {
                        Ok(codex_agent_id) => codex_agent_id,
                        Err(spawn_error) => {
                            let message = format!(
                                "Codex child agent spawn failed after Qunux thread {} was created: {spawn_error}",
                                spawned.thread_id
                            );
                            if let Err(recovery_error) =
                                runtime.record_child_thread_spawn_failed(&spawned.thread_id, message)
                            {
                                return Err(FunctionCallError::RespondToModel(format!(
                                    "{spawn_error}; additionally failed to record Qunux spawn-failure recovery state: {recovery_error}"
                                )));
                            }
                            return Err(spawn_error);
                        }
                    };
                runtime
                    .bind_thread_actor(&spawned.thread_id, codex_agent_id.to_string())
                    .map_err(qunux_error)?;
                let message = format!(
                    "Qunux: spawned thread {} for {} with Codex agent {}",
                    spawned.thread_id, spawned.root_problem_id, codex_agent_id
                );
                tool_value(
                    SpawnThreadResponse {
                        thread_id: spawned.thread_id,
                        root_problem_id: spawned.root_problem_id,
                        handle_id: spawned.handle_id,
                        wait_id: spawned.wait_id,
                        codex_agent_id: Some(codex_agent_id),
                        bootstrap_instruction: spawned.bootstrap_instruction,
                    },
                    message,
                )
            }
            QunuxOperation::Wait => {
                let args: WaitArgs = parse_arguments(&arguments)?;
                let result = runtime
                    .wait(WaitCommand::Park {
                        mode: args.mode,
                        reason: args.reason,
                        specs: args.specs,
                    })
                    .map_err(qunux_error)?;
                let WaitResult::Parked { wait } = result else {
                    return Err(FunctionCallError::Fatal(
                        "Qunux wait handler only supports park results".to_string(),
                    ));
                };
                let message = "Qunux: current thread parked on wait handle".to_string();
                tool_value(ParkedWaitResponse { wait }, message)
            }
            QunuxOperation::AckInbox => {
                let args: AckInboxArgs = parse_arguments(&arguments)?;
                let item = runtime
                    .acknowledge_inbox_item(args.inbox_item_id, args.note)
                    .map_err(qunux_error)?;
                let message = format!("Qunux: acknowledged inbox item {}", item.id);
                tool_value(item, message)
            }
            QunuxOperation::IngestUserTask => {
                let args: IngestUserTaskArgs = parse_arguments(&arguments)?;
                let inbox_item_id = args.inbox_item_id.clone();
                let created = runtime
                    .create_user_task_from_inbox(
                        args.inbox_item_id,
                        args.title,
                        args.body,
                        "actionable inbox item converted to a Qunux child problem",
                    )
                    .map_err(qunux_error)?;
                let message = format!(
                    "Qunux: converted inbox item {inbox_item_id} into user task {}",
                    created.problem_id
                );
                tool_value(created, message)
            }
            QunuxOperation::ScaffoldUserTask => {
                let args: ScaffoldUserTaskArgs = parse_arguments(&arguments)?;
                let inbox_item_id = args.inbox_item_id.clone();
                let scaffolded = runtime
                    .scaffold_user_task_from_inbox(
                        args.inbox_item_id,
                        args.problem_title,
                        args.problem_body,
                        args.ticket_title,
                        args.ticket_body,
                        args.note,
                    )
                    .map_err(qunux_error)?;
                let message = format!(
                    "Qunux: scaffolded inbox item {inbox_item_id} into problem {} and ticket {}",
                    scaffolded.problem_id, scaffolded.ticket_id
                );
                tool_value(scaffolded, message)
            }
            QunuxOperation::JoinThread => {
                let args: JoinThreadArgs = parse_arguments(&arguments)?;
                let joined = runtime
                    .join_thread(args.target_thread_id)
                    .map_err(qunux_error)?;
                let message = format!(
                    "Qunux: joined child thread {} back into {}",
                    joined.thread_id, joined.parent_thread_id
                );
                tool_value(joined, message)
            }
            QunuxOperation::RecoverThread => {
                let args: RecoverThreadArgs = parse_arguments(&arguments)?;
                let recovered = runtime
                    .recover_thread(args.target_thread_id)
                    .map_err(qunux_error)?;
                let message = format!(
                    "Qunux: recovered failed child thread {} and returned {} to {}",
                    recovered.thread_id, recovered.root_problem_id, recovered.parent_thread_id
                );
                tool_value(recovered, message)
            }
            QunuxOperation::ListThreads => tool_value(
                runtime.list_threads(),
                "Qunux: listed process threads".to_string(),
            ),
            QunuxOperation::ThreadStatus => {
                let args: ThreadStatusArgs = parse_arguments(&arguments)?;
                let thread = runtime
                    .thread_status(args.target_thread_id)
                    .map_err(qunux_error)?;
                let message = format!("Qunux: thread {} is {:?}", thread.id, thread.status);
                tool_value(thread, message)
            }
            QunuxOperation::Status => {
                let status = runtime.status();
                let message = format!(
                    "Qunux: process {} thread {} has {} open problems and {} waiting threads",
                    status.process_id, status.thread_id, status.open_problems, status.waiting_threads
                );
                tool_value(status, message)
            }
            QunuxOperation::Validate => {
                runtime.validate().map_err(qunux_error)?;
                tool_value(OkResponse::ok(), "Qunux: state is valid".to_string())
            }
            QunuxOperation::Render => tool_value(
                RenderResponse {
                    markdown: runtime.render(),
                },
                "Qunux: rendered process task tree".to_string(),
            ),
        }
        .map_err(|err| FunctionCallError::Fatal(err.to_string()))?;
        let value = attach_message(value, message);
        let text = serde_json::to_string_pretty(&value)
            .map_err(|err| FunctionCallError::Fatal(err.to_string()))?;
        Ok(FunctionToolOutput::from_text(text, Some(true)))
    }
}

fn runtime_for_invocation(
    invocation: &ToolInvocation,
    title: Option<String>,
    body: Option<String>,
) -> Result<QunuxRuntime, FunctionCallError> {
    let context = if let Some(context) = invocation.session.services.qunux_runtime_context.clone() {
        context
    } else {
        let actor_session_id = invocation.session.thread_id().to_string();
        if matches!(
            &invocation.turn.session_source,
            codex_protocol::protocol::SessionSource::SubAgent(_)
        ) {
            return Err(FunctionCallError::RespondToModel(
                "Qunux subagent session is not bound to a Qunux thread. Create Qunux child work with qunux.spawn_thread so Codex can inject process/thread identity.".to_string(),
            ));
        }
        RuntimeContext::for_session(invocation.turn.cwd.to_path_buf(), actor_session_id)
            .map_err(qunux_error)?
    };
    let mut runtime = QunuxRuntime::load_or_init(
        context,
        title
            .clone()
            .unwrap_or_else(|| DEFAULT_ROOT_PROBLEM_TITLE.to_string()),
        body.clone()
            .unwrap_or_else(|| DEFAULT_ROOT_PROBLEM_BODY.to_string()),
    )
    .map_err(qunux_error)?;
    runtime
        .initialize_root_problem(title, body)
        .map_err(qunux_error)?;
    Ok(runtime)
}

async fn spawn_codex_child_agent(
    invocation: &ToolInvocation,
    parent_context: RuntimeContext,
    spawned: &SpawnedThread,
    agent_type: Option<String>,
) -> Result<String, FunctionCallError> {
    let child_depth = next_thread_spawn_depth(&invocation.turn.session_source);
    if exceeds_thread_spawn_depth_limit(child_depth, invocation.turn.config.agent_max_depth) {
        return Err(FunctionCallError::RespondToModel(
            "Qunux child thread spawn would exceed the configured Codex agent depth limit"
                .to_string(),
        ));
    }

    let mut config = build_agent_spawn_config(
        &invocation.session.get_base_instructions().await,
        invocation.turn.as_ref(),
    )?;
    apply_spawn_agent_overrides(&mut config, child_depth);

    let message = child_thread_initial_message(spawned);
    let initial_operation: Op = vec![UserInput::Text {
        text: message,
        text_elements: Vec::new(),
    }]
    .into();
    let session_source = thread_spawn_source(
        invocation.session.thread_id(),
        &invocation.turn.session_source,
        child_depth,
        agent_type.as_deref(),
        Some(format!("qunux_{}", spawned.thread_id.to_ascii_lowercase())),
    )?;
    let mut qunux_runtime_context = RuntimeContext::with_ids(
        parent_context.workspace_root,
        parent_context.process_id,
        spawned.thread_id.clone(),
    )
    .map_err(qunux_error)?;
    if let Some(parent_actor_session_id) = parent_context.actor_session_id {
        qunux_runtime_context = qunux_runtime_context
            .with_parent_actor_session_id(parent_actor_session_id)
            .map_err(qunux_error)?;
    }
    let child = Box::pin(
        invocation
            .session
            .services
            .agent_control
            .spawn_agent_with_metadata(
                config,
                initial_operation,
                Some(session_source),
                SpawnAgentOptions {
                    fork_parent_spawn_call_id: Some(invocation.call_id.clone()),
                    fork_mode: Some(SpawnAgentForkMode::FullHistory),
                    environments: Some(invocation.turn.environments.to_selections()),
                    qunux_runtime_context: Some(qunux_runtime_context),
                },
            ),
    )
    .await
    .map_err(collab_spawn_error)?;
    Ok(child.thread_id.to_string())
}

fn child_thread_initial_message(spawned: &SpawnedThread) -> String {
    format!(
        "You are a Qunux child thread.\n\nQunux thread id: {}\nBound root problem: {}\n\nRules:\n- Work only inside this Qunux thread subtree.\n- Start by calling qunux.current, then qunux.next.\n- Do exactly the current next action and let Qunux enforce closure.\n- If this is a watcher child thread for a fuzzy or semantic wait, inspect the requested signals, judge the criteria, record evidence, and close only when the criteria are met or the bootstrap escalation rule says to stop. Do not create a semantic wait primitive.\n\nBootstrap:\n{}",
        spawned.thread_id, spawned.root_problem_id, spawned.bootstrap_instruction
    )
}

fn default_thread_bootstrap() -> String {
    "Solve the bound Qunux subtree to closure. Do not touch parent or sibling subtrees.".to_string()
}

fn qunux_error(error: codex_qunux::QunuxError) -> FunctionCallError {
    FunctionCallError::RespondToModel(error.to_string())
}

fn tool_value<T: Serialize>(value: T, message: String) -> serde_json::Result<(JsonValue, String)> {
    serde_json::to_value(value).map(|value| (value, message))
}

fn attach_message(value: JsonValue, message: String) -> JsonValue {
    match value {
        JsonValue::Object(mut object) => {
            object.insert("message".to_string(), JsonValue::String(message));
            JsonValue::Object(object)
        }
        data => serde_json::json!({
            "message": message,
            "data": data,
        }),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct CurrentArgs {
    title: Option<String>,
    body: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct CreateProblemArgs {
    parent_id: String,
    from_ticket_id: String,
    mode: TicketChildMode,
    title: String,
    body: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct CreateTicketArgs {
    problem_id: String,
    title: String,
    body: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct ClassifyTicketArgs {
    ticket_id: String,
    classification: TicketClassification,
    reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct SetStatusArgs {
    kind: EntityKind,
    id: String,
    status: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct ResultArgs {
    ticket_id: String,
    title: String,
    body: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct CheckArgs {
    problem_id: String,
    status: CheckStatus,
    result_ids: Vec<String>,
    title: String,
    body: String,
    followup_title: Option<String>,
    followup_body: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
struct SpawnThreadArgs {
    problem_id: String,
    context_policy: Option<ContextForkPolicy>,
    bootstrap_instruction: Option<String>,
    agent_type: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
struct WaitArgs {
    mode: WaitMode,
    reason: String,
    specs: Vec<WaitSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct AckInboxArgs {
    inbox_item_id: String,
    note: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct IngestUserTaskArgs {
    inbox_item_id: String,
    title: String,
    body: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct ScaffoldUserTaskArgs {
    inbox_item_id: String,
    problem_title: String,
    problem_body: String,
    ticket_title: String,
    ticket_body: String,
    note: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct JoinThreadArgs {
    target_thread_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct RecoverThreadArgs {
    target_thread_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct ThreadStatusArgs {
    target_thread_id: String,
}

#[derive(Debug, Serialize)]
struct IdResponse {
    id: String,
}

#[derive(Debug, Serialize)]
struct OkResponse {
    ok: bool,
}

#[derive(Debug, Serialize)]
struct SpawnThreadResponse {
    thread_id: String,
    root_problem_id: String,
    handle_id: String,
    wait_id: String,
    codex_agent_id: Option<String>,
    bootstrap_instruction: String,
}

#[derive(Debug, Serialize)]
struct ParkedWaitResponse {
    wait: codex_qunux::ParkedWait,
}

impl OkResponse {
    fn ok() -> Self {
        Self { ok: true }
    }
}

#[derive(Debug, Serialize)]
struct RenderResponse {
    markdown: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::tests::make_session_and_context;
    use crate::tools::context::ToolCallSource;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_protocol::ThreadId;
    use codex_qunux::DEFAULT_THREAD_ID;
    use codex_qunux::IoEventKind;
    use codex_qunux::IoHandleStatus;
    use codex_qunux::NextAction;
    use codex_qunux::NextDisposition;
    use codex_qunux::PassiveEventInput;
    use codex_qunux::PassiveEventKind;
    use codex_qunux::ThreadStatus;
    use codex_qunux::process_id_for_session_id;
    use core_test_support::TempDirExt;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn child_thread_bootstrap_preserves_watcher_guidance() {
        let spawned = SpawnedThread {
            thread_id: "QT001".to_string(),
            root_problem_id: "P001".to_string(),
            handle_id: "H000".to_string(),
            wait_id: "W000".to_string(),
            bootstrap_instruction: "Watcher goal: inspect CI every 30 minutes; close only when CI is green with evidence.".to_string(),
        };

        let message = child_thread_initial_message(&spawned);

        assert!(message.contains("Qunux thread id: QT001"));
        assert!(message.contains("Bound root problem: P001"));
        assert!(message.contains("watcher child thread"));
        assert!(message.contains("record evidence"));
        assert!(message.contains("Do not create a semantic wait primitive"));
        assert!(message.contains("Watcher goal: inspect CI every 30 minutes"));
    }

    #[tokio::test]
    async fn native_handler_binds_root_session_to_process() {
        let (session, mut turn) = make_session_and_context().await;
        let actor_session_id = session.thread_id().to_string();
        let expected_process_id = process_id_for_session_id(&actor_session_id).expect("process id");
        let temp_dir = tempfile::tempdir().expect("temp dir");
        turn.cwd = temp_dir.abs();
        let session = Arc::new(session);
        let turn = Arc::new(turn);

        let current = call(
            session.clone(),
            turn.clone(),
            QunuxOperation::Current,
            json!({"title": "Root", "body": "# Root"}),
        )
        .await;
        let current: serde_json::Value = serde_json::from_str(&current).expect("json");
        assert_eq!(
            current["context"]["process_id"].as_str(),
            Some(expected_process_id.as_str())
        );
        assert_eq!(
            current["context"]["thread_id"].as_str(),
            Some(DEFAULT_THREAD_ID)
        );
        assert_eq!(
            current["context"]["actor_session_id"].as_str(),
            Some(actor_session_id.as_str())
        );

        let second = call(session, turn, QunuxOperation::Current, json!({})).await;
        let second: serde_json::Value = serde_json::from_str(&second).expect("json");
        assert_eq!(
            second["context"]["process_id"].as_str(),
            Some(expected_process_id.as_str())
        );
        assert_eq!(second["status"]["root_problem_id"].as_str(), Some("P000"));
    }

    #[tokio::test]
    async fn native_handler_default_current_initializes_process_mission_root() {
        let (session, mut turn) = make_session_and_context().await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        turn.cwd = temp_dir.abs();
        let session = Arc::new(session);
        let turn = Arc::new(turn);

        let current = call(
            session.clone(),
            turn.clone(),
            QunuxOperation::Current,
            json!({}),
        )
        .await;
        let current: serde_json::Value = serde_json::from_str(&current).expect("json");
        assert_eq!(current["status"]["total_problems"].as_u64(), Some(1));
        assert_eq!(current["status"]["total_tickets"].as_u64(), Some(0));
        assert_eq!(current["status"]["total_results"].as_u64(), Some(0));
        assert_eq!(current["status"]["total_checks"].as_u64(), Some(0));
        assert_eq!(current["status"]["total_handles"].as_u64(), Some(0));
        assert_eq!(
            current["next"]["action"].as_str(),
            Some("create_solution_ticket")
        );

        let render = call(session, turn, QunuxOperation::Render, json!({})).await;
        let render: serde_json::Value = serde_json::from_str(&render).expect("json");
        let markdown = render["markdown"].as_str().expect("markdown");
        assert!(markdown.contains(DEFAULT_ROOT_PROBLEM_TITLE));
        assert!(!markdown.contains("Qunux root task"));
    }

    #[tokio::test]
    async fn native_handler_status_uses_session_bound_identity() {
        let (session, mut turn) = make_session_and_context().await;
        let actor_session_id = session.thread_id().to_string();
        let expected_process_id = process_id_for_session_id(&actor_session_id).expect("process id");
        let temp_dir = tempfile::tempdir().expect("temp dir");
        turn.cwd = temp_dir.abs();
        let session = Arc::new(session);
        let turn = Arc::new(turn);

        call(
            session.clone(),
            turn.clone(),
            QunuxOperation::Current,
            json!({"title": "Root", "body": "# Root"}),
        )
        .await;

        let status = call(session, turn, QunuxOperation::Status, json!({})).await;
        let status: serde_json::Value = serde_json::from_str(&status).expect("json");
        assert_eq!(
            status["process_id"].as_str(),
            Some(expected_process_id.as_str())
        );
        assert_eq!(status["thread_id"].as_str(), Some(DEFAULT_THREAD_ID));
        assert_eq!(status["thread_root_problem_id"].as_str(), Some("P000"));
        assert_eq!(status["valid"].as_bool(), Some(true));
    }

    #[tokio::test]
    async fn native_handler_outputs_chat_runtime_messages() {
        let (session, mut turn) = make_session_and_context().await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        turn.cwd = temp_dir.abs();
        let session = Arc::new(session);
        let turn = Arc::new(turn);

        let current = call(
            session.clone(),
            turn.clone(),
            QunuxOperation::Current,
            json!({"title": "Root", "body": "# Root"}),
        )
        .await;
        let current: serde_json::Value = serde_json::from_str(&current).expect("json");
        assert!(
            current["message"]
                .as_str()
                .is_some_and(|message| message.contains("Qunux: process"))
        );
        assert_eq!(current["status"]["root_problem_id"].as_str(), Some("P000"));

        let next = call(
            session.clone(),
            turn.clone(),
            QunuxOperation::Next,
            json!({}),
        )
        .await;
        let next: serde_json::Value = serde_json::from_str(&next).expect("json");
        assert!(
            next["message"]
                .as_str()
                .is_some_and(|message| message.contains("Qunux: next"))
        );
        assert_eq!(next["action"].as_str(), Some("create_solution_ticket"));

        let ticket = call(
            session,
            turn,
            QunuxOperation::CreateTicket,
            json!({"problem_id": "P000", "title": "Ticket", "body": "# Ticket"}),
        )
        .await;
        let ticket: serde_json::Value = serde_json::from_str(&ticket).expect("json");
        assert_eq!(ticket["id"].as_str(), Some("T000"));
        assert!(
            ticket["message"]
                .as_str()
                .is_some_and(|message| message.contains("Qunux: created ticket T000"))
        );
    }

    #[tokio::test]
    async fn native_handler_create_problem_supports_runtime_spawn_mode() {
        let (session, mut turn) = make_session_and_context().await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        turn.cwd = temp_dir.abs();
        let session = Arc::new(session);
        let turn = Arc::new(turn);

        call(
            session.clone(),
            turn.clone(),
            QunuxOperation::Current,
            json!({"title": "Root", "body": "# Root"}),
        )
        .await;
        call(
            session.clone(),
            turn.clone(),
            QunuxOperation::CreateTicket,
            json!({"problem_id": "P000", "title": "Parent ticket", "body": "# Parent"}),
        )
        .await;
        call(
            session.clone(),
            turn.clone(),
            QunuxOperation::ClassifyTicket,
            json!({"ticket_id": "T000", "classification": "one_go", "reason": "bounded"}),
        )
        .await;
        call(
            session.clone(),
            turn.clone(),
            QunuxOperation::SetStatus,
            json!({"kind": "problem", "id": "P000", "status": "doing"}),
        )
        .await;
        call(
            session.clone(),
            turn.clone(),
            QunuxOperation::SetStatus,
            json!({"kind": "ticket", "id": "T000", "status": "executing"}),
        )
        .await;

        let created = call(
            session.clone(),
            turn.clone(),
            QunuxOperation::CreateProblem,
            json!({
                "parent_id": "P000",
                "from_ticket_id": "T000",
                "mode": "spawn",
                "title": "Runtime child",
                "body": "# Runtime child"
            }),
        )
        .await;
        let created: serde_json::Value = serde_json::from_str(&created).expect("json");
        assert_eq!(created["id"].as_str(), Some("P001"));

        let next = call(session, turn, QunuxOperation::Next, json!({})).await;
        let next: serde_json::Value = serde_json::from_str(&next).expect("json");
        assert_eq!(next["action"].as_str(), Some("spawn_thread"));
        assert_eq!(next["problem_id"].as_str(), Some("P001"));
    }

    #[tokio::test]
    async fn native_handler_ingest_user_task_creates_child_problem() {
        let (session, mut turn) = make_session_and_context().await;
        let actor_session_id = session.thread_id().to_string();
        let temp_dir = tempfile::tempdir().expect("temp dir");
        turn.cwd = temp_dir.abs();
        let context = RuntimeContext::for_session(turn.cwd.clone(), actor_session_id)
            .expect("runtime context");
        let mut runtime =
            QunuxRuntime::load_or_init(context, "Root", "# Root").expect("init runtime");
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
            .expect("receive event");
        let session = Arc::new(session);
        let turn = Arc::new(turn);

        let ingested = call(
            session.clone(),
            turn.clone(),
            QunuxOperation::IngestUserTask,
            json!({
                "inbox_item_id": "IN000",
                "title": "Implement requested change",
                "body": "# Implement requested change\n\n## Problem\n\nImplement it.\n\n## Success Criteria\n\n- Done."
            }),
        )
        .await;
        let ingested: serde_json::Value = serde_json::from_str(&ingested).expect("json");
        assert_eq!(ingested["problem_id"].as_str(), Some("P001"));
        assert_eq!(ingested["parent_problem_id"].as_str(), Some("P000"));
        assert_eq!(ingested["inbox_item_id"].as_str(), Some("IN000"));
        assert_eq!(ingested["passive_event_id"].as_str(), Some("PE000"));
        assert_eq!(ingested["source_kind"].as_str(), Some("user_input"));
        assert!(
            ingested["message"]
                .as_str()
                .is_some_and(|message| message.contains("converted inbox item IN000"))
        );

        let current = call(
            session.clone(),
            turn.clone(),
            QunuxOperation::Current,
            json!({}),
        )
        .await;
        let current: serde_json::Value = serde_json::from_str(&current).expect("json");
        assert_eq!(current["status"]["total_problems"].as_u64(), Some(2));
        assert_eq!(current["status"]["pending_inbox_items"].as_u64(), Some(0));
        assert_eq!(current["next"]["action"].as_str(), Some("spawn_thread"));
        assert_eq!(current["next"]["problem_id"].as_str(), Some("P001"));

        let validate = call(session, turn, QunuxOperation::Validate, json!({})).await;
        assert!(validate.contains("\"ok\": true"));
    }

    #[tokio::test]
    async fn native_handler_scaffold_user_task_creates_problem_and_ticket() {
        let (session, mut turn) = make_session_and_context().await;
        let actor_session_id = session.thread_id().to_string();
        let temp_dir = tempfile::tempdir().expect("temp dir");
        turn.cwd = temp_dir.abs();
        let context = RuntimeContext::for_session(turn.cwd.clone(), actor_session_id)
            .expect("runtime context");
        let mut runtime =
            QunuxRuntime::load_or_init(context, "Root", "# Root").expect("init runtime");
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
            .expect("receive event");
        let session = Arc::new(session);
        let turn = Arc::new(turn);

        let scaffolded = call(
            session.clone(),
            turn.clone(),
            QunuxOperation::ScaffoldUserTask,
            json!({
                "inbox_item_id": "IN000",
                "problem_title": "Implement requested change",
                "problem_body": "# Implement requested change\n\n## Problem\n\nImplement it.\n\n## Success Criteria\n\n- Done.",
                "ticket_title": "Implement the requested change",
                "ticket_body": "# Implement the requested change\n\n## Problem Definition\n\nImplement it.\n\n## Proposed Solution\n\nMake the required code change.\n\n## Acceptance Criteria\n\n- Done.\n\n## Verification Plan\n\n- Run targeted tests.\n\n## Risks\n\n- Scope creep.\n\n## Assumptions\n\n- The request is actionable.",
                "note": "actionable inbox item converted to a Qunux child problem and default ticket"
            }),
        )
        .await;
        let scaffolded: serde_json::Value = serde_json::from_str(&scaffolded).expect("json");
        assert_eq!(scaffolded["problem_id"].as_str(), Some("P001"));
        assert_eq!(scaffolded["parent_problem_id"].as_str(), Some("P000"));
        assert_eq!(scaffolded["ticket_id"].as_str(), Some("T000"));
        assert_eq!(scaffolded["inbox_item_id"].as_str(), Some("IN000"));
        assert_eq!(scaffolded["passive_event_id"].as_str(), Some("PE000"));
        assert_eq!(scaffolded["source_kind"].as_str(), Some("user_input"));
        assert!(
            scaffolded["message"]
                .as_str()
                .is_some_and(|message| message.contains("scaffolded inbox item IN000"))
        );

        let current = call(
            session.clone(),
            turn.clone(),
            QunuxOperation::Current,
            json!({}),
        )
        .await;
        let current: serde_json::Value = serde_json::from_str(&current).expect("json");
        assert_eq!(current["status"]["total_problems"].as_u64(), Some(2));
        assert_eq!(current["status"]["total_tickets"].as_u64(), Some(1));
        assert_eq!(current["status"]["pending_inbox_items"].as_u64(), Some(0));
        assert_eq!(current["next"]["action"].as_str(), Some("spawn_thread"));
        assert_eq!(current["next"]["problem_id"].as_str(), Some("P001"));

        let validate = call(session, turn, QunuxOperation::Validate, json!({})).await;
        assert!(validate.contains("\"ok\": true"));
    }

    #[tokio::test]
    async fn native_handler_rejects_legacy_current_thread_id_arguments() {
        let (session, mut turn) = make_session_and_context().await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        turn.cwd = temp_dir.abs();
        let session = Arc::new(session);
        let turn = Arc::new(turn);

        let thread_status_result = call_result(
            session.clone(),
            turn.clone(),
            QunuxOperation::ThreadStatus,
            json!({"thread_id": "QT000"}),
        )
        .await;
        let Err(thread_status_err) = thread_status_result else {
            panic!("legacy thread_id must be rejected");
        };
        assert!(
            thread_status_err.to_string().contains("target_thread_id"),
            "unexpected error: {thread_status_err}"
        );

        let join_result = call_result(
            session,
            turn,
            QunuxOperation::JoinThread,
            json!({"thread_id": "QT001"}),
        )
        .await;
        let Err(join_err) = join_result else {
            panic!("legacy thread_id must be rejected");
        };
        assert!(
            join_err.to_string().contains("target_thread_id"),
            "unexpected error: {join_err}"
        );
    }

    #[tokio::test]
    async fn native_handlers_complete_one_go_loop() {
        let (session, mut turn) = make_session_and_context().await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        turn.cwd = temp_dir.abs();
        let session = Arc::new(session);
        let turn = Arc::new(turn);

        let current = call(
            session.clone(),
            turn.clone(),
            QunuxOperation::Current,
            json!({"title": "Root", "body": "# Root"}),
        )
        .await;
        assert!(current.contains("\"root_problem_id\": \"P000\""));

        let ticket = call(
            session.clone(),
            turn.clone(),
            QunuxOperation::CreateTicket,
            json!({"problem_id": "P000", "title": "Ticket", "body": "# Ticket"}),
        )
        .await;
        assert!(ticket.contains("\"id\": \"T000\""));

        call(
            session.clone(),
            turn.clone(),
            QunuxOperation::ClassifyTicket,
            json!({"ticket_id": "T000", "classification": "one_go", "reason": "bounded"}),
        )
        .await;
        call(
            session.clone(),
            turn.clone(),
            QunuxOperation::SetStatus,
            json!({"kind": "problem", "id": "P000", "status": "doing"}),
        )
        .await;
        let result = call(
            session.clone(),
            turn.clone(),
            QunuxOperation::Result,
            json!({"ticket_id": "T000", "title": "Result", "body": "# Result"}),
        )
        .await;
        assert!(result.contains("\"id\": \"R000\""));
        call(
            session.clone(),
            turn.clone(),
            QunuxOperation::Check,
            json!({
                "problem_id": "P000",
                "status": "success",
                "result_ids": ["R000"],
                "title": "Check",
                "body": "# Check"
            }),
        )
        .await;
        let next = call(session, turn, QunuxOperation::Next, json!({})).await;
        assert!(next.contains("\"action\": \"none\""));
        assert!(next.contains("\"disposition\": \"terminal\""));
    }

    #[tokio::test]
    async fn native_handler_wait_parks_current_thread_only() {
        let (session, mut turn) = make_session_and_context().await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        turn.cwd = temp_dir.abs();
        let session = Arc::new(session);
        let turn = Arc::new(turn);

        call(
            session.clone(),
            turn.clone(),
            QunuxOperation::Current,
            json!({"title": "Root", "body": "# Root"}),
        )
        .await;

        let parked = call(
            session.clone(),
            turn.clone(),
            QunuxOperation::Wait,
            json!({
                "mode": "all",
                "reason": "need user reply",
                "specs": [{
                    "kind": "user_input",
                    "condition": "reply",
                    "source": "chat",
                    "dedupe_key": "reply-1"
                }]
            }),
        )
        .await;
        let parked: serde_json::Value = serde_json::from_str(&parked).expect("json");
        assert_eq!(parked["wait"]["status"].as_str(), Some("waiting"));
        assert!(parked.get("op").is_none());

        let next = call(session, turn, QunuxOperation::Next, json!({})).await;
        let next: serde_json::Value = serde_json::from_str(&next).expect("json");
        assert_eq!(next["action"].as_str(), Some("wait_io"));
        assert_eq!(next["disposition"].as_str(), Some("io_wait"));
    }

    #[tokio::test]
    async fn native_handler_wait_rejects_internal_op_payloads() {
        let (session, mut turn) = make_session_and_context().await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        turn.cwd = temp_dir.abs();
        let session = Arc::new(session);
        let turn = Arc::new(turn);

        call(
            session.clone(),
            turn.clone(),
            QunuxOperation::Current,
            json!({"title": "Root", "body": "# Root"}),
        )
        .await;
        let result = call_result(
            session,
            turn,
            QunuxOperation::Wait,
            json!({
                "op": "park",
                "mode": "all",
                "reason": "wait for timer window",
                "specs": [{
                    "kind": "timer",
                    "condition": "poll-window",
                    "source": "timer"
                }]
            }),
        )
        .await;
        let Err(err) = result else {
            panic!("agent-facing wait must reject internal op payloads");
        };
        assert!(
            err.to_string().contains("unknown field `op`"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn native_handler_wait_rejects_nested_internal_payloads() {
        let (session, mut turn) = make_session_and_context().await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        turn.cwd = temp_dir.abs();
        let session = Arc::new(session);
        let turn = Arc::new(turn);

        call(
            session.clone(),
            turn.clone(),
            QunuxOperation::Current,
            json!({"title": "Root", "body": "# Root"}),
        )
        .await;
        let result = call_result(
            session.clone(),
            turn.clone(),
            QunuxOperation::Wait,
            json!({
                "mode": "all",
                "reason": "wait for timer window",
                "specs": [{
                    "kind": "timer",
                    "condition": "poll-window",
                    "source": "timer",
                    "op": "wake"
                }]
            }),
        )
        .await;
        let Err(err) = result else {
            panic!("nested internal payload fields must be rejected");
        };
        assert!(
            err.to_string().contains("unknown field `op`"),
            "unexpected error: {err}"
        );

        let result = call_result(
            session,
            turn,
            QunuxOperation::Wait,
            json!({
                "mode": "all",
                "reason": "wait for github checks",
                "specs": [{
                    "kind": "event_key",
                    "event_key": {
                        "kind": "github.checks.completed",
                        "resource": "repo#42",
                        "source": "github",
                        "op": "wake"
                    }
                }]
            }),
        )
        .await;
        let Err(err) = result else {
            panic!("nested event key internal payload fields must be rejected");
        };
        assert!(
            err.to_string().contains("unknown field `op`"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn native_handler_rejects_unbound_subagent_session() {
        let (session, mut turn) = make_session_and_context().await;
        let temp_dir = tempfile::tempdir().expect("temp dir");
        turn.cwd = temp_dir.abs();
        turn.session_source = codex_protocol::protocol::SessionSource::SubAgent(
            codex_protocol::protocol::SubAgentSource::ThreadSpawn {
                parent_thread_id: session.thread_id(),
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            },
        );
        let session = Arc::new(session);
        let turn = Arc::new(turn);

        let result = call_result(
            session,
            turn,
            QunuxOperation::Current,
            json!({"title": "Should not bind", "body": "# Should not bind"}),
        )
        .await;
        let Err(err) = result else {
            panic!("unbound subagent must not lazily enter Qunux");
        };
        assert!(
            err.to_string().contains("not bound to a Qunux thread"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn native_next_returns_wait_without_bound_actor() {
        let (session, mut turn) = make_session_and_context().await;
        let actor_session_id = session.thread_id().to_string();
        let temp_dir = tempfile::tempdir().expect("temp dir");
        turn.cwd = temp_dir.abs();
        let root_context =
            RuntimeContext::for_session(temp_dir.abs(), actor_session_id).expect("root context");
        let session = Arc::new(session);
        let turn = Arc::new(turn);

        create_split_child(session.clone(), turn.clone()).await;

        let mut runtime = QunuxRuntime::load(root_context).expect("load runtime");
        let spawned = runtime
            .spawn_thread(
                "P001",
                ContextForkPolicy::FullContext,
                "Solve child",
                None,
                None,
                Vec::new(),
            )
            .expect("spawn logical child");
        assert_eq!(runtime.next().action, NextAction::WaitThread);
        assert_eq!(runtime.next().disposition, NextDisposition::IoWait);
        assert!(
            runtime.state().threads[&spawned.thread_id]
                .actor_session_id
                .is_none()
        );

        let next = call(session, turn, QunuxOperation::Next, json!({})).await;
        let next: serde_json::Value = serde_json::from_str(&next).expect("json");
        assert_eq!(next["action"].as_str(), Some("wait_thread"));
        assert_eq!(next["disposition"].as_str(), Some("io_wait"));
        assert_eq!(
            next["target_thread_id"].as_str(),
            Some(spawned.thread_id.as_str())
        );
    }

    #[tokio::test]
    async fn native_next_returns_wait_without_status_subscription() {
        let (session, mut turn) = make_session_and_context().await;
        let actor_session_id = session.thread_id().to_string();
        let temp_dir = tempfile::tempdir().expect("temp dir");
        turn.cwd = temp_dir.abs();
        let root_context =
            RuntimeContext::for_session(temp_dir.abs(), actor_session_id).expect("root context");
        let session = Arc::new(session);
        let turn = Arc::new(turn);

        create_split_child(session.clone(), turn.clone()).await;

        let mut runtime = QunuxRuntime::load(root_context).expect("load runtime");
        let spawned = runtime
            .spawn_thread(
                "P001",
                ContextForkPolicy::FullContext,
                "Solve child",
                None,
                None,
                Vec::new(),
            )
            .expect("spawn logical child");
        runtime
            .bind_thread_actor(&spawned.thread_id, ThreadId::new().to_string())
            .expect("bind unavailable actor id");
        assert_eq!(runtime.next().disposition, NextDisposition::IoWait);

        let next = call(session, turn, QunuxOperation::Next, json!({})).await;
        let next: serde_json::Value = serde_json::from_str(&next).expect("json");
        assert_eq!(next["action"].as_str(), Some("wait_thread"));
        assert_eq!(next["disposition"].as_str(), Some("io_wait"));
        assert_eq!(
            next["target_thread_id"].as_str(),
            Some(spawned.thread_id.as_str())
        );
    }

    #[tokio::test]
    async fn native_handler_records_spawn_failure_after_logical_thread_creation() {
        let (session, mut turn) = make_session_and_context().await;
        let actor_session_id = session.thread_id().to_string();
        let temp_dir = tempfile::tempdir().expect("temp dir");
        turn.cwd = temp_dir.abs();
        let root_context =
            RuntimeContext::for_session(temp_dir.abs(), actor_session_id).expect("root context");
        let session = Arc::new(session);
        let turn = Arc::new(turn);

        call(
            session.clone(),
            turn.clone(),
            QunuxOperation::Current,
            json!({"title": "Root", "body": "# Root"}),
        )
        .await;
        call(
            session.clone(),
            turn.clone(),
            QunuxOperation::CreateTicket,
            json!({"problem_id": "P000", "title": "Parent ticket", "body": "# Parent"}),
        )
        .await;
        call(
            session.clone(),
            turn.clone(),
            QunuxOperation::ClassifyTicket,
            json!({"ticket_id": "T000", "classification": "split", "reason": "needs child"}),
        )
        .await;
        call(
            session.clone(),
            turn.clone(),
            QunuxOperation::SetStatus,
            json!({"kind": "ticket", "id": "T000", "status": "splitting"}),
        )
        .await;
        call(
            session.clone(),
            turn.clone(),
            QunuxOperation::CreateProblem,
            json!({
                "parent_id": "P000",
                "from_ticket_id": "T000",
                "mode": "split",
                "title": "Child",
                "body": "# Child"
            }),
        )
        .await;

        let result = call_result(
            session,
            turn,
            QunuxOperation::SpawnThread,
            json!({"problem_id": "P001", "bootstrap_instruction": "Solve child"}),
        )
        .await;
        let Err(err) = result else {
            panic!("spawn should fail without an AgentControl manager");
        };
        assert!(
            err.to_string().contains("collab manager unavailable"),
            "unexpected error: {err}"
        );

        let runtime = QunuxRuntime::load(root_context).expect("load runtime");
        let child_thread = runtime
            .state()
            .threads
            .values()
            .find(|thread| thread.root_problem_id == "P001")
            .expect("child Qunux thread");
        assert_eq!(child_thread.status, ThreadStatus::Failed);
        let handle = runtime
            .state()
            .handles
            .values()
            .find(|handle| handle.target_thread_id.as_deref() == Some(child_thread.id.as_str()))
            .expect("child handle");
        assert_eq!(handle.status, IoHandleStatus::Failed);
        assert_eq!(runtime.next().action, NextAction::RecoverThread);
        assert!(runtime.state().io_events.iter().any(|event| {
            event.kind == IoEventKind::ChildThreadSpawnFailed
                && event.thread_id.as_deref() == Some(child_thread.id.as_str())
                && event.handle_id.as_deref() == Some(handle.id.as_str())
        }));
    }

    #[tokio::test]
    async fn native_handler_recovers_failed_child_thread() {
        let (session, mut turn) = make_session_and_context().await;
        let actor_session_id = session.thread_id().to_string();
        let temp_dir = tempfile::tempdir().expect("temp dir");
        turn.cwd = temp_dir.abs();
        let root_context =
            RuntimeContext::for_session(temp_dir.abs(), actor_session_id).expect("root context");
        let session = Arc::new(session);
        let turn = Arc::new(turn);

        create_split_child(session.clone(), turn.clone()).await;

        let mut runtime = QunuxRuntime::load(root_context.clone()).expect("load runtime");
        let spawned = runtime
            .spawn_thread(
                "P001",
                ContextForkPolicy::FullContext,
                "Solve child",
                None,
                None,
                Vec::new(),
            )
            .expect("spawn logical child");
        runtime
            .record_child_thread_spawn_failed(&spawned.thread_id, "Codex child spawn failed")
            .expect("record failure");
        assert_eq!(runtime.next().action, NextAction::RecoverThread);

        let recovered = call(
            session,
            turn,
            QunuxOperation::RecoverThread,
            json!({"target_thread_id": spawned.thread_id}),
        )
        .await;
        let recovered: serde_json::Value = serde_json::from_str(&recovered).expect("json");
        assert_eq!(recovered["thread_id"].as_str(), Some("QT001"));
        assert_eq!(recovered["root_problem_id"].as_str(), Some("P001"));

        let runtime = QunuxRuntime::load(root_context).expect("reload runtime");
        let thread = &runtime.state().threads["QT001"];
        assert_eq!(thread.status, ThreadStatus::Recovered);
        assert_eq!(
            runtime.state().problems["P001"].owner_thread_id,
            DEFAULT_THREAD_ID
        );
        assert_eq!(runtime.next().action, NextAction::SpawnThread);
        runtime.validate().expect("valid");
    }

    async fn create_split_child(
        session: Arc<crate::session::session::Session>,
        turn: Arc<crate::session::turn_context::TurnContext>,
    ) {
        call(
            session.clone(),
            turn.clone(),
            QunuxOperation::Current,
            json!({"title": "Root", "body": "# Root"}),
        )
        .await;
        call(
            session.clone(),
            turn.clone(),
            QunuxOperation::CreateTicket,
            json!({"problem_id": "P000", "title": "Parent ticket", "body": "# Parent"}),
        )
        .await;
        call(
            session.clone(),
            turn.clone(),
            QunuxOperation::ClassifyTicket,
            json!({"ticket_id": "T000", "classification": "split", "reason": "needs child"}),
        )
        .await;
        call(
            session.clone(),
            turn.clone(),
            QunuxOperation::SetStatus,
            json!({"kind": "ticket", "id": "T000", "status": "splitting"}),
        )
        .await;
        call(
            session,
            turn,
            QunuxOperation::CreateProblem,
            json!({
                "parent_id": "P000",
                "from_ticket_id": "T000",
                "mode": "split",
                "title": "Child",
                "body": "# Child"
            }),
        )
        .await;
    }

    async fn call(
        session: Arc<crate::session::session::Session>,
        turn: Arc<crate::session::turn_context::TurnContext>,
        operation: QunuxOperation,
        arguments: serde_json::Value,
    ) -> String {
        call_result(session, turn, operation, arguments)
            .await
            .expect("handler call")
            .into_text()
    }

    async fn call_result(
        session: Arc<crate::session::session::Session>,
        turn: Arc<crate::session::turn_context::TurnContext>,
        operation: QunuxOperation,
        arguments: serde_json::Value,
    ) -> Result<FunctionToolOutput, FunctionCallError> {
        QunuxHandler::new(operation)
            .handle(ToolInvocation {
                session,
                turn,
                cancellation_token: CancellationToken::new(),
                tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
                call_id: format!("call-{}", operation.name()),
                tool_name: ToolName::namespaced(QUNUX_NAMESPACE, operation.name()),
                source: ToolCallSource::Direct,
                payload: ToolPayload::Function {
                    arguments: arguments.to_string(),
                },
            })
            .await
    }
}
