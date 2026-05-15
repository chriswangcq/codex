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
use codex_qunux::EntityKind;
use codex_qunux::QunuxRuntime;
use codex_qunux::RuntimeContext;
use codex_qunux::SpawnedThread;
use codex_qunux::TicketClassification;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde::Serialize;

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
    JoinThread,
    ListThreads,
    ThreadStatus,
    Status,
    Validate,
    Render,
}

impl QunuxOperation {
    pub fn all() -> [Self; 15] {
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
            Self::JoinThread,
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
            Self::JoinThread => "join_thread",
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
        let value = match self.operation {
            QunuxOperation::Current => {
                let args: CurrentArgs = parse_arguments(&arguments)?;
                runtime = runtime_for_invocation(&invocation, args.title, args.body)?;
                serde_json::to_value(runtime.current())
            }
            QunuxOperation::Next => {
                serde_json::to_value(runtime.next())
            }
            QunuxOperation::CreateProblem => {
                let args: CreateProblemArgs = parse_arguments(&arguments)?;
                let id = runtime
                    .create_problem_from_ticket(
                        args.parent_id,
                        args.from_ticket_id,
                        args.title,
                        args.body,
                    )
                    .map_err(qunux_error)?;
                serde_json::to_value(IdResponse { id })
            }
            QunuxOperation::CreateTicket => {
                let args: CreateTicketArgs = parse_arguments(&arguments)?;
                let id = runtime
                    .create_ticket(args.problem_id, args.title, args.body)
                    .map_err(qunux_error)?;
                serde_json::to_value(IdResponse { id })
            }
            QunuxOperation::ClassifyTicket => {
                let args: ClassifyTicketArgs = parse_arguments(&arguments)?;
                runtime
                    .classify_ticket(args.ticket_id, args.classification, args.reason)
                    .map_err(qunux_error)?;
                serde_json::to_value(OkResponse::ok())
            }
            QunuxOperation::SetStatus => {
                let args: SetStatusArgs = parse_arguments(&arguments)?;
                runtime
                    .set_status(args.kind, args.id, args.status)
                    .map_err(qunux_error)?;
                serde_json::to_value(OkResponse::ok())
            }
            QunuxOperation::Result => {
                let args: ResultArgs = parse_arguments(&arguments)?;
                let id = runtime
                    .record_result(args.ticket_id, args.title, args.body)
                    .map_err(qunux_error)?;
                serde_json::to_value(IdResponse { id })
            }
            QunuxOperation::Check => {
                let args: CheckArgs = parse_arguments(&arguments)?;
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
                serde_json::to_value(IdResponse { id })
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
                serde_json::to_value(SpawnThreadResponse {
                    thread_id: spawned.thread_id,
                    root_problem_id: spawned.root_problem_id,
                    handle_id: spawned.handle_id,
                    wait_id: spawned.wait_id,
                    codex_agent_id: Some(codex_agent_id),
                    bootstrap_instruction: spawned.bootstrap_instruction,
                })
            }
            QunuxOperation::JoinThread => {
                let args: JoinThreadArgs = parse_arguments(&arguments)?;
                let joined = runtime
                    .join_thread(args.target_thread_id)
                    .map_err(qunux_error)?;
                serde_json::to_value(joined)
            }
            QunuxOperation::ListThreads => serde_json::to_value(runtime.list_threads()),
            QunuxOperation::ThreadStatus => {
                let args: ThreadStatusArgs = parse_arguments(&arguments)?;
                let thread = runtime
                    .thread_status(args.target_thread_id)
                    .map_err(qunux_error)?;
                serde_json::to_value(thread)
            }
            QunuxOperation::Status => serde_json::to_value(runtime.status()),
            QunuxOperation::Validate => runtime
                .validate()
                .map(|()| serde_json::to_value(OkResponse::ok()))
                .map_err(qunux_error)?,
            QunuxOperation::Render => serde_json::to_value(RenderResponse {
                markdown: runtime.render(),
            }),
        }
        .map_err(|err| FunctionCallError::Fatal(err.to_string()))?;
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
            .unwrap_or_else(|| "Qunux root task".to_string()),
        body.clone()
            .unwrap_or_else(|| "# Qunux root task\n\nNative Codex Qunux process.".to_string()),
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
#[serde(rename_all = "snake_case")]
struct JoinThreadArgs {
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
