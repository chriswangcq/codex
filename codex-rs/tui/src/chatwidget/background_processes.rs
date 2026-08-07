//! Background-process manager construction for `ChatWidget`.

use super::*;
use crate::bottom_pane::BackgroundProcessItem;

impl ChatWidget {
    fn background_process_items(&self) -> Vec<BackgroundProcessItem> {
        self.unified_exec_processes
            .iter()
            .map(|process| BackgroundProcessItem {
                process_id: process.key.clone(),
                command_display: process.command_display.clone(),
                recent_output: process.recent_chunks.clone(),
                monitor: process.monitor_info().cloned(),
                started_at: process.started_at,
            })
            .collect()
    }

    pub(super) fn show_background_processes(&mut self) {
        let items = self.background_process_items();
        self.bottom_pane.show_background_processes(items);
    }

    pub(super) fn sync_background_processes_view(&mut self) {
        let items = self.background_process_items();
        self.bottom_pane.update_background_processes(items);
    }

    pub(crate) fn update_background_monitor_output(
        &mut self,
        response: crate::app_event::BackgroundMonitorOutputResponse,
    ) {
        self.bottom_pane.update_background_monitor_output(response);
    }

    pub(crate) fn update_background_process_stop(
        &mut self,
        process_id: &str,
        result: Result<bool, String>,
    ) {
        self.bottom_pane
            .update_background_process_stop(process_id, result);
    }
}
