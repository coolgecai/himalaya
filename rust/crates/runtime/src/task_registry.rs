#![allow(clippy::must_use_candidate, clippy::unnecessary_map_or)]
//! In-memory task registry for sub-agent task lifecycle management.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{
    evaluate_verification_result, infer_verification_policy, validate_packet, ModelRouteFeedback,
    PlanDag, PlanExecution, RecoveryActionExecution, RecoveryEvent, TaskPacket,
    TaskPacketValidationError, TeamExecutionEvent, VerificationDecision, VerificationResult,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Created,
    Planning,
    Running,
    WaitingForPermission,
    WaitingForVerification,
    Recovering,
    Blocked,
    Completed,
    Failed,
    Stopped,
    Cancelled,
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Created => write!(f, "created"),
            Self::Planning => write!(f, "planning"),
            Self::Running => write!(f, "running"),
            Self::WaitingForPermission => write!(f, "waiting_for_permission"),
            Self::WaitingForVerification => write!(f, "waiting_for_verification"),
            Self::Recovering => write!(f, "recovering"),
            Self::Blocked => write!(f, "blocked"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
            Self::Stopped => write!(f, "stopped"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCheckpoint {
    pub seq: u64,
    pub label: String,
    pub status: TaskStatus,
    pub timestamp: u64,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResumeCursor {
    pub node_id: Option<String>,
    pub completed_nodes: Vec<String>,
    pub resumable_nodes: Vec<String>,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPlanSnapshot {
    pub dag: PlanDag,
    pub execution: PlanExecution,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_cursor: Option<TaskResumeCursor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub task_id: String,
    pub prompt: String,
    pub description: Option<String>,
    pub task_packet: Option<TaskPacket>,
    pub status: TaskStatus,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default)]
    pub messages: Vec<TaskMessage>,
    #[serde(default)]
    pub output: String,
    pub team_id: Option<String>,
    pub verification_result: Option<VerificationResult>,
    #[serde(default)]
    pub checkpoints: Vec<TaskCheckpoint>,
    #[serde(default)]
    pub plan: Option<TaskPlanSnapshot>,
    #[serde(default)]
    pub recovery_events: Vec<RecoveryEvent>,
    #[serde(default)]
    pub team_events: Vec<TeamExecutionEvent>,
    #[serde(default)]
    pub recovery_action_executions: Vec<RecoveryActionExecution>,
    #[serde(default)]
    pub route_feedback: Vec<ModelRouteFeedback>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMessage {
    pub role: String,
    pub content: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressLedgerEntry {
    pub seq: u64,
    pub task_id: String,
    pub event: String,
    pub status: TaskStatus,
    pub message: Option<String>,
    pub timestamp: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskEventLogEntry {
    pub seq: u64,
    pub task_id: String,
    pub event: String,
    pub status: TaskStatus,
    pub message: Option<String>,
    pub timestamp: u64,
}

const SNAPSHOT_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TaskRegistrySnapshot {
    #[serde(default = "current_snapshot_version")]
    version: u32,
    tasks: Vec<Task>,
    #[serde(default)]
    ledger: Vec<ProgressLedgerEntry>,
    #[serde(default)]
    event_log: Vec<TaskEventLogEntry>,
    #[serde(default)]
    counter: u64,
    #[serde(default)]
    ledger_counter: u64,
}

fn current_snapshot_version() -> u32 {
    SNAPSHOT_VERSION
}

#[derive(Debug, Clone, Default)]
pub struct TaskRegistry {
    inner: Arc<Mutex<RegistryInner>>,
}

#[derive(Debug, Default)]
struct RegistryInner {
    tasks: HashMap<String, Task>,
    counter: u64,
    ledger: Vec<ProgressLedgerEntry>,
    event_log: Vec<TaskEventLogEntry>,
    ledger_counter: u64,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn enrich_feedback_from_task_signals(task: &Task, feedback: &mut ModelRouteFeedback) {
    if let Some(result) = task.verification_result.as_ref() {
        feedback.succeeded = Some(result.passed);
        feedback.verification_passed = Some(result.passed);
        feedback.note = Some(result.summary.clone());
    }
    if !task.recovery_events.is_empty() || !task.recovery_action_executions.is_empty() {
        feedback.recovery_triggered = true;
    }
}

#[allow(clippy::too_many_arguments)]
fn enrich_latest_route_feedback(
    task: &mut Task,
    succeeded: Option<bool>,
    verification_passed: Option<bool>,
    latency_ms: Option<u32>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cost_usd: Option<f64>,
    recovery_triggered: Option<bool>,
    note: Option<String>,
) {
    let Some(feedback) = task.route_feedback.last_mut() else {
        return;
    };
    if let Some(succeeded) = succeeded {
        feedback.succeeded = Some(succeeded);
    }
    if let Some(verification_passed) = verification_passed {
        feedback.verification_passed = Some(verification_passed);
    }
    if latency_ms.is_some() {
        feedback.latency_ms = latency_ms;
    }
    if input_tokens.is_some() {
        feedback.input_tokens = input_tokens;
    }
    if output_tokens.is_some() {
        feedback.output_tokens = output_tokens;
    }
    if cost_usd.is_some() {
        feedback.cost_usd = cost_usd;
    }
    if let Some(recovery_triggered) = recovery_triggered {
        feedback.recovery_triggered |= recovery_triggered;
    }
    if note.is_some() {
        feedback.note = note;
    }
}

fn push_ledger_entry(
    inner: &mut RegistryInner,
    task_id: &str,
    event: impl Into<String>,
    status: TaskStatus,
    message: Option<String>,
    timestamp: u64,
) {
    inner.ledger_counter += 1;
    let entry = ProgressLedgerEntry {
        seq: inner.ledger_counter,
        task_id: task_id.to_owned(),
        event: event.into(),
        status,
        message,
        timestamp,
    };
    inner.event_log.push(TaskEventLogEntry::from(&entry));
    inner.ledger.push(entry);
}

impl From<&ProgressLedgerEntry> for TaskEventLogEntry {
    fn from(entry: &ProgressLedgerEntry) -> Self {
        Self {
            seq: entry.seq,
            task_id: entry.task_id.clone(),
            event: entry.event.clone(),
            status: entry.status,
            message: entry.message.clone(),
            timestamp: entry.timestamp,
        }
    }
}

impl From<&TaskEventLogEntry> for ProgressLedgerEntry {
    fn from(entry: &TaskEventLogEntry) -> Self {
        Self {
            seq: entry.seq,
            task_id: entry.task_id.clone(),
            event: entry.event.clone(),
            status: entry.status,
            message: entry.message.clone(),
            timestamp: entry.timestamp,
        }
    }
}

fn event_log_path(dir: &Path) -> std::path::PathBuf {
    dir.join("events.jsonl")
}

fn append_event_log_to_dir(dir: &Path, entries: &[TaskEventLogEntry]) -> io::Result<()> {
    let path = event_log_path(dir);
    let last_persisted_seq = read_event_log_from_dir(dir)?
        .last()
        .map_or(0, |entry| entry.seq);
    let new_entries = entries
        .iter()
        .filter(|entry| entry.seq > last_persisted_seq)
        .collect::<Vec<_>>();
    if new_entries.is_empty() {
        return Ok(());
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    for entry in new_entries {
        let line = serde_json::to_string(entry)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        writeln!(file, "{line}")?;
    }
    Ok(())
}

fn read_event_log_from_dir(dir: &Path) -> io::Result<Vec<TaskEventLogEntry>> {
    let path = event_log_path(dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(path)?;
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<TaskEventLogEntry>(line)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        })
        .collect()
}

impl TaskRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create(&self, prompt: &str, description: Option<&str>) -> Task {
        self.create_task(prompt.to_owned(), description.map(str::to_owned), None)
    }

    pub fn create_from_packet(
        &self,
        packet: TaskPacket,
    ) -> Result<Task, TaskPacketValidationError> {
        let packet = validate_packet(packet)?.into_inner();
        Ok(self.create_task(
            packet.objective.clone(),
            Some(packet.scope.clone()),
            Some(packet),
        ))
    }

    fn create_task(
        &self,
        prompt: String,
        description: Option<String>,
        task_packet: Option<TaskPacket>,
    ) -> Task {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        inner.counter += 1;
        let ts = now_secs();
        let task_id = format!("task_{:08x}_{}", ts, inner.counter);
        let mut task = Task {
            task_id: task_id.clone(),
            prompt,
            description,
            task_packet,
            status: TaskStatus::Created,
            created_at: ts,
            updated_at: ts,
            attempt: 0,
            messages: Vec::new(),
            output: String::new(),
            team_id: None,
            verification_result: None,
            checkpoints: Vec::new(),
            plan: None,
            recovery_events: Vec::new(),
            team_events: Vec::new(),
            recovery_action_executions: Vec::new(),
            route_feedback: Vec::new(),
        };
        task.checkpoints.push(TaskCheckpoint {
            seq: 1,
            label: "created".to_string(),
            status: TaskStatus::Created,
            timestamp: ts,
            message: task.description.clone(),
        });
        inner.tasks.insert(task_id.clone(), task.clone());
        push_ledger_entry(
            &mut inner,
            &task_id,
            "created",
            TaskStatus::Created,
            task.description.clone(),
            ts,
        );
        task
    }

    pub fn get(&self, task_id: &str) -> Option<Task> {
        let inner = self.inner.lock().expect("registry lock poisoned");
        inner.tasks.get(task_id).cloned()
    }

    pub fn list(&self, status_filter: Option<TaskStatus>) -> Vec<Task> {
        let inner = self.inner.lock().expect("registry lock poisoned");
        inner
            .tasks
            .values()
            .filter(|t| status_filter.map_or(true, |s| t.status == s))
            .cloned()
            .collect()
    }

    pub fn stop(&self, task_id: &str) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;

        match task.status {
            TaskStatus::Completed
            | TaskStatus::Failed
            | TaskStatus::Stopped
            | TaskStatus::Cancelled => {
                return Err(format!(
                    "task {task_id} is already in terminal state: {}",
                    task.status
                ));
            }
            _ => {}
        }

        let ts = now_secs();
        task.status = TaskStatus::Stopped;
        task.updated_at = ts;
        let stopped = task.clone();
        push_ledger_entry(
            &mut inner,
            task_id,
            "stopped",
            TaskStatus::Stopped,
            None,
            ts,
        );
        Ok(stopped)
    }

    pub fn update(&self, task_id: &str, message: &str) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;

        let ts = now_secs();
        task.messages.push(TaskMessage {
            role: String::from("user"),
            content: message.to_owned(),
            timestamp: ts,
        });
        task.updated_at = ts;
        let updated = task.clone();
        push_ledger_entry(
            &mut inner,
            task_id,
            "updated",
            updated.status,
            Some(message.to_owned()),
            ts,
        );
        Ok(updated)
    }

    pub fn output(&self, task_id: &str) -> Result<String, String> {
        let inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        Ok(task.output.clone())
    }

    pub fn append_output(&self, task_id: &str, output: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        task.output.push_str(output);
        let ts = now_secs();
        task.updated_at = ts;
        let status = task.status;
        push_ledger_entry(
            &mut inner,
            task_id,
            "output_appended",
            status,
            Some(output.to_owned()),
            ts,
        );
        Ok(())
    }

    pub fn set_status(&self, task_id: &str, status: TaskStatus) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        if status == TaskStatus::Completed {
            let policy = infer_verification_policy(task.task_packet.as_ref());
            if let VerificationDecision::Failed { reason } =
                evaluate_verification_result(policy, task.verification_result.as_ref())
            {
                return Err(format!("task {task_id} cannot complete: {reason}"));
            }
        }
        let ts = now_secs();
        task.status = status;
        task.updated_at = ts;
        push_ledger_entry(&mut inner, task_id, "status_changed", status, None, ts);
        Ok(())
    }

    pub fn assign_team(&self, task_id: &str, team_id: &str) -> Result<(), String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        let ts = now_secs();
        task.team_id = Some(team_id.to_owned());
        task.updated_at = ts;
        let status = task.status;
        push_ledger_entry(
            &mut inner,
            task_id,
            "team_assigned",
            status,
            Some(team_id.to_owned()),
            ts,
        );
        Ok(())
    }

    pub fn record_checkpoint(
        &self,
        task_id: &str,
        label: impl Into<String>,
        message: Option<String>,
    ) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        let ts = now_secs();
        let checkpoint = TaskCheckpoint {
            seq: task.checkpoints.len() as u64 + 1,
            label: label.into(),
            status: task.status,
            timestamp: ts,
            message: message.clone(),
        };
        task.checkpoints.push(checkpoint);
        task.updated_at = ts;
        let updated = task.clone();
        push_ledger_entry(
            &mut inner,
            task_id,
            "checkpoint_recorded",
            updated.status,
            message,
            ts,
        );
        Ok(updated)
    }

    pub fn record_plan(
        &self,
        task_id: &str,
        dag: PlanDag,
        execution: PlanExecution,
    ) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        let ts = now_secs();
        let cursor = TaskResumeCursor {
            node_id: execution.resumable_nodes().first().cloned(),
            completed_nodes: execution.completed_nodes(),
            resumable_nodes: execution.resumable_nodes(),
            updated_at: ts,
        };
        task.plan = Some(TaskPlanSnapshot {
            dag,
            execution,
            updated_at: ts,
            resume_cursor: Some(cursor),
        });
        task.updated_at = ts;
        let updated = task.clone();
        push_ledger_entry(
            &mut inner,
            task_id,
            "plan_recorded",
            updated.status,
            None,
            ts,
        );
        Ok(updated)
    }

    pub fn record_recovery_event(
        &self,
        task_id: &str,
        event: RecoveryEvent,
    ) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        let ts = now_secs();
        task.recovery_events.push(event);
        enrich_latest_route_feedback(task, None, None, None, None, None, None, Some(true), None);
        task.updated_at = ts;
        let updated = task.clone();
        push_ledger_entry(
            &mut inner,
            task_id,
            "recovery_recorded",
            updated.status,
            None,
            ts,
        );
        Ok(updated)
    }

    pub fn record_recovery_action_execution(
        &self,
        task_id: &str,
        execution: RecoveryActionExecution,
    ) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        let ts = now_secs();
        task.recovery_action_executions.push(execution);
        task.updated_at = ts;
        let updated = task.clone();
        push_ledger_entry(
            &mut inner,
            task_id,
            "recovery_action_recorded",
            updated.status,
            None,
            ts,
        );
        Ok(updated)
    }

    pub fn record_team_event(
        &self,
        task_id: &str,
        event: TeamExecutionEvent,
    ) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        let ts = now_secs();
        task.team_events.push(event);
        task.updated_at = ts;
        let updated = task.clone();
        push_ledger_entry(
            &mut inner,
            task_id,
            "team_event_recorded",
            updated.status,
            None,
            ts,
        );
        Ok(updated)
    }

    pub fn record_route_feedback(
        &self,
        task_id: &str,
        mut feedback: ModelRouteFeedback,
    ) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        let ts = now_secs();
        enrich_feedback_from_task_signals(task, &mut feedback);
        task.route_feedback.push(feedback);
        task.updated_at = ts;
        let updated = task.clone();
        push_ledger_entry(
            &mut inner,
            task_id,
            "route_feedback_recorded",
            updated.status,
            None,
            ts,
        );
        Ok(updated)
    }

    pub fn update_latest_route_feedback(
        &self,
        task_id: &str,
        mut feedback: ModelRouteFeedback,
    ) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        let ts = now_secs();
        enrich_feedback_from_task_signals(task, &mut feedback);
        if let Some(existing) = task.route_feedback.iter_mut().rev().find(|existing| {
            existing.route.phase == feedback.route.phase
                && existing.route.model == feedback.route.model
                && existing.route.provider == feedback.route.provider
        }) {
            existing.merge_observations(&feedback);
        } else {
            task.route_feedback.push(feedback);
        }
        task.updated_at = ts;
        let updated = task.clone();
        push_ledger_entry(
            &mut inner,
            task_id,
            "route_feedback_recorded",
            updated.status,
            None,
            ts,
        );
        Ok(updated)
    }

    pub fn retry_plan_node(&self, task_id: &str, node_id: &str) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        let Some(plan) = task.plan.as_mut() else {
            return Err(format!("task {task_id} has no persisted plan"));
        };
        plan.execution.retry_node(&plan.dag, node_id)?;
        let ts = now_secs();
        plan.updated_at = ts;
        plan.resume_cursor = Some(TaskResumeCursor {
            node_id: Some(node_id.to_string()),
            completed_nodes: plan.execution.completed_nodes(),
            resumable_nodes: plan.execution.resumable_nodes(),
            updated_at: ts,
        });
        task.status = TaskStatus::Running;
        task.updated_at = ts;
        let updated = task.clone();
        push_ledger_entry(
            &mut inner,
            task_id,
            "node_retry_scheduled",
            TaskStatus::Running,
            Some(node_id.to_string()),
            ts,
        );
        Ok(updated)
    }

    pub fn attach_node_verification(
        &self,
        task_id: &str,
        node_id: &str,
        command: impl Into<String>,
        required: bool,
    ) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        let Some(plan) = task.plan.as_mut() else {
            return Err(format!("task {task_id} has no persisted plan"));
        };
        let command = command.into();
        plan.execution
            .attach_verification_gate(node_id, command.clone(), required)?;
        let ts = now_secs();
        plan.updated_at = ts;
        plan.resume_cursor = Some(TaskResumeCursor {
            node_id: Some(node_id.to_string()),
            completed_nodes: plan.execution.completed_nodes(),
            resumable_nodes: plan.execution.resumable_nodes(),
            updated_at: ts,
        });
        task.status = TaskStatus::WaitingForVerification;
        task.updated_at = ts;
        let updated = task.clone();
        push_ledger_entry(
            &mut inner,
            task_id,
            "node_verification_gate_recorded",
            TaskStatus::WaitingForVerification,
            Some(format!("{node_id}: {command}")),
            ts,
        );
        Ok(updated)
    }

    pub fn compact_task_events(&self, task_id: &str, keep_last: usize) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        if let Some(plan) = task.plan.as_mut() {
            if plan.execution.events.len() > keep_last {
                let removed = plan.execution.events.len() - keep_last;
                plan.execution.events.drain(0..removed);
            }
        }
        if task.recovery_events.len() > keep_last {
            let removed = task.recovery_events.len() - keep_last;
            task.recovery_events.drain(0..removed);
        }
        if task.team_events.len() > keep_last {
            let removed = task.team_events.len() - keep_last;
            task.team_events.drain(0..removed);
        }
        if task.route_feedback.len() > keep_last {
            let removed = task.route_feedback.len() - keep_last;
            task.route_feedback.drain(0..removed);
        }
        let ts = now_secs();
        task.updated_at = ts;
        let updated = task.clone();
        push_ledger_entry(
            &mut inner,
            task_id,
            "task_events_compacted",
            updated.status,
            Some(format!("keep_last={keep_last}")),
            ts,
        );
        Ok(updated)
    }

    pub fn resume(&self, task_id: &str) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        if matches!(
            task.status,
            TaskStatus::Completed | TaskStatus::Stopped | TaskStatus::Cancelled
        ) {
            return Err(format!(
                "task {task_id} cannot resume from terminal state: {}",
                task.status
            ));
        }
        let ts = now_secs();
        task.status = TaskStatus::Running;
        task.attempt = task.attempt.saturating_add(1);
        task.updated_at = ts;
        let resumed = task.clone();
        push_ledger_entry(
            &mut inner,
            task_id,
            "resumed",
            TaskStatus::Running,
            Some(format!("attempt {}", resumed.attempt)),
            ts,
        );
        Ok(resumed)
    }

    pub fn cancel(&self, task_id: &str) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        if matches!(
            task.status,
            TaskStatus::Completed
                | TaskStatus::Failed
                | TaskStatus::Stopped
                | TaskStatus::Cancelled
        ) {
            return Err(format!(
                "task {task_id} is already in terminal state: {}",
                task.status
            ));
        }
        let ts = now_secs();
        task.status = TaskStatus::Cancelled;
        task.updated_at = ts;
        let cancelled = task.clone();
        push_ledger_entry(
            &mut inner,
            task_id,
            "cancelled",
            TaskStatus::Cancelled,
            None,
            ts,
        );
        Ok(cancelled)
    }

    pub fn remove(&self, task_id: &str) -> Option<Task> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let removed = inner.tasks.remove(task_id)?;
        push_ledger_entry(
            &mut inner,
            task_id,
            "removed",
            removed.status,
            None,
            now_secs(),
        );
        Some(removed)
    }

    /// Clear any recorded verification result so the next completion check
    /// re-verifies against the current task state. Used by the bounded
    /// fix-verify re-drive loop after a recovery attempt mutates the workspace.
    pub fn clear_verification(&self, task_id: &str) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        task.verification_result = None;
        task.updated_at = now_secs();
        Ok(task.clone())
    }

    pub fn record_verification(
        &self,
        task_id: &str,
        result: VerificationResult,
    ) -> Result<Task, String> {
        let mut inner = self.inner.lock().expect("registry lock poisoned");
        let task = inner
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        let ts = now_secs();
        task.verification_result = Some(result.clone());
        enrich_latest_route_feedback(
            task,
            Some(result.passed),
            Some(result.passed),
            None,
            None,
            None,
            None,
            None,
            Some(result.summary.clone()),
        );
        task.updated_at = ts;
        let updated = task.clone();
        push_ledger_entry(
            &mut inner,
            task_id,
            "verification_recorded",
            updated.status,
            Some(result.summary),
            ts,
        );
        Ok(updated)
    }

    pub fn save_to_dir(&self, dir: &Path) -> io::Result<()> {
        let inner = self.inner.lock().expect("registry lock poisoned");
        fs::create_dir_all(dir)?;
        let snapshot = TaskRegistrySnapshot {
            version: SNAPSHOT_VERSION,
            tasks: inner.tasks.values().cloned().collect(),
            ledger: inner.ledger.clone(),
            event_log: inner.event_log.clone(),
            counter: inner.counter,
            ledger_counter: inner.ledger_counter,
        };
        let json = serde_json::to_string_pretty(&snapshot)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        fs::write(dir.join("tasks.json"), format!("{json}\n"))?;
        let ledger_jsonl = snapshot
            .ledger
            .iter()
            .map(|entry| {
                serde_json::to_string(entry)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            })
            .collect::<io::Result<Vec<_>>>()?
            .join("\n");
        fs::write(dir.join("ledger.jsonl"), format!("{ledger_jsonl}\n"))?;
        append_event_log_to_dir(dir, &snapshot.event_log)?;
        Ok(())
    }

    pub fn load_from_dir(dir: &Path) -> io::Result<Self> {
        let path = dir.join("tasks.json");
        let contents = fs::read_to_string(path)?;
        let snapshot = serde_json::from_str::<TaskRegistrySnapshot>(&contents)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let event_log_from_disk = read_event_log_from_dir(dir)?;
        let snapshot_event_log = if snapshot.event_log.is_empty() {
            snapshot
                .ledger
                .iter()
                .map(TaskEventLogEntry::from)
                .collect::<Vec<_>>()
        } else {
            snapshot.event_log
        };
        let event_log = if event_log_from_disk.is_empty() {
            snapshot_event_log
        } else {
            event_log_from_disk
        };
        let ledger = if snapshot.ledger.is_empty() {
            event_log
                .iter()
                .map(ProgressLedgerEntry::from)
                .collect::<Vec<_>>()
        } else {
            snapshot.ledger
        };
        let ledger_counter = snapshot
            .ledger_counter
            .max(
                ledger
                    .iter()
                    .map(|entry| entry.seq)
                    .max()
                    .unwrap_or_default(),
            )
            .max(
                event_log
                    .iter()
                    .map(|entry| entry.seq)
                    .max()
                    .unwrap_or_default(),
            );
        let tasks = snapshot
            .tasks
            .into_iter()
            .map(|task| (task.task_id.clone(), task))
            .collect();
        Ok(Self {
            inner: Arc::new(Mutex::new(RegistryInner {
                tasks,
                counter: snapshot.counter,
                ledger,
                event_log,
                ledger_counter,
            })),
        })
    }

    pub fn ledger(&self) -> Vec<ProgressLedgerEntry> {
        let inner = self.inner.lock().expect("registry lock poisoned");
        inner.ledger.clone()
    }

    pub fn event_log(&self) -> Vec<TaskEventLogEntry> {
        let inner = self.inner.lock().expect("registry lock poisoned");
        inner.event_log.clone()
    }

    pub fn event_log_for_task(&self, task_id: &str) -> Vec<TaskEventLogEntry> {
        let inner = self.inner.lock().expect("registry lock poisoned");
        inner
            .event_log
            .iter()
            .filter(|entry| entry.task_id == task_id)
            .cloned()
            .collect()
    }

    pub fn ledger_for_task(&self, task_id: &str) -> Vec<ProgressLedgerEntry> {
        let inner = self.inner.lock().expect("registry lock poisoned");
        inner
            .ledger
            .iter()
            .filter(|entry| entry.task_id == task_id)
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        let inner = self.inner.lock().expect("registry lock poisoned");
        inner.tasks.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_and_retrieves_tasks() {
        let registry = TaskRegistry::new();
        let task = registry.create("Do something", Some("A test task"));
        assert_eq!(task.status, TaskStatus::Created);
        assert_eq!(task.prompt, "Do something");
        assert_eq!(task.description.as_deref(), Some("A test task"));
        assert_eq!(task.task_packet, None);

        let fetched = registry.get(&task.task_id).expect("task should exist");
        assert_eq!(fetched.task_id, task.task_id);
    }

    #[test]
    fn creates_task_from_packet() {
        let registry = TaskRegistry::new();
        let packet = TaskPacket {
            objective: "Ship task packet support".to_string(),
            scope: "runtime/task system".to_string(),
            repo: "Himalaya-code-parity".to_string(),
            branch_policy: "origin/main only".to_string(),
            acceptance_tests: vec!["cargo test --workspace".to_string()],
            commit_policy: "single commit".to_string(),
            reporting_contract: "print commit sha".to_string(),
            escalation_policy: "manual escalation".to_string(),
        };

        let task = registry
            .create_from_packet(packet.clone())
            .expect("packet-backed task should be created");

        assert_eq!(task.prompt, packet.objective);
        assert_eq!(task.description.as_deref(), Some("runtime/task system"));
        assert_eq!(task.task_packet, Some(packet.clone()));

        let fetched = registry.get(&task.task_id).expect("task should exist");
        assert_eq!(fetched.task_packet, Some(packet));
    }

    #[test]
    fn lists_tasks_with_optional_filter() {
        let registry = TaskRegistry::new();
        registry.create("Task A", None);
        let task_b = registry.create("Task B", None);
        registry
            .set_status(&task_b.task_id, TaskStatus::Running)
            .expect("set status should succeed");

        let all = registry.list(None);
        assert_eq!(all.len(), 2);

        let running = registry.list(Some(TaskStatus::Running));
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].task_id, task_b.task_id);

        let created = registry.list(Some(TaskStatus::Created));
        assert_eq!(created.len(), 1);
    }

    #[test]
    fn stops_running_task() {
        let registry = TaskRegistry::new();
        let task = registry.create("Stoppable", None);
        registry
            .set_status(&task.task_id, TaskStatus::Running)
            .unwrap();

        let stopped = registry.stop(&task.task_id).expect("stop should succeed");
        assert_eq!(stopped.status, TaskStatus::Stopped);

        // Stopping again should fail
        let result = registry.stop(&task.task_id);
        assert!(result.is_err());
    }

    #[test]
    fn updates_task_with_messages() {
        let registry = TaskRegistry::new();
        let task = registry.create("Messageable", None);
        let updated = registry
            .update(&task.task_id, "Here's more context")
            .expect("update should succeed");
        assert_eq!(updated.messages.len(), 1);
        assert_eq!(updated.messages[0].content, "Here's more context");
        assert_eq!(updated.messages[0].role, "user");
    }

    #[test]
    fn appends_and_retrieves_output() {
        let registry = TaskRegistry::new();
        let task = registry.create("Output task", None);
        registry
            .append_output(&task.task_id, "line 1\n")
            .expect("append should succeed");
        registry
            .append_output(&task.task_id, "line 2\n")
            .expect("append should succeed");

        let output = registry.output(&task.task_id).expect("output should exist");
        assert_eq!(output, "line 1\nline 2\n");
    }

    #[test]
    fn records_progress_ledger_for_task_lifecycle() {
        let registry = TaskRegistry::new();
        let task = registry.create("Ledger task", Some("Track events"));
        registry
            .set_status(&task.task_id, TaskStatus::Running)
            .expect("set status should succeed");
        registry
            .update(&task.task_id, "more context")
            .expect("update should succeed");
        registry
            .append_output(&task.task_id, "line\n")
            .expect("append should succeed");
        registry
            .assign_team(&task.task_id, "team-ledger")
            .expect("assign should succeed");
        let removed = registry
            .remove(&task.task_id)
            .expect("remove should succeed");
        assert_eq!(removed.status, TaskStatus::Running);

        let entries = registry.ledger_for_task(&task.task_id);
        let events = entries
            .iter()
            .map(|entry| entry.event.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            events,
            vec![
                "created",
                "status_changed",
                "updated",
                "output_appended",
                "team_assigned",
                "removed",
            ]
        );
        assert!(entries
            .windows(2)
            .all(|window| window[0].seq < window[1].seq));
        assert_eq!(registry.event_log().len(), entries.len());
        assert_eq!(
            registry.event_log_for_task(&task.task_id).len(),
            entries.len()
        );
        assert_eq!(
            registry
                .event_log_for_task(&task.task_id)
                .iter()
                .map(|entry| entry.event.as_str())
                .collect::<Vec<_>>(),
            events
        );
    }

    #[test]
    fn persists_append_only_task_event_log() {
        let dir = std::env::temp_dir().join(format!(
            "Himalaya-task-events-{}-{}",
            std::process::id(),
            now_secs()
        ));
        let _ = fs::remove_dir_all(&dir);

        let registry = TaskRegistry::new();
        let task = registry.create("Persist events", Some("audit"));
        registry
            .set_status(&task.task_id, TaskStatus::Running)
            .expect("set status should succeed");
        registry
            .save_to_dir(&dir)
            .expect("first save should persist");
        registry
            .save_to_dir(&dir)
            .expect("second save should not duplicate");

        let event_log_contents =
            fs::read_to_string(dir.join("events.jsonl")).expect("events jsonl should be persisted");
        let event_lines = event_log_contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>();
        assert_eq!(event_lines.len(), 2);

        let loaded = TaskRegistry::load_from_dir(&dir).expect("registry should load");
        assert_eq!(loaded.event_log().len(), 2);
        assert_eq!(loaded.ledger().len(), 2);
        assert_eq!(loaded.event_log()[0].event, "created");
        assert_eq!(loaded.event_log()[1].event, "status_changed");

        loaded
            .append_output(&task.task_id, "line\n")
            .expect("append should succeed");
        loaded.save_to_dir(&dir).expect("third save should append");
        let event_log_contents =
            fs::read_to_string(dir.join("events.jsonl")).expect("events jsonl should be persisted");
        let event_lines = event_log_contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>();
        assert_eq!(event_lines.len(), 3);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn assigns_team_and_removes_task() {
        let registry = TaskRegistry::new();
        let task = registry.create("Team task", None);
        registry
            .assign_team(&task.task_id, "team_abc")
            .expect("assign should succeed");

        let fetched = registry.get(&task.task_id).unwrap();
        assert_eq!(fetched.team_id.as_deref(), Some("team_abc"));

        let removed = registry.remove(&task.task_id);
        assert!(removed.is_some());
        assert!(registry.get(&task.task_id).is_none());
        assert!(registry.is_empty());
    }

    #[test]
    fn rejects_operations_on_missing_task() {
        let registry = TaskRegistry::new();
        assert!(registry.stop("nonexistent").is_err());
        assert!(registry.update("nonexistent", "msg").is_err());
        assert!(registry.output("nonexistent").is_err());
        assert!(registry.append_output("nonexistent", "data").is_err());
        assert!(registry
            .set_status("nonexistent", TaskStatus::Running)
            .is_err());
    }

    #[test]
    fn task_status_display_all_variants() {
        // given
        let cases = [
            (TaskStatus::Created, "created"),
            (TaskStatus::Planning, "planning"),
            (TaskStatus::Running, "running"),
            (TaskStatus::WaitingForPermission, "waiting_for_permission"),
            (
                TaskStatus::WaitingForVerification,
                "waiting_for_verification",
            ),
            (TaskStatus::Recovering, "recovering"),
            (TaskStatus::Blocked, "blocked"),
            (TaskStatus::Completed, "completed"),
            (TaskStatus::Failed, "failed"),
            (TaskStatus::Stopped, "stopped"),
            (TaskStatus::Cancelled, "cancelled"),
        ];

        // when
        let rendered: Vec<_> = cases
            .into_iter()
            .map(|(status, expected)| (status.to_string(), expected))
            .collect();

        // then
        assert_eq!(
            rendered,
            vec![
                ("created".to_string(), "created"),
                ("planning".to_string(), "planning"),
                ("running".to_string(), "running"),
                (
                    "waiting_for_permission".to_string(),
                    "waiting_for_permission",
                ),
                (
                    "waiting_for_verification".to_string(),
                    "waiting_for_verification",
                ),
                ("recovering".to_string(), "recovering"),
                ("blocked".to_string(), "blocked"),
                ("completed".to_string(), "completed"),
                ("failed".to_string(), "failed"),
                ("stopped".to_string(), "stopped"),
                ("cancelled".to_string(), "cancelled"),
            ]
        );
    }

    #[test]
    fn stop_rejects_completed_task() {
        // given
        let registry = TaskRegistry::new();
        let task = registry.create("done", None);
        registry
            .set_status(&task.task_id, TaskStatus::Completed)
            .expect("set status should succeed");

        // when
        let result = registry.stop(&task.task_id);

        // then
        let error = result.expect_err("completed task should be rejected");
        assert!(error.contains("already in terminal state"));
        assert!(error.contains("completed"));
    }

    #[test]
    fn stop_rejects_failed_task() {
        // given
        let registry = TaskRegistry::new();
        let task = registry.create("failed", None);
        registry
            .set_status(&task.task_id, TaskStatus::Failed)
            .expect("set status should succeed");

        // when
        let result = registry.stop(&task.task_id);

        // then
        let error = result.expect_err("failed task should be rejected");
        assert!(error.contains("already in terminal state"));
        assert!(error.contains("failed"));
    }

    #[test]
    fn stop_succeeds_from_created_state() {
        // given
        let registry = TaskRegistry::new();
        let task = registry.create("created task", None);

        // when
        let stopped = registry.stop(&task.task_id).expect("stop should succeed");

        // then
        assert_eq!(stopped.status, TaskStatus::Stopped);
        assert!(stopped.updated_at >= task.updated_at);
    }

    #[test]
    fn new_registry_is_empty() {
        // given
        let registry = TaskRegistry::new();

        // when
        let all_tasks = registry.list(None);

        // then
        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
        assert!(all_tasks.is_empty());
    }

    #[test]
    fn create_without_description() {
        // given
        let registry = TaskRegistry::new();

        // when
        let task = registry.create("Do the thing", None);

        // then
        assert!(task.task_id.starts_with("task_"));
        assert_eq!(task.description, None);
        assert_eq!(task.task_packet, None);
        assert!(task.messages.is_empty());
        assert!(task.output.is_empty());
        assert_eq!(task.team_id, None);
        assert_eq!(task.attempt, 0);
        assert_eq!(task.checkpoints.len(), 1);
    }

    #[test]
    fn persists_plan_recovery_team_and_route_feedback() {
        let registry = TaskRegistry::new();
        let task = registry.create("Long task", Some("checkpointed"));
        let dag = PlanDag {
            task_id: task.task_id.clone(),
            root_id: task.task_id.clone(),
            nodes: vec![crate::PlanDagNode {
                kind: crate::PlanNodeKind::Task,
                id: task.task_id.clone(),
                title: "Long task".to_string(),
                parallelizable: false,
                estimated_effort: 1,
                candidate_tools: Vec::new(),
                notes: Vec::new(),
            }],
            edges: Vec::new(),
        };
        let execution = PlanExecution::new(&dag);
        registry
            .record_plan(&task.task_id, dag.clone(), execution.clone())
            .expect("plan should record");
        registry
            .record_checkpoint(&task.task_id, "after_plan", Some("planned".to_string()))
            .expect("checkpoint should record");
        registry
            .record_recovery_event(&task.task_id, RecoveryEvent::RecoverySucceeded)
            .expect("recovery event should record");
        let team_event = TeamExecutionEvent {
            seq: 1,
            team_id: "team-1".to_string(),
            task_id: task.task_id.clone(),
            role: crate::TeamRole::Planner,
            kind: crate::TeamExecutionEventKind::TaskAssigned,
            model_route: None,
            message: Some("assigned".to_string()),
        };
        registry
            .record_team_event(&task.task_id, team_event)
            .expect("team event should record");
        let route = crate::ModelRouteDecision {
            phase: crate::ModelRoutePhase::Planning,
            model: "sonnet".to_string(),
            provider: None,
            reason: "test".to_string(),
            confidence: Some(0.8),
            fallback_model: Some("opus".to_string()),
        };
        registry
            .record_route_feedback(
                &task.task_id,
                ModelRouteFeedback::pending(task.task_id.clone(), route, now_secs())
                    .with_outcome(true, None, false, None),
            )
            .expect("route feedback should record");

        registry
            .record_verification(
                &task.task_id,
                VerificationResult {
                    task_id: task.task_id.clone(),
                    passed: true,
                    observed_green_level: Some(crate::green_contract::GreenLevel::Workspace),
                    summary: "verified".to_string(),
                    evidence: vec!["cargo test".to_string()],
                },
            )
            .expect("verification should record");

        let loaded = registry.get(&task.task_id).expect("task should exist");
        assert!(loaded.plan.is_some());
        assert_eq!(loaded.checkpoints.len(), 2);
        assert_eq!(loaded.recovery_events.len(), 1);
        assert_eq!(loaded.team_events.len(), 1);
        assert_eq!(loaded.route_feedback.len(), 1);
        assert_eq!(loaded.route_feedback[0].succeeded, Some(true));
        assert_eq!(loaded.route_feedback[0].verification_passed, Some(true));
        assert!(loaded.route_feedback[0].recovery_triggered);
        assert_eq!(loaded.route_feedback[0].note.as_deref(), Some("verified"));
    }

    #[test]
    fn update_latest_route_feedback_merges_runtime_metrics() {
        let registry = TaskRegistry::new();
        let task = registry.create("Route metrics", None);
        let route = crate::ModelRouteDecision {
            phase: crate::ModelRoutePhase::Coding,
            model: "sonnet".to_string(),
            provider: None,
            reason: "test".to_string(),
            confidence: Some(0.8),
            fallback_model: None,
        };
        registry
            .record_route_feedback(
                &task.task_id,
                ModelRouteFeedback::pending(task.task_id.clone(), route.clone(), now_secs()),
            )
            .expect("pending route feedback should record");
        registry
            .update_latest_route_feedback(
                &task.task_id,
                ModelRouteFeedback::pending(task.task_id.clone(), route, now_secs())
                    .with_metrics(Some(42), Some(100), Some(25), Some(0.001))
                    .with_outcome(true, None, false, None),
            )
            .expect("route feedback metrics should update");

        let loaded = registry.get(&task.task_id).expect("task should exist");
        assert_eq!(loaded.route_feedback.len(), 1);
        assert_eq!(loaded.route_feedback[0].latency_ms, Some(42));
        assert_eq!(loaded.route_feedback[0].total_tokens(), Some(125));
        assert_eq!(loaded.route_feedback[0].succeeded, Some(true));
    }

    #[test]
    fn remove_nonexistent_returns_none() {
        // given
        let registry = TaskRegistry::new();

        // when
        let removed = registry.remove("missing");

        // then
        assert!(removed.is_none());
    }

    #[test]
    fn assign_team_rejects_missing_task() {
        // given
        let registry = TaskRegistry::new();

        // when
        let result = registry.assign_team("missing", "team_123");

        // then
        let error = result.expect_err("missing task should be rejected");
        assert_eq!(error, "task not found: missing");
    }
}
