use codex_tools::JsonSchema;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

use super::QunuxOperation;

pub const QUNUX_NAMESPACE: &str = "qunux";

pub fn create_qunux_tool(operation: QunuxOperation) -> ToolSpec {
    ToolSpec::Namespace(ResponsesApiNamespace {
        name: QUNUX_NAMESPACE.to_string(),
        description: "Native Qunux Agent OS tools for durable task closure. Qunux does not replace LLM reasoning; it makes the LLM CPU's work runnable, resumable, auditable, recoverable, and structurally closed.".to_string(),
        tools: vec![ResponsesApiNamespaceTool::Function(tool_for_operation(operation))],
    })
}

fn tool_for_operation(operation: QunuxOperation) -> ResponsesApiTool {
    let (description, parameters) = match operation {
        QunuxOperation::Current => (
            "Get or initialize the current Qunux process/thread closure state.",
            object(
                [
                    optional_string(
                        "title",
                        "Optional root title used only when initializing a new Qunux state.",
                    ),
                    optional_string(
                        "body",
                        "Optional root Markdown body used only when initializing a new Qunux state.",
                    ),
                ],
                [],
            ),
        ),
        QunuxOperation::Next => (
            "Return the next concrete Qunux action for the current process/thread.",
            object([], []),
        ),
        QunuxOperation::CreateProblem => (
            "Create a child problem from a source ticket. Use mode=split for plan-time children from a splitting split ticket; use mode=spawn for runtime child problems from an executing one_go ticket.",
            object(
                [
                    string("parent_id", "Parent problem id, for example P000."),
                    string("from_ticket_id", "Ticket id that owns the child creation."),
                    string_enum(
                        "mode",
                        "Child creation mode: split is plan-time decomposition; spawn is runtime subprogram creation during execution.",
                        ["split", "spawn"],
                    ),
                    string("title", "Child problem title."),
                    string("body", "Child problem Markdown body."),
                ],
                ["parent_id", "from_ticket_id", "mode", "title", "body"],
            ),
        ),
        QunuxOperation::CreateTicket => (
            "Create the single solution ticket for a problem.",
            object(
                [
                    string("problem_id", "Problem id, for example P000."),
                    string("title", "Ticket title."),
                    string("body", "Ticket Markdown body."),
                ],
                ["problem_id", "title", "body"],
            ),
        ),
        QunuxOperation::ClassifyTicket => (
            "Classify a defined ticket as one_go or split.",
            object(
                [
                    string("ticket_id", "Ticket id, for example T000."),
                    string_enum(
                        "classification",
                        "Classification for the ticket.",
                        ["one_go", "split"],
                    ),
                    string("reason", "Concrete classification reason."),
                ],
                ["ticket_id", "classification", "reason"],
            ),
        ),
        QunuxOperation::SetStatus => (
            "Perform a legal preparatory status transition.",
            object(
                [
                    string_enum("kind", "Entity kind.", ["problem", "ticket"]),
                    string("id", "Problem or ticket id."),
                    string(
                        "status",
                        "Allowed status: problem -> doing; ticket -> executing or splitting.",
                    ),
                ],
                ["kind", "id", "status"],
            ),
        ),
        QunuxOperation::Result => (
            "Record the result body for the current ticket and mark that ticket done.",
            object(
                [
                    string("ticket_id", "Ticket id."),
                    string("title", "Result title."),
                    string("body", "Result Markdown body."),
                ],
                ["ticket_id", "title", "body"],
            ),
        ),
        QunuxOperation::Check => (
            "Run problem-level check_success. Use not_success with followup_title and followup_body when gaps remain.",
            object(
                [
                    string("problem_id", "Problem id."),
                    string_enum("status", "Check status.", ["success", "not_success"]),
                    array_of_strings("result_ids", "Result ids considered by this check."),
                    string("title", "Check title."),
                    string("body", "Check Markdown body."),
                    optional_string("followup_title", "Required when status is not_success."),
                    optional_string("followup_body", "Required when status is not_success."),
                ],
                ["problem_id", "status", "result_ids", "title", "body"],
            ),
        ),
        QunuxOperation::SpawnThread => (
            "Spawn a Qunux child thread bound to a problem subtree and fork a Codex child agent. For fuzzy or semantic waits, use this same ordinary child-thread mechanism with a watcher bootstrap; do not invent a semantic wait kernel primitive.",
            object(
                [
                    string(
                        "problem_id",
                        "Problem subtree root to bind to the child thread.",
                    ),
                    optional_string(
                        "bootstrap_instruction",
                        "Optional instruction appended to the child agent bootstrap. For watcher child threads, include the semantic goal, criteria, signals to inspect, evidence required, interval or trigger policy, budget/deadline, and escalation rule.",
                    ),
                    string_enum(
                        "context_policy",
                        "Context fork policy.",
                        ["full_context", "summary_context", "fresh_context"],
                    ),
                    optional_string("agent_type", "Optional Codex subagent role/type."),
                ],
                ["problem_id"],
            ),
        ),
        QunuxOperation::Wait => (
            "Park the current Qunux thread on typed wait specs. This is the only model-facing wait syscall; wake, consume, cancel, and inspect are runtime/host responsibilities.",
            object(
                [
                    string_enum("mode", "Wait mode.", ["all", "any"]),
                    string("reason", "Human-readable reason for parking the thread."),
                    (
                        "specs",
                        JsonSchema::array(
                            wait_spec_schema(),
                            Some("Wait specs that describe what can wake this thread.".to_string()),
                        ),
                    ),
                ],
                ["mode", "reason", "specs"],
            ),
        ),
        QunuxOperation::AckInbox => (
            "Acknowledge that the current Qunux thread has incorporated a pending inbox item into its response, wait, or next legal PTRC action. This is a state-only acknowledgement; it does not send a user-visible chat reply.",
            object(
                [
                    string("inbox_item_id", "Inbox item id, for example IN000."),
                    string(
                        "note",
                        "Brief note describing how the inbox item was handled or incorporated. Do not claim a user-visible reply unless the assistant actually emitted one in this turn.",
                    ),
                ],
                ["inbox_item_id", "note"],
            ),
        ),
        QunuxOperation::IngestUserTask => (
            "Convert an actionable pending inbox item into an ordinary Qunux child problem. Use this for real user work requests; for small talk, acknowledgements, or meta answers, reply visibly and use ack_inbox instead.",
            object(
                [
                    string("inbox_item_id", "Inbox item id, for example IN000."),
                    string("title", "Child problem title for the actionable user task."),
                    string(
                        "body",
                        "Child problem Markdown body with problem context and success criteria.",
                    ),
                ],
                ["inbox_item_id", "title", "body"],
            ),
        ),
        QunuxOperation::ScaffoldUserTask => (
            "Convert an actionable pending inbox item into an ordinary Qunux child problem and its default solution ticket. Use this when the user request is concrete enough to create both the problem body and first ticket body in one syscall; use ingest_user_task when the ticket must be authored separately.",
            object(
                [
                    string("inbox_item_id", "Inbox item id, for example IN000."),
                    string(
                        "problem_title",
                        "Child problem title for the actionable user task.",
                    ),
                    string(
                        "problem_body",
                        "Child problem Markdown body with problem context and success criteria.",
                    ),
                    string(
                        "ticket_title",
                        "Default solution ticket title for the child problem.",
                    ),
                    string(
                        "ticket_body",
                        "Default solution ticket Markdown body with proposed solution, acceptance criteria, verification plan, risks, and assumptions.",
                    ),
                    string(
                        "note",
                        "Brief note describing why this inbox item became a durable child problem and ticket.",
                    ),
                ],
                [
                    "inbox_item_id",
                    "problem_title",
                    "problem_body",
                    "ticket_title",
                    "ticket_body",
                    "note",
                ],
            ),
        ),
        QunuxOperation::JoinThread => (
            "Join a completed child Qunux thread back into the current parent thread.",
            object(
                [string(
                    "target_thread_id",
                    "Child Qunux thread id to join, for example QT001.",
                )],
                ["target_thread_id"],
            ),
        ),
        QunuxOperation::RecoverThread => (
            "Recover a failed child Qunux thread by returning its unfinished problem subtree to the current parent thread. Use only when qunux.next returns recover_thread.",
            object(
                [string(
                    "target_thread_id",
                    "Failed child Qunux thread id to recover, for example QT001.",
                )],
                ["target_thread_id"],
            ),
        ),
        QunuxOperation::ListThreads => (
            "List Qunux threads for the current process.",
            object([], []),
        ),
        QunuxOperation::ThreadStatus => (
            "Return one Qunux thread record.",
            object(
                [string(
                    "target_thread_id",
                    "Qunux thread id to inspect, for example QT001.",
                )],
                ["target_thread_id"],
            ),
        ),
        QunuxOperation::Status => (
            "Return Qunux status counts and validity for the current process/thread.",
            object([], []),
        ),
        QunuxOperation::Validate => ("Validate the current Qunux state.", object([], [])),
        QunuxOperation::Render => ("Render the current Qunux problem tree.", object([], [])),
    };

    ResponsesApiTool {
        name: operation.name().to_string(),
        description: description.to_string(),
        strict: false,
        defer_loading: None,
        parameters,
        output_schema: None,
    }
}

fn object<const N: usize, const M: usize>(
    properties: [(&'static str, JsonSchema); N],
    required: [&'static str; M],
) -> JsonSchema {
    JsonSchema::object(
        BTreeMap::from(properties.map(|(name, schema)| (name.to_string(), schema))),
        Some(required.into_iter().map(ToString::to_string).collect()),
        Some(false.into()),
    )
}

fn string(name: &'static str, description: &'static str) -> (&'static str, JsonSchema) {
    (name, JsonSchema::string(Some(description.to_string())))
}

fn optional_string(name: &'static str, description: &'static str) -> (&'static str, JsonSchema) {
    (name, JsonSchema::string(Some(description.to_string())))
}

fn string_enum<const N: usize>(
    name: &'static str,
    description: &'static str,
    values: [&'static str; N],
) -> (&'static str, JsonSchema) {
    (
        name,
        JsonSchema::string_enum(
            values.into_iter().map(|value| json!(value)).collect(),
            Some(description.to_string()),
        ),
    )
}

fn array_of_strings(name: &'static str, description: &'static str) -> (&'static str, JsonSchema) {
    (
        name,
        JsonSchema::array(JsonSchema::string(None), Some(description.to_string())),
    )
}

fn wait_spec_schema() -> JsonSchema {
    JsonSchema::object(
        BTreeMap::from([
            (
                "kind".to_string(),
                JsonSchema::string_enum(
                    ["user_input", "timer", "external_signal", "event_key"]
                        .into_iter()
                        .map(|value| json!(value))
                        .collect(),
                    Some("Wait spec kind.".to_string()),
                ),
            ),
            (
                "event_key".to_string(),
                event_key_schema("Exact event key for kind=event_key."),
            ),
            (
                "target_thread_id".to_string(),
                JsonSchema::string(Some(
                    "Optional target thread id; defaults to the current thread for passive specs."
                        .to_string(),
                )),
            ),
            (
                "condition".to_string(),
                JsonSchema::string(Some(
                    "Resource/condition to match, for example a user reply condition or timer window."
                        .to_string(),
                )),
            ),
            (
                "source".to_string(),
                JsonSchema::string(Some(
                    "Optional source to match, for example chat, timer, github, or webhook."
                        .to_string(),
                )),
            ),
            (
                "dedupe_key".to_string(),
                JsonSchema::string(Some(
                    "Optional idempotency key expected from the matching wake event.".to_string(),
                )),
            ),
        ]),
        Some(vec!["kind".to_string()]),
        Some(false.into()),
    )
}

fn event_key_schema(description: &'static str) -> JsonSchema {
    JsonSchema::object(
        BTreeMap::from([
            (
                "kind".to_string(),
                JsonSchema::string(Some(
                    "Event kind, for example user.input, timer, external.signal, or github.checks.completed."
                        .to_string(),
                )),
            ),
            (
                "resource".to_string(),
                JsonSchema::string(Some("Optional resource/condition key.".to_string())),
            ),
            (
                "source".to_string(),
                JsonSchema::string(Some("Optional event source.".to_string())),
            ),
            (
                "target_thread_id".to_string(),
                JsonSchema::string(Some("Optional target Qunux thread id.".to_string())),
            ),
        ]),
        Some(vec!["kind".to_string()]),
        Some(false.into()),
    )
    .with_description(description)
}

trait JsonSchemaDescriptionExt {
    fn with_description(self, description: &'static str) -> Self;
}

impl JsonSchemaDescriptionExt for JsonSchema {
    fn with_description(mut self, description: &'static str) -> Self {
        self.description = Some(description.to_string());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_namespace_spec_for_each_operation() {
        for operation in QunuxOperation::all() {
            let ToolSpec::Namespace(namespace) = create_qunux_tool(operation) else {
                panic!("expected namespace spec");
            };
            assert_eq!(namespace.name, QUNUX_NAMESPACE);
            assert_eq!(namespace.tools.len(), 1);
        }
    }

    #[test]
    fn public_schemas_do_not_accept_runtime_identity_fields() {
        for operation in QunuxOperation::all() {
            let ToolSpec::Namespace(namespace) = create_qunux_tool(operation) else {
                panic!("expected namespace spec");
            };
            let ResponsesApiNamespaceTool::Function(tool) = namespace.tools.first().expect("tool");
            let properties = tool
                .parameters
                .properties
                .as_ref()
                .expect("object properties");

            assert!(
                !properties.contains_key("process_id"),
                "{} must not expose process_id",
                operation.name()
            );
            assert!(
                !properties.contains_key("thread_id"),
                "{} must not expose thread_id",
                operation.name()
            );
        }
    }

    #[test]
    fn target_thread_resource_schemas_use_target_thread_id() {
        for operation in [
            QunuxOperation::JoinThread,
            QunuxOperation::RecoverThread,
            QunuxOperation::ThreadStatus,
        ] {
            let ToolSpec::Namespace(namespace) = create_qunux_tool(operation) else {
                panic!("expected namespace spec");
            };
            let ResponsesApiNamespaceTool::Function(tool) = namespace.tools.first().expect("tool");
            let properties = tool
                .parameters
                .properties
                .as_ref()
                .expect("object properties");
            let required = tool.parameters.required.as_ref().expect("required fields");

            assert!(
                properties.contains_key("target_thread_id"),
                "{} should accept target_thread_id",
                operation.name()
            );
            assert!(
                required.contains(&"target_thread_id".to_string()),
                "{} should require target_thread_id",
                operation.name()
            );
            assert!(
                !properties.contains_key("thread_id"),
                "{} must not expose current thread_id",
                operation.name()
            );
        }
    }

    #[test]
    fn spawn_thread_schema_does_not_expose_spawn_agent_switch() {
        let ToolSpec::Namespace(namespace) = create_qunux_tool(QunuxOperation::SpawnThread) else {
            panic!("expected namespace spec");
        };
        let ResponsesApiNamespaceTool::Function(tool) = namespace.tools.first().expect("tool");
        let properties = tool
            .parameters
            .properties
            .as_ref()
            .expect("object properties");

        assert!(
            !properties.contains_key("spawn_agent"),
            "qunux.spawn_thread must always create a Codex child agent"
        );
    }

    #[test]
    fn spawn_thread_schema_describes_watcher_bootstrap_pattern() {
        let ToolSpec::Namespace(namespace) = create_qunux_tool(QunuxOperation::SpawnThread) else {
            panic!("expected namespace spec");
        };
        let ResponsesApiNamespaceTool::Function(tool) = namespace.tools.first().expect("tool");
        let properties = tool
            .parameters
            .properties
            .as_ref()
            .expect("object properties");
        let bootstrap_description = properties["bootstrap_instruction"]
            .description
            .as_deref()
            .expect("bootstrap description");

        assert!(tool.description.contains("watcher bootstrap"));
        assert!(tool.description.contains("ordinary child-thread"));
        assert!(tool.description.contains("semantic wait kernel primitive"));
        assert!(bootstrap_description.contains("semantic goal"));
        assert!(bootstrap_description.contains("criteria"));
        assert!(bootstrap_description.contains("signals"));
        assert!(bootstrap_description.contains("evidence"));
        assert!(bootstrap_description.contains("escalation"));
    }

    #[test]
    fn wait_schema_is_park_only_agent_syscall() {
        let ToolSpec::Namespace(namespace) = create_qunux_tool(QunuxOperation::Wait) else {
            panic!("expected namespace spec");
        };
        let ResponsesApiNamespaceTool::Function(tool) = namespace.tools.first().expect("tool");
        let properties = tool
            .parameters
            .properties
            .as_ref()
            .expect("object properties");
        let required = tool.parameters.required.as_ref().expect("required fields");

        assert_eq!(tool.name, "wait");
        assert!(tool.description.contains("Park the current Qunux thread"));
        assert!(!properties.contains_key("op"));
        assert!(!properties.contains_key("handle_id"));
        assert!(!properties.contains_key("event"));
        assert!(properties.contains_key("mode"));
        assert!(properties.contains_key("reason"));
        assert!(properties.contains_key("specs"));
        assert_eq!(
            required,
            &vec![
                "mode".to_string(),
                "reason".to_string(),
                "specs".to_string()
            ]
        );
    }

    #[test]
    fn ack_inbox_schema_warns_that_ack_is_state_only() {
        let ToolSpec::Namespace(namespace) = create_qunux_tool(QunuxOperation::AckInbox) else {
            panic!("expected namespace spec");
        };
        let ResponsesApiNamespaceTool::Function(tool) = namespace.tools.first().expect("tool");
        let properties = tool
            .parameters
            .properties
            .as_ref()
            .expect("object properties");
        let note_description = properties["note"]
            .description
            .as_deref()
            .expect("note description");

        assert!(tool.description.contains("state-only"));
        assert!(
            tool.description
                .contains("does not send a user-visible chat reply")
        );
        assert!(note_description.contains("Do not claim a user-visible reply"));
    }
}
