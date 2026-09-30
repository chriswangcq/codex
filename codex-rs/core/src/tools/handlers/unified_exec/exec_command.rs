use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::exec::DEFAULT_EXEC_COMMAND_TIMEOUT_MS;
use crate::exec_policy::prompt_is_rejected_by_policy;
use crate::function_tool::FunctionCallError;
use crate::maybe_emit_implicit_skill_invocation;
use crate::tools::context::ExecCommandToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::apply_granted_turn_permissions;
use crate::tools::handlers::apply_patch::intercept_apply_patch;
use crate::tools::handlers::file_system_sandbox_policy_context_for_cwd;
use crate::tools::handlers::implicit_granted_permissions;
use crate::tools::handlers::normalize_and_validate_additional_permissions;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::parse_arguments_with_base_path;
use crate::tools::handlers::resolve_sandbox_permissions;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::handlers::rewrite_function_string_argument;
use crate::tools::handlers::updated_hook_command;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolExecutor;
use crate::unified_exec::ExecCommandRequest;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::UnifiedExecError;
use crate::unified_exec::UnifiedExecProcessManager;
use crate::unified_exec::generate_chunk_id;
use codex_features::Feature;
use codex_otel::SessionTelemetry;
use codex_otel::TOOL_CALL_UNIFIED_EXEC_METRIC;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::CommandMonitorInfo;
use codex_sandboxing::SandboxManager;
use codex_sandboxing::SandboxType;
use codex_sandboxing::SandboxablePreference;
use codex_shell_command::shell_detect::detect_shell_type;
use codex_tools::JsonSchema;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_output_truncation::approx_token_count;
use codex_utils_path_uri::PathConvention;
use codex_utils_string::truncate_middle_chars;
use rand::distr::Alphanumeric;
use rand::distr::SampleString;

use super::super::shell_spec::CommandToolOptions;
use super::super::shell_spec::create_exec_command_tool_with_environment_id;
use super::super::shell_spec::create_monitor_tool;
use super::ExecCommandArgs;
use super::ExecCommandEnvironmentArgs;
use super::get_command;
use super::post_unified_exec_tool_use_payload;
use super::shell_mode_for_environment;

// A byte limit is a conservative hard token bound even for byte-fallback tokenizers.
const EXEC_COMMAND_REJECTION_MAX_BYTES: usize = 900;

#[derive(Clone, Copy)]
pub(crate) struct ExecCommandHandlerOptions {
    pub(crate) allow_login_shell: bool,
    pub(crate) allow_tty: bool,
    pub(crate) exec_permission_approvals_enabled: bool,
    pub(crate) include_environment_id: bool,
    pub(crate) include_shell_parameter: bool,
    pub(crate) include_windows_shell_guidance: bool,
    pub(crate) monitor: bool,
}

#[derive(Clone, Copy)]
enum ExecCommandLifetime {
    Interactive,
    OneShot,
}

pub struct ExecCommandHandler {
    options: ExecCommandHandlerOptions,
    lifetime: ExecCommandLifetime,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MonitorArgs {
    command: String,
    description: String,
    #[serde(default)]
    environment_id: Option<String>,
    #[serde(default = "default_monitor_timeout_ms")]
    pub(super) timeout_ms: u64,
    #[serde(default)]
    pub(super) persistent: bool,
}

struct MonitorToolOutput {
    monitor: CommandMonitorInfo,
    command: String,
}

impl MonitorToolOutput {
    fn response_text(&self) -> String {
        let lifetime = if self.monitor.persistent {
            "persistent — runs until TaskStop or session end".to_string()
        } else {
            format!("timeout {}ms", self.monitor.timeout_ms)
        };
        format!(
            "Monitor started (task {}, {lifetime}). You will be notified on each event. Keep working — do not poll or sleep. Events may arrive while you are waiting for the user — an event is not their reply.",
            self.monitor.task_id
        )
    }

    fn structured_result(&self) -> serde_json::Value {
        serde_json::json!({
            "taskId": self.monitor.task_id,
            "timeoutMs": self.monitor.timeout_ms,
            "persistent": self.monitor.persistent,
        })
    }
}

impl ToolOutput for MonitorToolOutput {
    fn log_output(&self) -> String {
        self.response_text()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        let mut output = FunctionCallOutputPayload::from_text(self.response_text());
        output.success = Some(true);
        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output,
        }
    }

    fn post_tool_use_input(&self, _payload: &ToolPayload) -> Option<serde_json::Value> {
        Some(serde_json::json!({ "command": self.command }))
    }

    fn post_tool_use_response(
        &self,
        _call_id: &str,
        _payload: &ToolPayload,
    ) -> Option<serde_json::Value> {
        Some(self.structured_result())
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> serde_json::Value {
        self.structured_result()
    }
}

impl Default for ExecCommandHandler {
    fn default() -> Self {
        Self {
            lifetime: ExecCommandLifetime::Interactive,
            options: ExecCommandHandlerOptions {
                allow_login_shell: false,
                allow_tty: true,
                exec_permission_approvals_enabled: false,
                include_environment_id: false,
                include_shell_parameter: true,
                include_windows_shell_guidance: cfg!(windows),
                monitor: false,
            },
        }
    }
}

impl ExecCommandHandler {
    pub(crate) fn new(options: ExecCommandHandlerOptions) -> Self {
        Self {
            options,
            lifetime: ExecCommandLifetime::Interactive,
        }
    }

    pub(crate) fn one_shot(options: ExecCommandHandlerOptions) -> Self {
        Self {
            options,
            lifetime: ExecCommandLifetime::OneShot,
        }
    }
}

impl ToolExecutor<ToolInvocation> for ExecCommandHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(if self.options.monitor {
            "monitor"
        } else {
            "exec_command"
        })
    }

    fn spec(&self) -> ToolSpec {
        if self.options.monitor {
            return create_monitor_tool(self.options.include_environment_id);
        }
        let spec = create_exec_command_tool_with_environment_id(
            CommandToolOptions {
                allow_login_shell: self.options.allow_login_shell,
                exec_permission_approvals_enabled: self.options.exec_permission_approvals_enabled,
            },
            self.options.include_environment_id,
            self.options.include_shell_parameter,
            self.options.include_windows_shell_guidance,
        );
        let mut spec = match self.lifetime {
            ExecCommandLifetime::Interactive => spec,
            ExecCommandLifetime::OneShot => one_shot_exec_command_spec(spec),
        };
        if !self.options.allow_tty
            && let ToolSpec::Function(spec) = &mut spec
        {
            spec.parameters
                .properties
                .get_or_insert_default()
                .remove("tty");
        }
        spec
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl ExecCommandHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            step_context,
            cancellation_token,
            tracker,
            call_id,
            payload,
            ..
        } = invocation;

        let mut arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "exec_command handler received unsupported payload".to_string(),
                ));
            }
        };

        let monitor = if self.options.monitor {
            let monitor: MonitorArgs = parse_arguments(&arguments)?;
            if monitor_command_has_hidden_control_chars(&monitor.command) {
                return Err(FunctionCallError::RespondToModel(
                    "command contains control characters that would be hidden in the approval dialog"
                        .to_string(),
                ));
            }
            let timeout_ms = effective_monitor_timeout_ms(monitor.timeout_ms, monitor.persistent)
                .map_err(FunctionCallError::RespondToModel)?;
            arguments = serde_json::json!({
                "cmd": monitor.command,
                "environment_id": monitor.environment_id.or_else(|| {
                    step_context
                        .environments
                        .local()
                        .map(|environment| environment.selection.environment_id.clone())
                }),
                "yield_time_ms": 250,
                "tty": false,
            })
            .to_string();
            let task_id = format!(
                "b{}",
                Alphanumeric
                    .sample_string(&mut rand::rng(), 8)
                    .to_ascii_lowercase()
            );
            Some(CommandMonitorInfo {
                task_id,
                description: monitor.description,
                timeout_ms,
                persistent: monitor.persistent,
            })
        } else {
            None
        };

        let manager: &UnifiedExecProcessManager = &session.services.unified_exec_manager;
        let context = UnifiedExecContext::new(
            session.clone(),
            step_context.clone(),
            cancellation_token,
            call_id.clone(),
        );
        let environment_args: ExecCommandEnvironmentArgs = parse_arguments(&arguments)?;
        let Some(turn_environment) = resolve_tool_environment(
            &step_context.environments,
            environment_args.environment_id.as_deref(),
        )?
        else {
            return Err(FunctionCallError::RespondToModel(
                "unified exec is unavailable in this session".to_string(),
            ));
        };
        let native_environment_cwd = turn_environment.cwd().clone();
        let cwd = environment_args
            .workdir
            .as_deref()
            .filter(|workdir| !workdir.is_empty())
            .map_or_else(
                || Ok(native_environment_cwd.clone()),
                |workdir| native_environment_cwd.join(workdir),
            )
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))?;
        let environment = Arc::clone(&turn_environment.environment);
        if monitor.is_some() && environment.is_remote() {
            return Err(FunctionCallError::RespondToModel(
                "monitor is currently available only in the local execution environment"
                    .to_string(),
            ));
        }
        let fs = environment.get_filesystem();

        // Remote executors enforce URI-native sandbox policy themselves. Only a host-local
        // sandbox needs a native cwd for resolving paths nested in the permissions config.
        let requires_host_native_cwd = !environment.is_remote()
            && SandboxManager::new().select_initial(
                turn_environment.permission_profile(),
                SandboxablePreference::Auto,
                codex_protocol::sandbox::effective_windows_sandbox_type(
                    turn_environment.config().windows_sandbox_type,
                    turn_environment.config().windows_sandbox_level,
                ),
                turn.network.is_some(),
            ) != SandboxType::None;
        // `to_abs_path()` alone cannot identify foreign drive paths: `file:///C:/repo` is
        // representable as `/C:/repo` on POSIX. Require the inferred convention to match too.
        let cwd_uses_native_convention =
            cwd.infer_path_convention() == Some(PathConvention::native());
        let native_cwd = match cwd.to_abs_path() {
            Ok(cwd) if cwd_uses_native_convention => Some(cwd),
            _ if !requires_host_native_cwd => None,
            Err(err) => return Err(FunctionCallError::RespondToModel(err.to_string())),
            Ok(_) => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "path URI `{cwd}` does not use the host's native {} path convention",
                    PathConvention::native()
                )));
            }
        };
        let mut args: ExecCommandArgs = match native_cwd.as_ref() {
            Some(native_cwd) => {
                // The base path only resolves paths nested in the permissions config types.
                parse_arguments_with_base_path(&arguments, native_cwd)?
            }
            None => {
                // Foreign executor cwd values cannot seed this host's AbsolutePathBufGuard.
                // Sandbox intent and URI-native roots are still sent to the executor.
                parse_arguments(&arguments)?
            }
        };
        if args.tty && !session.features().enabled(Feature::UnifiedExecTty) {
            return Err(FunctionCallError::RespondToModel(
                "TTY execution is disabled by config; omit `tty` or set it to false.".to_string(),
            ));
        }
        let sandbox_permissions =
            resolve_sandbox_permissions(args.sandbox_permissions, args.justification.as_deref())?;
        let hook_command = args.cmd.clone();
        maybe_emit_implicit_skill_invocation(
            session.as_ref(),
            context.step_context.turn.as_ref(),
            &hook_command,
            &cwd,
            native_cwd.as_ref(),
            &turn_environment.selection.environment_id,
        )
        .await;
        let shell_mode =
            shell_mode_for_environment(&turn.unified_exec_shell_mode, environment.as_ref());
        // Remote environments may use a different OS and must build commands with their native
        // shell; fall back to the session shell when the environment did not report one.
        let shell = turn_environment
            .shell
            .clone()
            .map(Arc::new)
            .unwrap_or_else(|| session.user_shell());
        // TODO(anp): Resolve requested shells in remote environments instead of restricting
        // commands to the reported default shell.
        if environment.is_remote()
            && let Some(requested_shell) = args.shell.take()
        {
            let Some(remote_shell) = turn_environment.shell.as_ref() else {
                return Err(FunctionCallError::RespondToModel(format!(
                    "environment `{}` does not report a shell",
                    turn_environment.selection.environment_id
                )));
            };
            if detect_shell_type(Path::new(&requested_shell)) != Some(remote_shell.shell_type) {
                return Err(FunctionCallError::RespondToModel(format!(
                    "environment `{}` only supports `{}`",
                    turn_environment.selection.environment_id,
                    remote_shell.name()
                )));
            }
        }
        let resolved_command = get_command(
            &args,
            shell,
            &shell_mode,
            turn_environment.config().allow_login_shell,
        )
        .map_err(FunctionCallError::RespondToModel)?;
        let command = resolved_command.command;
        let ExecCommandArgs {
            mut tty,
            yield_time_ms,
            timeout_ms,
            max_output_tokens,
            sandbox_permissions: _,
            additional_permissions,
            justification,
            prefix_rule,
            ..
        } = args;
        let completion_timeout = match self.lifetime {
            ExecCommandLifetime::Interactive => None,
            ExecCommandLifetime::OneShot => {
                tty = false;
                Some(Duration::from_millis(
                    timeout_ms.unwrap_or(DEFAULT_EXEC_COMMAND_TIMEOUT_MS),
                ))
            }
        };

        let exec_permission_approvals_enabled =
            session.features().enabled(Feature::ExecPermissionApprovals);
        let requested_additional_permissions = additional_permissions.clone();
        let sandbox_context =
            turn_environment.sandbox_context(/*additional_permissions*/ None);
        let permission_context = file_system_sandbox_policy_context_for_cwd(&sandbox_context, &cwd);
        let effective_additional_permissions = apply_granted_turn_permissions(
            context.session.as_ref(),
            turn_environment,
            &cwd,
            sandbox_permissions,
            additional_permissions,
        )
        .await;
        let additional_permissions_allowed = exec_permission_approvals_enabled
            || (session.features().enabled(Feature::RequestPermissionsTool)
                && effective_additional_permissions.permissions_preapproved);

        // Sticky turn permissions have already been approved, so they should
        // continue through the normal exec approval flow for the command.
        let approval_policy = context.step_context.settings.approval_policy();
        if effective_additional_permissions
            .sandbox_permissions
            .requests_sandbox_override()
            && !effective_additional_permissions.permissions_preapproved
            && prompt_is_rejected_by_policy(approval_policy, /*prompt_is_rule*/ false).is_some()
        {
            return Err(FunctionCallError::RespondToModel(format!(
                "approval policy is {approval_policy:?}; reject command — you cannot ask for escalated permissions if the approval policy is {approval_policy:?}"
            )));
        }

        let normalized_additional_permissions = match implicit_granted_permissions(
            sandbox_permissions,
            requested_additional_permissions.as_ref(),
            &effective_additional_permissions,
        )
        .map_or_else(
            || {
                normalize_and_validate_additional_permissions(
                    additional_permissions_allowed,
                    approval_policy,
                    effective_additional_permissions.sandbox_permissions,
                    effective_additional_permissions.additional_permissions,
                    effective_additional_permissions.permissions_preapproved,
                    &permission_context,
                )
            },
            |permissions| Ok(Some(permissions)),
        ) {
            Ok(normalized) => normalized,
            Err(err) => {
                return Err(FunctionCallError::RespondToModel(err));
            }
        };

        let intercepted_patch = if self.options.monitor {
            None
        } else {
            intercept_apply_patch(
                &command,
                &cwd,
                fs.as_ref(),
                turn_environment.clone(),
                context.session.clone(),
                Arc::clone(&context.step_context),
                context.cancellation_token.clone(),
                Some(&tracker),
                &context.call_id,
                "exec_command",
            )
            .await?
        };
        if let Some(output) = intercepted_patch {
            return Ok(boxed_tool_output(ExecCommandToolOutput {
                event_call_id: String::new(),
                chunk_id: String::new(),
                wall_time: std::time::Duration::ZERO,
                raw_output: output.into_text().into_bytes(),
                truncation_policy: step_context.settings.model_info.truncation_policy.into(),
                max_output_tokens,
                process_id: None,
                exit_code: None,
                original_token_count: None,
                output_omitted_bytes: None,
                hook_command: None,
            }));
        }

        emit_unified_exec_tty_metric(&step_context.session_telemetry, tty);
        crate::tools::lifecycle::notify_command_start(
            context.session.as_ref(),
            context.step_context.turn.as_ref(),
            &context.call_id,
            &command,
            &cwd,
            fs.as_ref(),
        )
        .await;
        // Preparation can be cancelled. Reserve a process only once it is done.
        let process_id = manager.allocate_process_id().await;
        let request = ExecCommandRequest {
            command,
            shell: resolved_command.shell,
            hook_command: hook_command.clone(),
            process_id,
            yield_time_ms,
            max_output_tokens,
            cwd,
            sandbox_cwd: native_environment_cwd,
            turn_environment: turn_environment.clone(),
            shell_mode,
            network: context.step_context.turn.network.clone(),
            tty,
            sandbox_permissions: effective_additional_permissions.sandbox_permissions,
            additional_permissions: normalized_additional_permissions,
            additional_permissions_preapproved: effective_additional_permissions
                .permissions_preapproved,
            justification,
            prefix_rule,
            monitor: monitor.clone(),
        };
        let result = match completion_timeout {
            Some(timeout) => {
                UnifiedExecProcessManager::exec_command_to_completion(request, &context, timeout)
                    .await
            }
            None => manager.exec_command(request, &context).await,
        };
        match result {
            Ok(response) => {
                if let Some(monitor) = monitor {
                    return Ok(boxed_tool_output(MonitorToolOutput {
                        monitor,
                        command: hook_command,
                    }));
                }
                Ok(boxed_tool_output(response))
            }
            Err(UnifiedExecError::SandboxDenied {
                output,
                original_token_count,
                output_omitted_bytes,
                ..
            }) => {
                let output_text = output.aggregated_output.text;
                if monitor.is_some() {
                    let detail = if output_text.is_empty() {
                        format!("process exited with code {}", output.exit_code)
                    } else {
                        output_text
                    };
                    return Err(FunctionCallError::RespondToModel(format!(
                        "monitor command was denied by the sandbox: {detail}"
                    )));
                }
                let original_token_count =
                    original_token_count.unwrap_or_else(|| approx_token_count(&output_text));
                Ok(boxed_tool_output(ExecCommandToolOutput {
                    event_call_id: context.call_id.clone(),
                    chunk_id: generate_chunk_id(),
                    wall_time: output.duration,
                    raw_output: output_text.into_bytes(),
                    truncation_policy: step_context.settings.model_info.truncation_policy.into(),
                    max_output_tokens,
                    // Sandbox denial is terminal, so there is no live
                    // process for write_stdin to resume.
                    process_id: None,
                    exit_code: Some(output.exit_code),
                    original_token_count: Some(original_token_count),
                    output_omitted_bytes,
                    hook_command: Some(hook_command),
                }))
            }
            Err(err) => {
                let message = format!("exec_command failed: {err:?}");
                Err(FunctionCallError::RespondToModel(truncate_middle_chars(
                    &message,
                    EXEC_COMMAND_REJECTION_MAX_BYTES,
                )))
            }
        }
    }
}

fn one_shot_exec_command_spec(spec: ToolSpec) -> ToolSpec {
    let ToolSpec::Function(mut spec) = spec else {
        unreachable!("exec_command has a function schema");
    };
    spec.description = spec.description.replacen(
        "Runs a command in a PTY, returning output or a session ID for ongoing interaction.",
        "Runs a command to completion and returns its output. The process is terminated on timeout or cancellation and cannot be resumed.",
        1,
    );
    let properties = spec.parameters.properties.get_or_insert_default();
    properties.remove("tty");
    properties.remove("yield_time_ms");
    properties.insert(
        "timeout_ms".to_string(),
        JsonSchema::number(Some(
            "Maximum command runtime. Defaults to 10000 ms.".to_string(),
        )),
    );
    spec.output_schema = spec.output_schema.map(|schema| {
        let mut schema = schema.into_value();
        if let Some(output_properties) = schema
            .get_mut("properties")
            .and_then(serde_json::Value::as_object_mut)
        {
            output_properties.remove("session_id");
        }
        schema.into()
    });
    ToolSpec::Function(spec)
}

fn default_monitor_timeout_ms() -> u64 {
    300_000
}

pub(super) fn effective_monitor_timeout_ms(
    timeout_ms: u64,
    persistent: bool,
) -> Result<u64, String> {
    if timeout_ms < 1_000 {
        return Err(if persistent {
            "monitor timeout_ms must be at least 1000"
        } else {
            "monitor timeout_ms must be between 1000 and 3600000"
        }
        .to_string());
    }
    if !persistent && timeout_ms > 3_600_000 {
        return Err("monitor timeout_ms must be between 1000 and 3600000".to_string());
    }
    Ok(if persistent { 0 } else { timeout_ms })
}

fn monitor_command_has_hidden_control_chars(command: &str) -> bool {
    command.chars().any(|character| {
        let codepoint = u32::from(character);
        (codepoint < 0x20 && character != '\t' && character != '\n')
            || (0x7f..=0x9f).contains(&codepoint)
    })
}

impl CoreToolRuntime for ExecCommandHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };

        let command = if self.options.monitor {
            parse_arguments::<MonitorArgs>(arguments)
                .ok()
                .map(|args| args.command)
        } else {
            parse_arguments::<ExecCommandArgs>(arguments)
                .ok()
                .map(|args| args.cmd)
        };
        command.map(|command| PreToolUsePayload {
            tool_name: HookToolName::bash(),
            tool_input: serde_json::json!({ "command": command }),
        })
    }

    fn with_updated_hook_input(
        &self,
        mut invocation: ToolInvocation,
        updated_input: serde_json::Value,
    ) -> Result<ToolInvocation, FunctionCallError> {
        let ToolPayload::Function { arguments } = invocation.payload else {
            return Err(FunctionCallError::RespondToModel(
                "hook input rewrite received unsupported exec_command payload".to_string(),
            ));
        };
        invocation.payload = ToolPayload::Function {
            arguments: rewrite_function_string_argument(
                &arguments,
                if self.options.monitor {
                    "monitor"
                } else {
                    "exec_command"
                },
                if self.options.monitor {
                    "command"
                } else {
                    "cmd"
                },
                updated_hook_command(&updated_input)?,
            )?,
        };
        Ok(invocation)
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &dyn crate::tools::context::ToolOutput,
    ) -> Option<PostToolUsePayload> {
        post_unified_exec_tool_use_payload(invocation, result)
    }
}

fn emit_unified_exec_tty_metric(session_telemetry: &SessionTelemetry, tty: bool) {
    session_telemetry.counter(
        TOOL_CALL_UNIFIED_EXEC_METRIC,
        /*inc*/ 1,
        &[("tty", if tty { "true" } else { "false" })],
    );
}

#[cfg(test)]
mod monitor_tests {
    use super::CommandMonitorInfo;
    use super::MonitorToolOutput;
    use super::monitor_command_has_hidden_control_chars;
    use crate::tools::context::ToolOutput;
    use crate::tools::context::ToolPayload;

    #[test]
    fn command_control_character_validation_matches_monitor_contract() {
        assert!(!monitor_command_has_hidden_control_chars(
            "printf 'one\\ntwo\\t三😀'"
        ));
        assert!(!monitor_command_has_hidden_control_chars(
            "literal\nline\tvalue"
        ));
        for rejected in ['\0', '\r', '\u{001b}', '\u{007f}', '\u{0085}', '\u{009f}'] {
            assert!(
                monitor_command_has_hidden_control_chars(&format!("before{rejected}after")),
                "expected U+{:04X} to be rejected",
                u32::from(rejected)
            );
        }
    }

    #[test]
    fn monitor_start_output_has_text_and_structured_contracts() {
        let output = MonitorToolOutput {
            monitor: CommandMonitorInfo {
                task_id: "b1234abcd".to_string(),
                description: "build".to_string(),
                timeout_ms: 300_000,
                persistent: false,
            },
            command: "make watch".to_string(),
        };
        let payload = ToolPayload::Function {
            arguments: serde_json::json!({
                "command": "make watch",
                "description": "build"
            })
            .to_string(),
        };

        assert_eq!(
            output.response_text(),
            "Monitor started (task b1234abcd, timeout 300000ms). You will be notified on each event. Keep working — do not poll or sleep. Events may arrive while you are waiting for the user — an event is not their reply."
        );
        assert_eq!(
            output.code_mode_result(&payload),
            serde_json::json!({
                "taskId": "b1234abcd",
                "timeoutMs": 300_000,
                "persistent": false
            })
        );
        assert_eq!(
            output.post_tool_use_input(&payload),
            Some(serde_json::json!({ "command": "make watch" }))
        );
    }
}
