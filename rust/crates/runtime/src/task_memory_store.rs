use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::task_registry::Task;
use crate::{infer_verification_policy, RouteFeedbackStore, TaskStatus};

const TASK_MEMORY_SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskMemorySnapshot {
    pub version: u32,
    pub entries: Vec<TaskMemoryEntry>,
    pub summaries: Vec<TaskMemorySummary>,
    pub recovery_actions: Vec<RecoveryActionMemorySummary>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskMemoryEntry {
    pub task_id: String,
    pub task_type: String,
    pub status: TaskStatus,
    pub final_status: Option<String>,
    pub completed: bool,
    pub blocked: bool,
    pub updated_at: u64,
    pub verification_policy: String,
    pub acceptance_tests: Vec<String>,
    pub latest_failure_class: Option<String>,
    pub latest_failure_reason: Option<String>,
    pub latest_report_message: Option<String>,
    #[serde(default)]
    pub plan_total_nodes: usize,
    #[serde(default)]
    pub plan_completed_nodes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub node_handoffs: Vec<TaskNodeMemoryArtifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_summary: Option<String>,
    pub recovery_triggered: bool,
    pub recovery_actions: Vec<TaskRecoveryActionSignal>,
    pub route_feedback_count: usize,
    pub route_failures: usize,
    pub route_recovery_triggered: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskNodeMemoryArtifact {
    pub node_id: String,
    pub status: String,
    pub summary: Option<String>,
    pub evidence: Vec<String>,
    pub blocking_reason: Option<String>,
    pub confidence_percent: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecoveryActionSignal {
    pub kind: String,
    pub scenario: String,
    pub executed: bool,
    pub blocked: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskMemorySummary {
    pub task_type: String,
    pub total: usize,
    pub completed: usize,
    pub blocked: usize,
    pub failed: usize,
    pub recovery_triggered: usize,
    pub common_failure_classes: BTreeMap<String, usize>,
    pub verification_commands: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecoveryActionMemorySummary {
    pub kind: String,
    pub total: usize,
    pub executed: usize,
    pub blocked: usize,
    pub success_rate: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskMemoryContext {
    pub task_type: String,
    pub similar_count: usize,
    pub successful_acceptance_tests: Vec<String>,
    pub common_failure_classes: BTreeMap<String, usize>,
    pub recovery_actions: Vec<RecoveryActionMemorySummary>,
    pub route_failure_rate: Option<f32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub current_node_handoffs: Vec<TaskNodeMemoryArtifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_handoff_summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_node_hint: Option<String>,
    pub recommendations: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct TaskMemoryStore {
    entries: Vec<TaskMemoryEntry>,
}

impl TaskMemoryStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn from_tasks(tasks: &[Task]) -> Self {
        let entries = tasks.iter().map(TaskMemoryEntry::from_task).collect();
        Self { entries }
    }

    pub fn load_from_dir(dir: &Path) -> io::Result<Self> {
        let path = dir.join("tasks.json");
        if !path.exists() {
            return Ok(Self::new());
        }
        let contents = fs::read_to_string(path)?;
        let snapshot = serde_json::from_str::<TaskMemorySnapshot>(&contents)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(Self {
            entries: snapshot.entries,
        })
    }

    pub fn save_to_dir(&self, dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)?;
        let snapshot = self.snapshot();
        let json = serde_json::to_string_pretty(&snapshot)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let path = dir.join("tasks.json");
        let tmp = dir.join("tasks.json.tmp");
        fs::write(&tmp, format!("{json}\n"))?;
        fs::rename(&tmp, path)
    }

    #[must_use]
    pub fn entries(&self) -> &[TaskMemoryEntry] {
        &self.entries
    }

    #[must_use]
    pub fn entry_for_task(&self, task_id: &str) -> Option<&TaskMemoryEntry> {
        self.entries.iter().find(|entry| entry.task_id == task_id)
    }

    #[must_use]
    pub fn summaries(&self) -> Vec<TaskMemorySummary> {
        let mut summaries = BTreeMap::<String, TaskMemorySummary>::new();
        for entry in &self.entries {
            let summary =
                summaries
                    .entry(entry.task_type.clone())
                    .or_insert_with(|| TaskMemorySummary {
                        task_type: entry.task_type.clone(),
                        total: 0,
                        completed: 0,
                        blocked: 0,
                        failed: 0,
                        recovery_triggered: 0,
                        common_failure_classes: BTreeMap::new(),
                        verification_commands: BTreeMap::new(),
                    });
            summary.total += 1;
            summary.completed += usize::from(entry.completed);
            summary.blocked += usize::from(entry.blocked);
            summary.failed += usize::from(entry.status == TaskStatus::Failed);
            summary.recovery_triggered += usize::from(entry.recovery_triggered);
            if let Some(failure_class) = entry.latest_failure_class.as_ref() {
                *summary
                    .common_failure_classes
                    .entry(failure_class.clone())
                    .or_insert(0) += 1;
            }
            for command in &entry.acceptance_tests {
                *summary
                    .verification_commands
                    .entry(command.clone())
                    .or_insert(0) += 1;
            }
        }
        summaries.into_values().collect()
    }

    #[must_use]
    pub fn recovery_action_summaries(&self) -> Vec<RecoveryActionMemorySummary> {
        recovery_action_summaries_for_entries(self.entries.iter())
    }

    #[must_use]
    pub fn recovery_action_summaries_for_type(
        &self,
        task_type: &str,
    ) -> Vec<RecoveryActionMemorySummary> {
        recovery_action_summaries_for_entries(
            self.entries
                .iter()
                .filter(|entry| entry.task_type == task_type),
        )
    }

    #[must_use]
    pub fn similar_entries(&self, task_type: &str) -> Vec<TaskMemoryEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.task_type == task_type)
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn context_for_task(&self, task: &Task) -> TaskMemoryContext {
        let current = TaskMemoryEntry::from_task(task);
        self.context_for_entry(&current)
    }

    #[must_use]
    pub fn context_for_entry(&self, current: &TaskMemoryEntry) -> TaskMemoryContext {
        let similar = self
            .entries
            .iter()
            .filter(|entry| {
                entry.task_type == current.task_type && entry.task_id != current.task_id
            })
            .collect::<Vec<_>>();
        let mut tests = BTreeSet::new();
        let mut failure_classes = BTreeMap::<String, usize>::new();
        let mut route_feedback_count = 0_usize;
        let mut route_failures = 0_usize;
        for entry in &similar {
            if entry.completed {
                tests.extend(entry.acceptance_tests.iter().cloned());
            }
            if let Some(class) = entry.latest_failure_class.as_ref() {
                *failure_classes.entry(class.clone()).or_insert(0) += 1;
            }
            route_feedback_count += entry.route_feedback_count;
            route_failures += entry.route_failures;
        }
        let mut recovery_actions = recovery_action_summaries_for_entries(similar.iter().copied());
        recovery_actions.sort_by(|left, right| {
            right
                .success_rate
                .total_cmp(&left.success_rate)
                .then_with(|| right.executed.cmp(&left.executed))
                .then_with(|| left.blocked.cmp(&right.blocked))
                .then_with(|| left.kind.cmp(&right.kind))
        });
        let route_failure_rate = (route_feedback_count > 0)
            .then_some(route_failures as f32 / route_feedback_count as f32);
        let successful_acceptance_tests = tests.into_iter().collect::<Vec<_>>();
        let recommendations = memory_context_recommendations(
            &successful_acceptance_tests,
            &failure_classes,
            &recovery_actions,
            route_failure_rate,
            current.next_node_id.as_deref(),
            current.handoff_summary.as_deref(),
        );
        TaskMemoryContext {
            task_type: current.task_type.clone(),
            similar_count: similar.len(),
            successful_acceptance_tests,
            common_failure_classes: failure_classes,
            recovery_actions,
            route_failure_rate,
            current_node_handoffs: current.node_handoffs.clone(),
            current_handoff_summary: current.handoff_summary.clone(),
            next_node_hint: current.next_node_id.clone(),
            recommendations,
        }
    }

    #[must_use]
    pub fn task_type_for(task: &Task) -> String {
        task_type(task)
    }

    #[must_use]
    pub fn entry_task_type(task: &Task) -> String {
        task_type(task)
    }

    #[must_use]
    pub fn memory_feedback_for_task<'a>(
        &self,
        task: &Task,
        feedback: impl Iterator<Item = &'a crate::ModelRouteFeedback>,
    ) -> Vec<crate::ModelRouteFeedback> {
        let task_type = task_type(task);
        feedback
            .filter(|entry| {
                entry.task_type.as_deref() == Some(task_type.as_str())
                    || (entry.task_type.is_none()
                        && self
                            .entry_for_task(&entry.task_id)
                            .is_some_and(|memory| memory.task_type == task_type))
            })
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn snapshot(&self) -> TaskMemorySnapshot {
        TaskMemorySnapshot {
            version: TASK_MEMORY_SNAPSHOT_VERSION,
            entries: self.entries.clone(),
            summaries: self.summaries(),
            recovery_actions: self.recovery_action_summaries(),
        }
    }
}

fn recovery_action_summaries_for_entries<'a>(
    entries: impl Iterator<Item = &'a TaskMemoryEntry>,
) -> Vec<RecoveryActionMemorySummary> {
    let mut summaries = BTreeMap::<String, RecoveryActionMemorySummary>::new();
    for signal in entries.flat_map(|entry| entry.recovery_actions.iter()) {
        let summary =
            summaries
                .entry(signal.kind.clone())
                .or_insert_with(|| RecoveryActionMemorySummary {
                    kind: signal.kind.clone(),
                    total: 0,
                    executed: 0,
                    blocked: 0,
                    success_rate: 0.0,
                });
        summary.total += 1;
        summary.executed += usize::from(signal.executed && !signal.blocked);
        summary.blocked += usize::from(signal.blocked);
    }
    summaries
        .into_values()
        .map(|mut summary| {
            summary.success_rate = if summary.total == 0 {
                0.0
            } else {
                summary.executed as f32 / summary.total as f32
            };
            summary
        })
        .collect()
}

fn memory_context_recommendations(
    tests: &[String],
    failure_classes: &BTreeMap<String, usize>,
    recovery_actions: &[RecoveryActionMemorySummary],
    route_failure_rate: Option<f32>,
    next_node_id: Option<&str>,
    handoff_summary: Option<&str>,
) -> Vec<String> {
    let mut recommendations = Vec::new();
    if let Some(next_node_id) = next_node_id {
        recommendations.push(format!(
            "Resume from node `{next_node_id}` using the current task handoff before replanning."
        ));
    }
    if handoff_summary.is_some() {
        recommendations.push("Preserve completed node artifacts; do not redo finished work unless verification fails.".to_string());
    }
    if !tests.is_empty() {
        recommendations.push(format!(
            "Seed planning with {} successful acceptance test(s) from similar tasks.",
            tests.len()
        ));
    }
    if let Some((class, count)) = failure_classes.iter().max_by_key(|(_, count)| *count) {
        recommendations.push(format!(
            "Pre-check recurring failure class `{class}` observed in {count} similar task(s)."
        ));
    }
    if let Some(action) = recovery_actions
        .iter()
        .find(|action| action.total > 0 && action.success_rate >= 0.5)
    {
        recommendations.push(format!(
            "Prefer recovery action `{}` first; historical success rate is {:.0}%.",
            action.kind,
            action.success_rate * 100.0
        ));
    }
    if route_failure_rate.is_some_and(|rate| rate >= 0.5) {
        recommendations.push(
            "Route similar tasks conservatively; historical route failure rate is high."
                .to_string(),
        );
    }
    if recommendations.is_empty() {
        recommendations
            .push("No memory-informed planning adjustment is available yet.".to_string());
    }
    recommendations
}

impl TaskMemoryEntry {
    #[must_use]
    pub fn from_task(task: &Task) -> Self {
        let latest_report = task.execution_reports.last();
        let route_failure_feedback = task
            .route_feedback
            .iter()
            .rev()
            .find(|feedback| feedback.failure_class.is_some());
        let latest_failure_class = latest_report
            .and_then(|report| report.failure.as_ref())
            .map(|failure| failure.failure_class.clone())
            .or_else(|| route_failure_feedback.and_then(|feedback| feedback.failure_class.clone()));
        let latest_failure_reason = latest_report
            .and_then(|report| report.failure.as_ref())
            .map(|failure| failure.reason.clone())
            .or_else(|| route_failure_feedback.and_then(|feedback| feedback.note.clone()));
        let route_store = RouteFeedbackStore::from_feedback(task.route_feedback.clone());
        let route_summaries = route_store.summaries();
        let route_failures = route_summaries
            .iter()
            .map(|summary| summary.failures)
            .sum::<usize>();
        let route_recovery_triggered = route_summaries
            .iter()
            .map(|summary| summary.recovery_triggered)
            .sum::<usize>();
        let recovery_actions = task
            .recovery_action_executions
            .iter()
            .flat_map(|execution| execution.results.iter())
            .map(|result| TaskRecoveryActionSignal {
                kind: format!("{:?}", result.action.kind),
                scenario: result.action.scenario.to_string(),
                executed: result.executed,
                blocked: result.blocked,
                reason: result.reason.clone(),
            })
            .collect::<Vec<_>>();
        let completed = latest_report.map_or(task.status == TaskStatus::Completed, |report| {
            report.completed
        });
        let blocked = latest_report.map_or(
            matches!(
                task.status,
                TaskStatus::Blocked | TaskStatus::Failed | TaskStatus::WaitingForPermission
            ),
            |report| report.blocked,
        );
        let plan_total_nodes = task
            .plan
            .as_ref()
            .map_or(0, |plan| plan.execution.nodes.len());
        let plan_completed_nodes = task
            .plan
            .as_ref()
            .map_or(0, |plan| plan.execution.completed_nodes().len());
        let next_node_id = task
            .plan
            .as_ref()
            .and_then(|plan| plan.resume_cursor.as_ref())
            .and_then(|cursor| cursor.node_id.clone());
        let node_handoffs = task
            .plan
            .as_ref()
            .map(node_memory_artifacts)
            .unwrap_or_default();
        let handoff_summary = task_handoff_summary(
            plan_completed_nodes,
            plan_total_nodes,
            next_node_id.as_deref(),
            &node_handoffs,
        );
        Self {
            task_id: task.task_id.clone(),
            task_type: task_type(task),
            status: task.status,
            final_status: latest_report.map(|report| report.final_status.to_string()),
            completed,
            blocked,
            updated_at: task.updated_at,
            verification_policy: format!(
                "{:?}",
                infer_verification_policy(task.task_packet.as_ref())
            ),
            acceptance_tests: task
                .task_packet
                .as_ref()
                .map(|packet| packet.acceptance_tests.clone())
                .unwrap_or_default(),
            latest_failure_class,
            latest_failure_reason,
            latest_report_message: latest_report.map(|report| report.message.clone()),
            plan_total_nodes,
            plan_completed_nodes,
            next_node_id,
            node_handoffs,
            handoff_summary,
            recovery_triggered: !task.recovery_events.is_empty()
                || !task.recovery_action_executions.is_empty()
                || task
                    .route_feedback
                    .iter()
                    .any(|feedback| feedback.recovery_triggered),
            recovery_actions,
            route_feedback_count: task.route_feedback.len(),
            route_failures,
            route_recovery_triggered,
        }
    }
}

fn node_memory_artifacts(plan: &crate::TaskPlanSnapshot) -> Vec<TaskNodeMemoryArtifact> {
    plan.execution
        .nodes
        .values()
        .filter_map(|node| {
            let artifact = node.artifact.as_ref();
            let summary = artifact
                .map(|artifact| artifact.summary.clone())
                .or_else(|| node.output_summary.clone());
            if summary.is_none()
                && node.failure_class.is_none()
                && artifact.is_none()
                && node.status != crate::PlanNodeStatus::Running
            {
                return None;
            }
            Some(TaskNodeMemoryArtifact {
                node_id: node.node_id.clone(),
                status: format!("{:?}", node.status).to_ascii_lowercase(),
                summary,
                evidence: artifact.map_or_else(Vec::new, |artifact| artifact.evidence.clone()),
                blocking_reason: artifact
                    .and_then(|artifact| artifact.blocking_reason.clone())
                    .or_else(|| node.failure_class.clone()),
                confidence_percent: artifact.map(|artifact| artifact.confidence_percent),
            })
        })
        .collect()
}

fn task_handoff_summary(
    completed: usize,
    total: usize,
    next_node_id: Option<&str>,
    node_handoffs: &[TaskNodeMemoryArtifact],
) -> Option<String> {
    if total == 0 && node_handoffs.is_empty() {
        return None;
    }
    let mut parts = vec![format!("completed {completed}/{total} plan node(s)")];
    if let Some(next) = next_node_id {
        parts.push(format!("next node: {next}"));
    }
    let recent = node_handoffs
        .iter()
        .rev()
        .take(3)
        .filter_map(|artifact| {
            artifact
                .summary
                .as_ref()
                .map(|summary| format!("{}={summary}", artifact.node_id))
        })
        .collect::<Vec<_>>();
    if !recent.is_empty() {
        parts.push(format!("recent artifacts: {}", recent.join("; ")));
    }
    Some(parts.join("; "))
}

fn task_type(task: &Task) -> String {
    if let Some(packet) = task.task_packet.as_ref() {
        let scope = normalize_type_fragment(&packet.scope);
        if !scope.is_empty() {
            return format!("packet:{scope}");
        }
        return "packet".to_string();
    }
    task.description
        .as_deref()
        .map(normalize_type_fragment)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "conversation".to_string())
}

fn normalize_type_fragment(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .take(4)
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ModelRouteDecision, ModelRouteFeedback, ModelRoutePhase, NodeExecutionArtifact, PlanDag,
        PlanDagEdge, PlanDagEdgeKind, PlanDagNode, PlanExecution, PlanNodeKind, RecoveryAction,
        RecoveryActionExecution, RecoveryActionKind, RecoveryActionResult, RecoveryActionRisk,
        TaskPacket,
    };

    fn packet_task() -> Task {
        let mut task = crate::TaskRegistry::new()
            .create_from_packet(TaskPacket {
                objective: "Fix task".to_string(),
                scope: "runtime scheduler".to_string(),
                repo: ".".to_string(),
                branch_policy: "current".to_string(),
                acceptance_tests: vec!["python3 --version".to_string()],
                commit_policy: "none".to_string(),
                reporting_contract: "report".to_string(),
                escalation_policy: "block".to_string(),
            })
            .expect("packet task");
        task.status = TaskStatus::Completed;
        task.route_feedback.push(
            ModelRouteFeedback::pending(
                task.task_id.clone(),
                ModelRouteDecision {
                    phase: ModelRoutePhase::Verification,
                    model: "sonnet".to_string(),
                    provider: None,
                    reason: "test".to_string(),
                    confidence: Some(0.8),
                    fallback_model: None,
                },
                1,
            )
            .with_outcome(true, Some(true), false, None),
        );
        task.recovery_action_executions
            .push(RecoveryActionExecution {
                task_id: task.task_id.clone(),
                results: vec![RecoveryActionResult {
                    action: RecoveryAction {
                        kind: RecoveryActionKind::SwitchModel,
                        scenario: crate::FailureScenario::ProviderFailure,
                        risk: RecoveryActionRisk::Safe,
                        node_id: None,
                        message: "switch".to_string(),
                    },
                    executed: true,
                    blocked: false,
                    reason: "scheduled".to_string(),
                }],
            });
        task
    }

    #[test]
    fn builds_memory_from_tasks() {
        let task = packet_task();
        let store = TaskMemoryStore::from_tasks(&[task.clone()]);
        let entry = store.entry_for_task(&task.task_id).expect("entry");

        assert_eq!(entry.task_type, "packet:runtime-scheduler");
        assert_eq!(entry.acceptance_tests, vec!["python3 --version"]);
        assert_eq!(entry.route_feedback_count, 1);
        assert_eq!(entry.recovery_actions[0].kind, "SwitchModel");
        assert_eq!(store.summaries()[0].completed, 1);
        assert_eq!(store.recovery_action_summaries()[0].executed, 1);
    }

    #[test]
    fn context_for_task_uses_similar_task_memory() {
        let mut previous = packet_task();
        previous.task_id = "previous-task".to_string();
        let mut current = packet_task();
        current.task_id = "current-task".to_string();
        current.recovery_action_executions.clear();
        let store = TaskMemoryStore::from_tasks(&[previous, current.clone()]);

        let context = store.context_for_task(&current);

        assert_eq!(context.task_type, "packet:runtime-scheduler");
        assert_eq!(context.similar_count, 1);
        assert_eq!(
            context.successful_acceptance_tests,
            vec!["python3 --version"]
        );
        assert_eq!(context.recovery_actions[0].kind, "SwitchModel");
        assert!(context
            .recommendations
            .iter()
            .any(|item| item.contains("acceptance test")));
    }

    #[test]
    fn context_for_task_includes_current_handoff_artifacts() {
        let registry = crate::TaskRegistry::new();
        let task = registry.create("resume complex work", Some("runtime scheduler"));
        let dag = PlanDag {
            task_id: task.task_id.clone(),
            root_id: task.task_id.clone(),
            nodes: vec![
                PlanDagNode {
                    kind: PlanNodeKind::Step,
                    id: "analyze".to_string(),
                    title: "Analyze".to_string(),
                    parallelizable: false,
                    estimated_effort: 1,
                    candidate_tools: Vec::new(),
                    notes: Vec::new(),
                },
                PlanDagNode {
                    kind: PlanNodeKind::Step,
                    id: "implement".to_string(),
                    title: "Implement".to_string(),
                    parallelizable: false,
                    estimated_effort: 2,
                    candidate_tools: Vec::new(),
                    notes: Vec::new(),
                },
            ],
            edges: vec![PlanDagEdge {
                from: "analyze".to_string(),
                to: "implement".to_string(),
                kind: PlanDagEdgeKind::DependsOn,
            }],
        };
        let mut execution = PlanExecution::new(&dag);
        execution.start_node("analyze").expect("start");
        execution
            .succeed_node_with_artifact(
                &dag,
                "analyze",
                Some("analysis complete".to_string()),
                Some(
                    NodeExecutionArtifact::new("analyze", "analysis complete")
                        .with_evidence(vec!["read src/lib.rs".to_string()])
                        .with_confidence_percent(90)
                        .with_producer("test"),
                ),
            )
            .expect("succeed");
        registry
            .record_plan(&task.task_id, dag, execution)
            .expect("plan");
        let task = registry.get(&task.task_id).expect("task");
        let store = TaskMemoryStore::from_tasks(std::slice::from_ref(&task));
        let context = store.context_for_task(&task);

        assert_eq!(context.next_node_hint.as_deref(), Some("implement"));
        assert!(context
            .current_handoff_summary
            .as_deref()
            .is_some_and(|summary| summary.contains("completed 1/2")));
        assert_eq!(context.current_node_handoffs.len(), 1);
        assert_eq!(
            context.current_node_handoffs[0].summary.as_deref(),
            Some("analysis complete")
        );
        assert!(context
            .recommendations
            .iter()
            .any(|item| item.contains("Resume from node `implement`")));
    }

    #[test]
    fn saves_and_loads_snapshot() {
        let task = packet_task();
        let store = TaskMemoryStore::from_tasks(&[task]);
        let dir = std::env::temp_dir().join(format!("himalaya-task-memory-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        store.save_to_dir(&dir).expect("save");
        let loaded = TaskMemoryStore::load_from_dir(&dir).expect("load");

        assert_eq!(loaded.entries().len(), 1);
        assert_eq!(loaded.summaries().len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
