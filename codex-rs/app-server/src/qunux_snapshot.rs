use codex_app_server_protocol::QunuxSnapshotNotification;
use serde_json::Value as JsonValue;
use std::fs;
use std::path::Path;

pub(crate) fn snapshot_notification_for_workspace(
    thread_id: &str,
    workspace: &Path,
    qunux_enabled: bool,
) -> Option<QunuxSnapshotNotification> {
    if !qunux_enabled {
        return None;
    }

    let processes_dir = workspace.join(".qunux/processes");
    let entries = fs::read_dir(processes_dir).ok()?;
    let mut candidates = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }

        let state_path = entry.path().join("closure.json");
        let Ok(raw) = fs::read_to_string(&state_path) else {
            continue;
        };
        let Ok(snapshot) = serde_json::from_str::<JsonValue>(&raw) else {
            continue;
        };
        let Some(qunux_thread_id) = bound_qunux_thread_id(&snapshot, thread_id) else {
            continue;
        };
        let process_id = snapshot
            .get("process_id")
            .and_then(JsonValue::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| entry.file_name().to_string_lossy().to_string());
        candidates.push(QunuxSnapshotNotification {
            thread_id: thread_id.to_string(),
            process_id,
            qunux_thread_id,
            state_path: Some(state_path.display().to_string()),
            snapshot,
        });
    }

    candidates.sort_by(|left, right| {
        left.process_id
            .cmp(&right.process_id)
            .then_with(|| left.qunux_thread_id.cmp(&right.qunux_thread_id))
    });
    candidates.into_iter().next()
}

fn bound_qunux_thread_id(snapshot: &JsonValue, actor_session_id: &str) -> Option<String> {
    let threads = snapshot.get("threads").and_then(JsonValue::as_object)?;
    let mut matches = Vec::new();
    for (map_id, thread) in threads {
        let actor_matches = string_field(thread, "actor_session_id") == Some(actor_session_id)
            || string_field(thread, "codex_thread_id") == Some(actor_session_id);
        if actor_matches {
            matches.push(
                string_field(thread, "id")
                    .unwrap_or(map_id.as_str())
                    .to_string(),
            );
        }
    }
    matches.sort();
    matches.into_iter().next()
}

fn string_field<'a>(value: &'a JsonValue, field: &str) -> Option<&'a str> {
    value.get(field).and_then(JsonValue::as_str)
}

#[cfg(test)]
mod tests {
    use super::snapshot_notification_for_workspace;
    use serde_json::json;
    use std::fs;

    #[test]
    fn builds_snapshot_for_bound_qunux_thread() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join(".qunux/processes/QP123");
        fs::create_dir_all(&state_dir).expect("create state dir");
        fs::write(
            state_dir.join("closure.json"),
            serde_json::to_string_pretty(&json!({
                "schema_version": 4,
                "process_id": "QP123",
                "main_thread_id": "QT000",
                "threads": {
                    "QT000": {
                        "id": "QT000",
                        "actor_session_id": "thread-1",
                        "codex_thread_id": "thread-1"
                    }
                },
                "problems": {}
            }))
            .expect("state json"),
        )
        .expect("write state");

        let notification =
            snapshot_notification_for_workspace("thread-1", temp.path(), true).expect("snapshot");

        assert_eq!(notification.thread_id, "thread-1");
        assert_eq!(notification.process_id, "QP123");
        assert_eq!(notification.qunux_thread_id, "QT000");
        assert!(
            notification
                .state_path
                .as_deref()
                .expect("state path")
                .ends_with(".qunux/processes/QP123/closure.json")
        );
        assert_eq!(notification.snapshot["process_id"], "QP123");
    }

    #[test]
    fn disabled_qunux_does_not_emit_snapshot() {
        let temp = tempfile::tempdir().expect("tempdir");

        let notification = snapshot_notification_for_workspace("thread-1", temp.path(), false);

        assert!(notification.is_none());
    }

    #[test]
    fn unbound_state_does_not_emit_snapshot() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().join(".qunux/processes/QP123");
        fs::create_dir_all(&state_dir).expect("create state dir");
        fs::write(
            state_dir.join("closure.json"),
            serde_json::to_string_pretty(&json!({
                "process_id": "QP123",
                "threads": {
                    "QT000": {
                        "id": "QT000",
                        "actor_session_id": "other-thread"
                    }
                }
            }))
            .expect("state json"),
        )
        .expect("write state");

        let notification = snapshot_notification_for_workspace("thread-1", temp.path(), true);

        assert!(notification.is_none());
    }
}
