//! Monitor registrations and archive cleanup, backed by the shared process store.

use super::MonitorCompletionClaim;
use super::monitor_process_has_fully_exited;
use super::unregister_network_approval_for_entry;
use crate::unified_exec::MonitorStopReason;
use crate::unified_exec::MonitorTaskStatus;
use crate::unified_exec::TerminateMonitorResult;
use crate::unified_exec::UnifiedExecProcess;
use crate::unified_exec::UnifiedExecProcessManager;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

pub(super) const MONITOR_WORKER_SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(2);

impl UnifiedExecProcessManager {
    pub(super) fn register_pending_monitor_process(
        &self,
        process_id: i32,
        process: Arc<UnifiedExecProcess>,
    ) {
        let mut pending = self
            .pending_monitor_processes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match pending.entry(process_id) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(process);
            }
            std::collections::hash_map::Entry::Occupied(_) => {
                panic!("allocated monitor process id must not already have a pending owner")
            }
        }
    }

    pub(super) fn remove_pending_monitor_process_if_matches(
        &self,
        process_id: i32,
        expected_process: &Arc<UnifiedExecProcess>,
    ) -> bool {
        let mut pending = self
            .pending_monitor_processes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !pending
            .get(&process_id)
            .is_some_and(|process| Arc::ptr_eq(process, expected_process))
        {
            return false;
        }
        pending.remove(&process_id);
        true
    }

    pub(super) fn pending_monitor_processes_snapshot(&self) -> Vec<(i32, Arc<UnifiedExecProcess>)> {
        self.pending_monitor_processes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(process_id, process)| (*process_id, Arc::clone(process)))
            .collect()
    }

    /// Releases delayed process ownership only when the id still names the
    /// expected Arc. This prevents a late cleanup from deleting an ABA-reused
    /// ProcessEntry.
    pub(super) async fn release_process_id_if_matches(
        &self,
        process_id: i32,
        expected_process: &Arc<UnifiedExecProcess>,
    ) {
        let (removed, stop_purposes) = {
            let mut store = self.process_store.lock().await;
            let matching_tasks = store
                .monitor_tasks
                .iter()
                .filter(|(_, task)| {
                    task.process_id == process_id && Arc::ptr_eq(&task.process, expected_process)
                })
                .map(|(task_id, task)| (task_id.clone(), task.purpose.clone()))
                .collect::<Vec<_>>();
            for (task_id, _) in &matching_tasks {
                store.monitor_tasks.remove(task_id);
                store
                    .monitor_statuses
                    .insert(task_id.clone(), MonitorTaskStatus::Killed);
            }
            let entry_matches = store
                .processes
                .get(&process_id)
                .is_some_and(|entry| Arc::ptr_eq(&entry.process, expected_process));
            let removed = entry_matches.then(|| store.remove(process_id)).flatten();
            let pending_removed =
                self.remove_pending_monitor_process_if_matches(process_id, expected_process);
            if pending_removed
                && !super::should_use_deterministic_process_ids()
                && !store.processes.contains_key(&process_id)
                && !store
                    .monitor_tasks
                    .values()
                    .any(|task| task.process_id == process_id)
            {
                store.reserved_process_ids.remove(&process_id);
            }
            (
                removed,
                matching_tasks
                    .into_iter()
                    .map(|(_, purpose)| purpose)
                    .collect::<Vec<_>>(),
            )
        };
        for purpose in stop_purposes {
            purpose.request_monitor_stop(MonitorStopReason::SessionShutdown);
        }
        if let Some(entry) = removed {
            unregister_network_approval_for_entry(&entry).await;
        }
    }

    /// Gives monitors whose first session-shutdown termination attempt failed
    /// one checked retry before archive cleanup decides which task state must
    /// remain intact. The stop guard makes the retry mutually exclusive with a
    /// natural-completion claim.
    pub(super) async fn retry_unconfirmed_monitor_terminations_for_cleanup(&self) {
        let candidates = {
            let store = self.process_store.lock().await;
            store
                .monitor_tasks
                .iter()
                .map(|(task_id, task)| {
                    (
                        task_id.clone(),
                        task.clone(),
                        task.process.begin_monitor_stop(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let stopped = futures::future::join_all(candidates.iter().map(|(_, task, _)| async move {
            monitor_process_has_fully_exited(&task.process)
                || task.process.terminate_confirmed().await.is_ok()
        }))
        .await;

        for ((task_id, task, stop_guard), stopped) in candidates.into_iter().zip(stopped) {
            if !stopped {
                warn!(
                    task_id = %task_id,
                    "command monitor remained live after shutdown termination retry"
                );
                drop(stop_guard);
                continue;
            }

            task.purpose
                .request_monitor_stop(MonitorStopReason::SessionShutdown);
            let entry = {
                let mut store = self.process_store.lock().await;
                let matches = store
                    .monitor_tasks
                    .get(&task_id)
                    .is_some_and(|registered| Arc::ptr_eq(&registered.process, &task.process));
                if !matches {
                    None
                } else {
                    store
                        .monitor_statuses
                        .insert(task_id.clone(), MonitorTaskStatus::Killed);
                    store.monitor_tasks.remove(&task_id);

                    store
                        .processes
                        .get(&task.process_id)
                        .is_some_and(|entry| Arc::ptr_eq(&entry.process, &task.process))
                        .then(|| store.remove(task.process_id))
                        .flatten()
                }
            };
            drop(stop_guard);
            if let Some(entry) = entry {
                unregister_network_approval_for_entry(&entry).await;
            }
        }

        let pending = self.pending_monitor_processes_snapshot();
        let stopped = futures::future::join_all(
            pending
                .iter()
                .map(|(_, process)| async move { process.terminate_confirmed().await.is_ok() }),
        )
        .await;
        for ((process_id, process), stopped) in pending.into_iter().zip(stopped) {
            if stopped {
                self.release_process_id_if_matches(process_id, &process)
                    .await;
            } else {
                warn!(
                    process_id,
                    "pending command monitor remained live after shutdown termination retry"
                );
            }
        }
    }

    /// Remove command-monitor archives after the session has finished using
    /// their model-visible paths. This is intentionally separate from
    /// `terminate_all_processes`: the background-terminal clean API may run
    /// while the session is still active.
    pub(crate) async fn cleanup_monitor_output_files(&self) {
        self.retry_unconfirmed_monitor_terminations_for_cleanup()
            .await;

        let candidates = {
            let store = self.process_store.lock().await;
            // Any remaining registration was not confirmed stopped, regardless
            // of its diagnostic status. A running status without a registration
            // is also retained conservatively so cleanup never turns a partial
            // startup into an untracked process.
            let retained_task_ids = store
                .monitor_tasks
                .keys()
                .cloned()
                .chain(
                    store
                        .monitor_statuses
                        .iter()
                        .filter(|(_, status)| **status == MonitorTaskStatus::Running)
                        .map(|(task_id, _)| task_id.clone()),
                )
                .collect::<HashSet<_>>();

            let removable_task_ids = store
                .monitor_output_files
                .keys()
                .chain(store.monitor_workers.keys())
                .filter(|task_id| !retained_task_ids.contains(*task_id))
                .cloned()
                .collect::<HashSet<_>>();
            removable_task_ids
                .into_iter()
                .map(|task_id| {
                    let output_file = store.monitor_output_files.get(&task_id).cloned();
                    let worker = store.monitor_workers.get(&task_id).cloned();
                    (task_id, output_file, worker)
                })
                .collect::<Vec<_>>()
        };
        let wait_for_workers = async {
            for (_, _, worker) in &candidates {
                if let Some(worker) = worker {
                    worker.done.cancelled().await;
                }
            }
        };
        if tokio::time::timeout(MONITOR_WORKER_SHUTDOWN_GRACE_PERIOD, wait_for_workers)
            .await
            .is_err()
        {
            warn!("timed out waiting for command monitor workers during session shutdown");
            for (_, _, worker) in &candidates {
                if let Some(worker) = worker
                    && !worker.done.is_cancelled()
                {
                    worker.abort_handle.abort();
                }
            }
            let wait_after_abort = async {
                for (_, _, worker) in &candidates {
                    if let Some(worker) = worker {
                        worker.done.cancelled().await;
                    }
                }
            };
            if tokio::time::timeout(MONITOR_WORKER_SHUTDOWN_GRACE_PERIOD, wait_after_abort)
                .await
                .is_err()
            {
                warn!("command monitor workers remained live after bounded abort wait");
            }
        }

        for (task_id, output_file, worker) in candidates {
            if worker
                .as_ref()
                .is_some_and(|worker| !worker.done.is_cancelled())
            {
                continue;
            }
            let file_removed = match output_file.as_ref() {
                None => true,
                Some(path) => match tokio::fs::remove_file(path).await {
                    Ok(()) => true,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
                    Err(err) => {
                        warn!(path = %path.display(), error = %err, "failed to remove command monitor output archive");
                        false
                    }
                },
            };
            let mut store = self.process_store.lock().await;
            if let Some(expected_worker) = worker.as_ref()
                && store
                    .monitor_workers
                    .get(&task_id)
                    .is_some_and(|current| Arc::ptr_eq(current, expected_worker))
            {
                store.monitor_workers.remove(&task_id);
            }
            if file_removed
                && output_file.as_ref().is_some_and(|expected| {
                    store.monitor_output_files.get(&task_id) == Some(expected)
                })
            {
                store.monitor_output_files.remove(&task_id);
            }
        }

        let output_dirs = {
            let store = self.process_store.lock().await;
            let retained_dirs = store
                .monitor_output_files
                .values()
                .filter_map(|path| path.parent().map(PathBuf::from))
                .collect::<HashSet<_>>();
            store
                .monitor_output_dirs
                .iter()
                .filter(|path| !retained_dirs.contains(*path))
                .cloned()
                .collect::<Vec<_>>()
        };
        for output_dir in output_dirs {
            let removed = match tokio::fs::remove_dir(&output_dir).await {
                Ok(()) => true,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
                Err(err) => {
                    warn!(path = %output_dir.display(), error = %err, "failed to remove command monitor archive directory");
                    false
                }
            };
            if removed {
                self.process_store
                    .lock()
                    .await
                    .monitor_output_dirs
                    .remove(&output_dir);
            }
        }
    }

    pub(crate) async fn terminate_monitor(&self, task_id: &str) -> TerminateMonitorResult {
        let candidate = {
            let store = self.process_store.lock().await;
            match store.monitor_statuses.get(task_id).copied() {
                Some(MonitorTaskStatus::Running) => {}
                Some(status) => return TerminateMonitorResult::NotRunning(status),
                None => return TerminateMonitorResult::NotFound,
            }
            store.monitor_tasks.get(task_id).and_then(|task| {
                let info = task.purpose.monitor_info()?.clone();
                Some((task.clone(), info, task.process.begin_monitor_stop()))
            })
        };
        let Some((task, info, stop_guard)) = candidate else {
            return TerminateMonitorResult::NotFound;
        };
        let monitor_fully_finished = monitor_process_has_fully_exited(&task.process);
        if !monitor_fully_finished && task.process.terminate_confirmed().await.is_err() {
            drop(stop_guard);
            return TerminateMonitorResult::StopFailed;
        }

        task.purpose.request_monitor_stop(MonitorStopReason::User);
        let entry = {
            let mut store = self.process_store.lock().await;
            if !store
                .monitor_tasks
                .get(task_id)
                .is_some_and(|registered| Arc::ptr_eq(&registered.process, &task.process))
            {
                None
            } else {
                store
                    .monitor_statuses
                    .insert(task_id.to_string(), MonitorTaskStatus::Killed);
                store.monitor_tasks.remove(task_id);
                self.remove_pending_monitor_process_if_matches(task.process_id, &task.process);
                let removable = store
                    .processes
                    .get(&task.process_id)
                    .is_some_and(|entry| Arc::ptr_eq(&entry.process, &task.process));
                removable.then(|| store.remove(task.process_id)).flatten()
            }
        };
        drop(stop_guard);
        if let Some(entry) = entry {
            unregister_network_approval_for_entry(&entry).await;
        }
        TerminateMonitorResult::Stopped {
            info,
            command: task.command,
        }
    }

    /// Atomically claims completion before publishing its terminal notification.
    /// A concurrent stop attempt returns `StopPending`; the worker waits for that
    /// attempt to resolve and retries if the task is still running.
    pub(crate) async fn claim_monitor_completion(
        &self,
        task_id: &str,
        expected_process: &Arc<UnifiedExecProcess>,
        status: MonitorTaskStatus,
    ) -> MonitorCompletionClaim {
        let entry = {
            let mut store = self.process_store.lock().await;
            if store.monitor_statuses.get(task_id) != Some(&MonitorTaskStatus::Running) {
                return MonitorCompletionClaim::NotRunning;
            }
            let Some(task) = store.monitor_tasks.get(task_id) else {
                return MonitorCompletionClaim::NotRunning;
            };
            if !Arc::ptr_eq(&task.process, expected_process) {
                return MonitorCompletionClaim::NotRunning;
            }
            if expected_process.is_monitor_stop_pending() {
                return MonitorCompletionClaim::StopPending;
            }
            let process_id = task.process_id;
            store.monitor_tasks.remove(task_id);
            let entry = store
                .processes
                .get(&process_id)
                .is_some_and(|entry| Arc::ptr_eq(&entry.process, expected_process))
                .then(|| store.remove(process_id))
                .flatten();
            store.monitor_statuses.insert(task_id.to_string(), status);
            entry
        };
        if let Some(entry) = entry {
            unregister_network_approval_for_entry(&entry).await;
        }
        MonitorCompletionClaim::Claimed
    }

    /// Removes a monitor that has reached a terminal state without requiring a
    /// later `write_stdin` poll. A pending stop is allowed to resolve first, then
    /// the task/process identity and running authority are checked again before
    /// this fallback may publish a terminal status.
    pub(crate) async fn reap_monitor(
        &self,
        task_id: &str,
        expected_process: &Arc<UnifiedExecProcess>,
        status: MonitorTaskStatus,
    ) {
        let entry = loop {
            {
                let mut store = self.process_store.lock().await;
                if store.monitor_statuses.get(task_id) != Some(&MonitorTaskStatus::Running) {
                    return;
                }
                let Some(process_id) = store
                    .monitor_tasks
                    .get(task_id)
                    .filter(|task| Arc::ptr_eq(&task.process, expected_process))
                    .map(|task| task.process_id)
                else {
                    return;
                };
                if !expected_process.is_monitor_stop_pending() {
                    store.monitor_tasks.remove(task_id);
                    let entry = store
                        .processes
                        .get(&process_id)
                        .is_some_and(|entry| Arc::ptr_eq(&entry.process, expected_process))
                        .then(|| store.remove(process_id))
                        .flatten();
                    store.monitor_statuses.insert(task_id.to_string(), status);
                    break entry;
                }
            }
            expected_process.wait_for_monitor_stop_resolution().await;
        };
        if let Some(entry) = entry {
            unregister_network_approval_for_entry(&entry).await;
        }
    }
}
