use std::time::Instant;

use super::App;
use crate::terminal::state::TerminalSleep;

impl App {
    /// Stops every pane process in the workspace while keeping its tabs, splits, cwd and
    /// agent resume plan, so `wake_workspace` can bring it back as it was.
    pub(crate) fn sleep_workspace(&mut self, ws_idx: usize) -> bool {
        let panes = self.workspace_pane_terminals(ws_idx);
        let mut changed = false;
        for (pane_id, terminal_id) in panes {
            let Some(runtime) = self.terminal_runtimes.remove(&terminal_id) else {
                continue;
            };
            let cwd = runtime.cwd_for_persistence();
            if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                if let Some(cwd) = cwd {
                    terminal.cwd = cwd;
                }
                // Read the plan before the exit below releases the agent's hook authority.
                terminal.pending_agent_resume_plan = sleep_resume_plan(terminal);
                terminal.sleep = TerminalSleep::Asleep;
                terminal.revision = terminal.revision.saturating_add(1);
            }
            *self.pending_sleep_runtime_exits.entry(pane_id).or_default() += 1;
            runtime.shutdown();

            let previous_toast = self.state.toast.clone();
            if let Some(update) = self.state.publish_pane_process_exit_if_agent(pane_id, true) {
                self.sync_full_lifecycle_authority_detection_pauses();
                self.refresh_new_herdr_toast_context_for_update(&update, &previous_toast);
                self.emit_pane_state_update(&update);
            }
            changed = true;
        }
        if changed {
            self.finish_sleep_change();
        }
        changed
    }

    /// Marks sleeping panes for respawn; the deferred agent resume path starts them.
    pub(crate) fn wake_workspace(&mut self, ws_idx: usize) -> bool {
        let panes = self.workspace_pane_terminals(ws_idx);
        let mut changed = false;
        for (_, terminal_id) in panes {
            if let Some(terminal) = self
                .state
                .terminals
                .get_mut(&terminal_id)
                .filter(|terminal| terminal.sleep == TerminalSleep::Asleep)
            {
                terminal.sleep = TerminalSleep::Waking;
                terminal.revision = terminal.revision.saturating_add(1);
                changed = true;
            }
        }
        if changed {
            self.sync_pending_agent_resume_deadline(Instant::now());
            self.finish_sleep_change();
        }
        changed
    }

    pub(crate) fn workspace_is_asleep(&self, ws_idx: usize) -> bool {
        self.state
            .terminal_ids_for_workspace(ws_idx)
            .iter()
            .filter_map(|terminal_id| self.state.terminals.get(terminal_id))
            .any(|terminal| terminal.sleep == TerminalSleep::Asleep)
    }

    fn workspace_pane_terminals(
        &self,
        ws_idx: usize,
    ) -> Vec<(crate::layout::PaneId, crate::terminal::TerminalId)> {
        self.state
            .pane_ids_for_workspace(ws_idx)
            .into_iter()
            .filter_map(|pane_id| {
                self.state
                    .terminal_id_for_pane(ws_idx, pane_id)
                    .map(|terminal_id| (pane_id, terminal_id))
            })
            .collect()
    }

    fn finish_sleep_change(&mut self) {
        self.schedule_session_save();
        self.render_dirty.request_generic();
        self.render_notify.notify_one();
    }
}

/// The command that resumes the pane's agent session, taken from the same sources
/// session persistence uses.
fn sleep_resume_plan(
    terminal: &crate::terminal::TerminalState,
) -> Option<crate::agent_resume::AgentResumePlan> {
    if let Some(resume) = terminal
        .reported_resume()
        .filter(|resume| crate::agent_resume::validate_resume_argv(&resume.argv).is_ok())
    {
        return Some(resume.plan(&terminal.cwd));
    }
    if let Some(authority) = terminal.hook_authority.as_ref() {
        if let Some(session_ref) = authority.session_ref.as_ref() {
            return crate::agent_resume::plan(
                &authority.source,
                &authority.agent_label,
                session_ref,
            );
        }
    }
    terminal
        .persisted_agent_session
        .as_ref()
        .and_then(|session| {
            crate::agent_resume::plan(&session.source, &session.agent, &session.session_ref)
        })
}
