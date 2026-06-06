use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    DecisioningEvent, ModelRouteDecision, PlanExecutionEvent, ProgressLedgerEntry,
    RecoveryActionExecution, RecoveryEvent, TaskExecutionOutcome, TeamExecutionEvent,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeEvent {
    Decisioning(Box<DecisioningEvent>),
    PlanExecution(PlanExecutionEvent),
    TaskLedger(ProgressLedgerEntry),
    ModelRoute(ModelRouteDecision),
    TeamExecution(TeamExecutionEvent),
    Recovery(RecoveryEvent),
    RecoveryAction(RecoveryActionExecution),
    TaskExecution(TaskExecutionOutcome),
}

impl RuntimeEvent {
    #[must_use]
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::Decisioning(_) => "decisioning_event",
            Self::PlanExecution(_) => "plan_execution_event",
            Self::TaskLedger(_) => "task_ledger_event",
            Self::ModelRoute(_) => "model_route_event",
            Self::TeamExecution(_) => "team_execution_event",
            Self::Recovery(_) => "recovery_event",
            Self::RecoveryAction(_) => "recovery_action_event",
            Self::TaskExecution(_) => "task_execution_event",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeEventSeverity {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeEventEnvelope {
    pub schema_version: u32,
    pub event_id: String,
    pub event_type: String,
    pub timestamp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_event_id: Option<String>,
    pub severity: RuntimeEventSeverity,
    pub payload: Value,
}

impl RuntimeEventEnvelope {
    pub const SCHEMA_VERSION: u32 = 1;

    pub fn from_runtime_event(event: &RuntimeEvent) -> Result<Self, serde_json::Error> {
        let payload = serde_json::to_value(event)?;
        Ok(Self::from_payload(
            event.event_type(),
            payload,
            None,
            task_id_for_runtime_event(event),
            None,
        ))
    }

    #[must_use]
    pub fn from_payload(
        event_type: impl Into<String>,
        payload: Value,
        run_id: Option<String>,
        task_id: Option<String>,
        worker_id: Option<String>,
    ) -> Self {
        Self::from_payload_at(event_type, payload, run_id, task_id, worker_id, now_secs())
    }

    #[must_use]
    pub fn from_payload_at(
        event_type: impl Into<String>,
        payload: Value,
        run_id: Option<String>,
        task_id: Option<String>,
        worker_id: Option<String>,
        timestamp: u64,
    ) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            event_id: next_event_id(),
            event_type: event_type.into(),
            timestamp,
            run_id,
            task_id,
            worker_id,
            parent_event_id: None,
            severity: RuntimeEventSeverity::Info,
            payload,
        }
    }

    #[must_use]
    pub fn with_parent_event_id(mut self, parent_event_id: impl Into<String>) -> Self {
        self.parent_event_id = Some(parent_event_id.into());
        self
    }

    #[must_use]
    pub fn with_severity(mut self, severity: RuntimeEventSeverity) -> Self {
        self.severity = severity;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeEventLog {
    path: PathBuf,
}

impl RuntimeEventLog {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, envelope: &RuntimeEventEnvelope) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let line = serde_json::to_string(envelope)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    pub fn read_all(&self) -> io::Result<Vec<RuntimeEventEnvelope>> {
        read_runtime_event_log(&self.path)
    }
}

pub fn append_runtime_event_log(path: &Path, envelope: &RuntimeEventEnvelope) -> io::Result<()> {
    RuntimeEventLog::new(path).append(envelope)
}

pub fn read_runtime_event_log(path: &Path) -> io::Result<Vec<RuntimeEventEnvelope>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(path)?;
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<RuntimeEventEnvelope>(line)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        })
        .collect()
}

pub trait RuntimeEventReporter: Send + Sync {
    fn emit_runtime_event(&self, event: &RuntimeEvent);
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn next_event_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    format!("evt_{:x}_{:x}_{:x}", now_secs(), std::process::id(), seq)
}

fn task_id_for_runtime_event(event: &RuntimeEvent) -> Option<String> {
    match event {
        RuntimeEvent::Decisioning(_)
        | RuntimeEvent::PlanExecution(_)
        | RuntimeEvent::ModelRoute(_) => None,
        RuntimeEvent::TaskLedger(value) => Some(value.task_id.clone()),
        RuntimeEvent::TeamExecution(value) => Some(value.task_id.clone()),
        RuntimeEvent::Recovery(_) => None,
        RuntimeEvent::RecoveryAction(value) => Some(value.task_id.clone()),
        RuntimeEvent::TaskExecution(value) => Some(value.task_id.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "runtime-event-log-{label}-{}-{}",
            std::process::id(),
            now_secs()
        ))
    }

    #[test]
    fn runtime_event_envelope_preserves_type_and_task_id() {
        let event = RuntimeEvent::TaskLedger(ProgressLedgerEntry {
            seq: 1,
            task_id: "task-1".to_string(),
            event: "created".to_string(),
            status: crate::TaskStatus::Created,
            message: Some("created task".to_string()),
            timestamp: 123,
        });

        let envelope =
            RuntimeEventEnvelope::from_runtime_event(&event).expect("event should serialize");

        assert_eq!(
            envelope.schema_version,
            RuntimeEventEnvelope::SCHEMA_VERSION
        );
        assert_eq!(envelope.event_type, "task_ledger_event");
        assert_eq!(envelope.task_id.as_deref(), Some("task-1"));
        assert_eq!(envelope.severity, RuntimeEventSeverity::Info);
        assert_eq!(envelope.payload["task_ledger"]["task_id"], "task-1");
    }

    #[test]
    fn runtime_event_log_appends_and_reads_jsonl() {
        let dir = unique_temp_path("append-read");
        let path = dir.join("runtime.jsonl");
        let log = RuntimeEventLog::new(&path);
        let envelope = RuntimeEventEnvelope::from_payload(
            "worker_event",
            serde_json::json!({"seq": 1, "kind": "ready_for_prompt"}),
            Some("run-1".to_string()),
            None,
            Some("worker-1".to_string()),
        )
        .with_severity(RuntimeEventSeverity::Warn);

        log.append(&envelope).expect("append should succeed");
        let loaded = log.read_all().expect("log should read");

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].event_type, "worker_event");
        assert_eq!(loaded[0].worker_id.as_deref(), Some("worker-1"));
        assert_eq!(loaded[0].severity, RuntimeEventSeverity::Warn);

        let _ = fs::remove_dir_all(dir);
    }
}
