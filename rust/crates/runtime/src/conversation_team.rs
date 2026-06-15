//! Multi-role team-convergence node execution, extracted from `conversation.rs`
//! to keep that module focused. This is a child module of `conversation`, so it
//! can access the private fields and methods of [`ConversationRuntime`] that the
//! convergence loop drives (api client, task registry, sibling sub-turn and
//! verification helpers).
//!
//! See [`crate::team_convergence`] for the pure, unit-tested state machine; the
//! methods here are the imperative runtime binding (sequential `&mut self`
//! sub-turns, which three simultaneous closures could not hold).

use super::{
    current_time_millis, ApiClient, ApiRequest, AssistantEvent, ConversationMessage,
    ConversationRuntime, ModelRouteDecision, NodeRunResult, NodeVerifyOutcome, ToolExecutor,
};

pub(super) const TEAM_ARCHITECT_SYSTEM_PROMPT: &str = "You are the Architect in a multi-role coding team. For the given step, produce a short, concrete approach brief: the intended solution shape, key decisions, and risks to watch. Do not implement; just set direction in a few sentences.";

pub(super) const TEAM_EXECUTOR_SYSTEM_PROMPT: &str = "You are the Executor in a multi-role coding team. Implement the given step following the architect's brief and addressing any reviewer feedback. Produce the concrete result/output for this step in a few sentences.";

pub(super) const TEAM_REVIEWER_SYSTEM_PROMPT: &str = "You are the Reviewer in a multi-role coding team. Judge whether the executor's result satisfies the step and the brief. Reply 'APPROVE' if it does; otherwise reply 'REQUEST_CHANGES: <specific reasons>'. Be concise and concrete.";

impl<C, T> ConversationRuntime<C, T>
where
    C: ApiClient,
    T: ToolExecutor,
{
    /// Run a node through the multi-role convergence loop: Architect drafts an
    /// approach, Executor implements it, Reviewer approves or requests changes
    /// (re-driving the Executor with the reasons), bounded by `max_rounds`
    /// review cycles. On convergence the node succeeds; on exhaustion it falls
    /// back to the Stage 3 single-Executor verified path so a struggling node
    /// still gets the standard recovery treatment before failing.
    pub(super) fn execute_structured_node_with_team(
        &mut self,
        task_id: &str,
        user_input: &str,
        node: &crate::structured_execution::ExecutionNode,
        acceptance: &[String],
        max_rounds: usize,
    ) -> NodeRunResult {
        let rounds_budget = max_rounds.max(1);
        // Architect: draft the approach once (Planning route).
        let brief = self
            .run_team_role_turn(
                task_id,
                crate::team_convergence::ConvergenceRole::Architect,
                user_input,
                node,
                None,
            )
            .map(|(text, _route)| text)
            .unwrap_or_else(|| format!("Implement step '{}' directly.", node.title));
        // Record the architect's brief as a team event so the role dialogue is
        // captured in the TeamExecutionLedger / runtime event stream.
        self.emit_node_role_event(
            task_id,
            crate::TeamRole::Planner,
            crate::TeamExecutionEventKind::NodeStarted,
            format!("[{}] architect brief: {}", node.id, brief),
        );

        let mut prior_reasons: Option<String> = None;
        for _round in 0..rounds_budget {
            // Executor: implement against the brief and any prior review reasons.
            let exec_context = match &prior_reasons {
                Some(reasons) => format!(
                    "Approach brief: {brief}\n\nThe reviewer requested changes: {reasons}. Address them.",
                ),
                None => format!("Approach brief: {brief}"),
            };
            let Some((output, exec_route)) = self.run_team_role_turn(
                task_id,
                crate::team_convergence::ConvergenceRole::Executor,
                user_input,
                node,
                Some(exec_context),
            ) else {
                prior_reasons = Some("executor produced no output".to_string());
                continue;
            };
            self.emit_node_role_event(
                task_id,
                crate::TeamRole::Implementer,
                crate::TeamExecutionEventKind::NodeFinished,
                format!("[{}] executor output: {}", node.id, output),
            );

            // Reviewer: approve or request changes (Verification route).
            let verdict = self.review_team_node(task_id, user_input, node, &brief, &output);
            match verdict {
                crate::team_convergence::ReviewVerdict::Approve => {
                    self.emit_node_role_event(
                        task_id,
                        crate::TeamRole::Reviewer,
                        crate::TeamExecutionEventKind::VerificationPassed,
                        format!("[{}] reviewer approved", node.id),
                    );
                    // Reviewer approved; still honor objective acceptance checks.
                    match self.verify_node_acceptance(task_id, node, acceptance) {
                        NodeVerifyOutcome::Passed | NodeVerifyOutcome::Skipped => {
                            return NodeRunResult::Succeeded { summary: output };
                        }
                        NodeVerifyOutcome::Failed { reason } => {
                            prior_reasons = Some(format!("acceptance checks failed: {reason}"));
                        }
                    }
                }
                crate::team_convergence::ReviewVerdict::RequestChanges { reasons } => {
                    self.emit_node_role_event(
                        task_id,
                        crate::TeamRole::Reviewer,
                        crate::TeamExecutionEventKind::VerificationFailed,
                        format!("[{}] reviewer requested changes: {}", node.id, reasons),
                    );
                    // Stage 4: record the executor route as failed so the next
                    // round's route selection can escalate this node's Executor
                    // to a higher-quality Coding route when alternates exist.
                    let _ = self.task_registry.update_latest_route_feedback(
                        task_id,
                        crate::ModelRouteFeedback::pending(
                            task_id.to_string(),
                            exec_route,
                            current_time_millis() / 1_000,
                        )
                        .with_outcome(
                            false,
                            Some(false),
                            true,
                            Some(reasons.clone()),
                        ),
                    );
                    prior_reasons = Some(reasons);
                }
            }
        }

        // Convergence exhausted — fall back to the Stage 3 verified path so the
        // node gets the standard recovery treatment before being failed.
        self.execute_structured_node_verified(task_id, user_input, node, acceptance, max_rounds)
    }

    /// Emit one role-dialogue event for a structured node into the team ledger
    /// and runtime event stream, so multi-role convergence is observable.
    fn emit_node_role_event(
        &self,
        task_id: &str,
        role: crate::TeamRole,
        kind: crate::TeamExecutionEventKind,
        message: String,
    ) {
        let seq = self
            .task_registry
            .get(task_id)
            .map_or(1, |task| task.team_events.len() as u64 + 1);
        let event = crate::TeamExecutionEvent {
            seq,
            team_id: format!("team-{}", self.session.session_id),
            task_id: task_id.to_string(),
            role,
            kind,
            model_route: None,
            message: Some(message),
        };
        self.emit_team_execution_event_for_task(task_id, event);
    }

    /// Run one role's focused sub-turn for a node, routed by the role's phase.
    /// `extra` carries role-specific context (the brief, prior review reasons).
    /// Returns the role's text output and the route used (so the caller can
    /// record per-role route feedback for adaptive escalation).
    fn run_team_role_turn(
        &mut self,
        task_id: &str,
        role: crate::team_convergence::ConvergenceRole,
        user_input: &str,
        node: &crate::structured_execution::ExecutionNode,
        extra: Option<String>,
    ) -> Option<(String, ModelRouteDecision)> {
        let phase = role.team_role().route_phase();
        let route = self.select_model_route_for_task_with_complexity(
            task_id,
            phase,
            Some(node.estimated_effort.max(1)),
        );
        let system_prompt = match role {
            crate::team_convergence::ConvergenceRole::Architect => TEAM_ARCHITECT_SYSTEM_PROMPT,
            crate::team_convergence::ConvergenceRole::Executor => TEAM_EXECUTOR_SYSTEM_PROMPT,
            crate::team_convergence::ConvergenceRole::Reviewer => TEAM_REVIEWER_SYSTEM_PROMPT,
        };
        let mut prompt = format!(
            "Overall task: {user_input}\n\nStep: {} — {}\nRole: {}",
            node.id,
            node.title,
            role.label()
        );
        if let Some(extra) = extra {
            prompt.push_str("\n\n");
            prompt.push_str(&extra);
        }
        let request = ApiRequest {
            system_prompt: vec![system_prompt.to_string()],
            messages: vec![ConversationMessage::user_text(prompt)],
            model_route: Some(route.clone()),
        };
        let events = self.api_client.stream(request).ok()?;
        let mut text = String::new();
        for event in events {
            if let AssistantEvent::TextDelta(delta) = event {
                text.push_str(&delta);
            }
        }
        let text = text.trim();
        if text.is_empty() {
            None
        } else {
            Some((text.to_string(), route))
        }
    }

    /// Reviewer sub-turn: returns Approve unless the review output signals a
    /// change request (case-insensitive "REQUEST_CHANGES" / "changes needed").
    /// A missing review defaults to Approve so a silent reviewer does not wedge
    /// the loop (acceptance checks still gate success).
    fn review_team_node(
        &mut self,
        task_id: &str,
        user_input: &str,
        node: &crate::structured_execution::ExecutionNode,
        brief: &str,
        output: &str,
    ) -> crate::team_convergence::ReviewVerdict {
        let context = format!(
            "Approach brief: {brief}\n\nExecutor result to review:\n{output}\n\nReply 'APPROVE' if the result satisfies the step, otherwise reply 'REQUEST_CHANGES: <reasons>'.",
        );
        let review = self.run_team_role_turn(
            task_id,
            crate::team_convergence::ConvergenceRole::Reviewer,
            user_input,
            node,
            Some(context),
        );
        match review {
            None => crate::team_convergence::ReviewVerdict::RequestChanges {
                reasons: "reviewer produced no verdict".to_string(),
            },
            Some((text, _route)) => {
                let upper = text.to_ascii_uppercase();
                if upper.contains("REQUEST_CHANGES") || upper.contains("CHANGES NEEDED") {
                    crate::team_convergence::ReviewVerdict::RequestChanges { reasons: text }
                } else {
                    crate::team_convergence::ReviewVerdict::Approve
                }
            }
        }
    }
}
