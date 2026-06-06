use std::collections::BTreeMap;
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
    pub recovery_triggered: bool,
    pub recovery_actions: Vec<TaskRecoveryActionSignal>,
    pub route_feedback_count: usize,
    pub route_failures: usize,
    pub route_recovery_triggered: usize,
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
        let mut summaries = BTreeMap::<String, RecoveryActionMemorySummary>::new();
        for signal in self
            .entries
            .iter()
            .flat_map(|entry| entry.recovery_actions.iter())
        {
            let summary = summaries.entry(signal.kind.clone()).or_insert_with(|| {
                RecoveryActionMemorySummary {
                    kind: signal.kind.clone(),
                    total: 0,
                    executed: 0,
                    blocked: 0,
                    success_rate: 0.0,
                }
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

    #[must_use]
    pub fn similar_entries(&self, task_type: &str) -> Vec<TaskMemoryEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.task_type == task_type)
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
        ModelRouteDecision, ModelRouteFeedback, ModelRoutePhase, RecoveryAction,
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
