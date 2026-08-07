use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::unified_exec::TerminateMonitorResult;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use std::collections::BTreeMap;

const TOOL_NAME: &str = "task_stop";

pub struct TaskStopHandler;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskStopArgs {
    task_id: String,
}

impl ToolExecutor<ToolInvocation> for TaskStopHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: TOOL_NAME.to_string(),
            description: "Stop a running monitor by its task ID.".to_string(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([(
                    "task_id".to_string(),
                    JsonSchema::string(Some("Task ID returned by the monitor tool.".to_string())),
                )]),
                Some(vec!["task_id".to_string()]),
                Some(false.into()),
            ),
            output_schema: None,
        })
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolInvocation {
                session, payload, ..
            } = invocation;
            let ToolPayload::Function { arguments } = payload else {
                return Err(FunctionCallError::RespondToModel(
                    "task_stop handler received unsupported payload".to_string(),
                ));
            };
            let args: TaskStopArgs = parse_arguments(&arguments)?;
            let result = session
                .services
                .unified_exec_manager
                .terminate_monitor(&args.task_id)
                .await;
            let (task_id, command) = match result {
                TerminateMonitorResult::Stopped { info, command } => (info.task_id, command),
                TerminateMonitorResult::StopFailed => {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "Failed to stop task {}",
                        args.task_id
                    )));
                }
                TerminateMonitorResult::NotRunning(status) => {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "Task {} is not running (status: {status})",
                        args.task_id
                    )));
                }
                TerminateMonitorResult::NotFound => {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "No running monitor found for task {}",
                        args.task_id
                    )));
                }
            };
            let output = serde_json::json!({
                "message": format!("Successfully stopped task: {task_id} ({command})"),
                "task_id": task_id,
                "task_type": "local_bash",
                "command": command,
            })
            .to_string();
            Ok(boxed_tool_output(FunctionToolOutput::from_text(
                output,
                Some(true),
            )))
        })
    }
}

impl CoreToolRuntime for TaskStopHandler {}
