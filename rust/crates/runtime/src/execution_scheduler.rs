use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::task_registry::Task as RegistryTask;
use crate::{
    PlanDag, PlanExecution, PlanExecutionEvent, PlanNodeStatus, TaskExecutionEngine,
    TaskExecutionOutcome, TaskRegistry, TaskStatus, VerificationRunner, WorkerRegistry,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerNodeSelection {
    pub node_id: String,
    pub event_offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerNodeOutcome {
    pub node_id: String,
    pub event_offset: usize,
    pub events: Vec<PlanExecutionEvent>,
}

pub struct ExecutionScheduler<'a> {
    dag: &'a PlanDag,
    execution: &'a mut PlanExecution,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableSchedulerStatus {
    Pending,
    Running,
    Completed,
    Blocked,
    Idle,
}

impl std::fmt::Display for DurableSchedulerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Running => write!(f, "running"),
            Self::Completed => write!(f, "completed"),
            Self::Blocked => write!(f, "blocked"),
            Self::Idle => write!(f, "idle"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableSchedulerTaskSnapshot {
    pub task_id: String,
    pub status: DurableSchedulerStatus,
    pub task_status: TaskStatus,
    pub current_node: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DurableSchedulerTick {
    pub status: DurableSchedulerStatus,
    pub selected_task_id: Option<String>,
    pub task: Option<RegistryTask>,
    pub outcome: Option<TaskExecutionOutcome>,
    pub queue: Vec<DurableSchedulerTaskSnapshot>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerDaemonStatus {
    Idle,
    Running,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerDaemonState {
    pub version: u32,
    pub status: SchedulerDaemonStatus,
    pub pid: u32,
    pub started_at: u64,
    pub updated_at: u64,
    pub tick_count: u64,
    pub lock_path: PathBuf,
    pub last_tick: Option<DurableSchedulerTick>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerDaemonEvent {
    pub seq: u64,
    pub timestamp: u64,
    pub event: String,
    pub status: SchedulerDaemonStatus,
    pub selected_task_id: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerDaemonRun {
    pub state: SchedulerDaemonState,
    pub tick: DurableSchedulerTick,
    pub event: SchedulerDaemonEvent,
}

#[derive(Debug, Clone)]
pub struct SchedulerDaemon {
    scheduler: DurableTaskScheduler,
    state_dir: PathBuf,
}

const SCHEDULER_DAEMON_STATE_VERSION: u32 = 1;

fn scheduler_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl SchedulerDaemonStatus {
    #[must_use]
    pub fn from_tick_status(status: DurableSchedulerStatus) -> Self {
        match status {
            DurableSchedulerStatus::Pending
            | DurableSchedulerStatus::Running
            | DurableSchedulerStatus::Completed => Self::Running,
            DurableSchedulerStatus::Blocked => Self::Blocked,
            DurableSchedulerStatus::Idle => Self::Idle,
        }
    }
}

impl SchedulerDaemon {
    #[must_use]
    pub fn new(scheduler: DurableTaskScheduler, state_dir: impl Into<PathBuf>) -> Self {
        Self {
            scheduler,
            state_dir: state_dir.into(),
        }
    }

    pub fn run_once(&self) -> io::Result<SchedulerDaemonRun> {
        fs::create_dir_all(&self.state_dir)?;
        let lock = SchedulerDaemonLock::acquire(self.lock_path())?;
        let previous_state = self.load_state().ok();
        let tick = self
            .scheduler
            .tick()
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error))?;
        let now = scheduler_now_secs();
        let status = SchedulerDaemonStatus::from_tick_status(tick.status);
        let state = SchedulerDaemonState {
            version: SCHEDULER_DAEMON_STATE_VERSION,
            status,
            pid: std::process::id(),
            started_at: previous_state
                .as_ref()
                .map_or(now, |state| state.started_at),
            updated_at: now,
            tick_count: previous_state
                .as_ref()
                .map_or(1, |state| state.tick_count.saturating_add(1)),
            lock_path: lock.path.clone(),
            last_tick: Some(tick.clone()),
            message: tick.message.clone(),
        };
        self.save_state(&state)?;
        let event = self.append_event(&state, &tick)?;
        drop(lock);
        Ok(SchedulerDaemonRun { state, tick, event })
    }

    pub fn load_state(&self) -> io::Result<SchedulerDaemonState> {
        let contents = fs::read_to_string(self.state_path())?;
        serde_json::from_str(&contents)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    #[must_use]
    pub fn state_path(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }

    #[must_use]
    pub fn events_path(&self) -> PathBuf {
        self.state_dir.join("events.jsonl")
    }

    #[must_use]
    pub fn lock_path(&self) -> PathBuf {
        self.state_dir.join("scheduler.lock")
    }

    fn save_state(&self, state: &SchedulerDaemonState) -> io::Result<()> {
        let json = serde_json::to_string_pretty(state)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        fs::write(self.state_path(), format!("{json}\n"))
    }

    fn append_event(
        &self,
        state: &SchedulerDaemonState,
        tick: &DurableSchedulerTick,
    ) -> io::Result<SchedulerDaemonEvent> {
        let event = SchedulerDaemonEvent {
            seq: state.tick_count,
            timestamp: state.updated_at,
            event: "tick".to_string(),
            status: state.status,
            selected_task_id: tick.selected_task_id.clone(),
            message: tick.message.clone(),
        };
        let line = serde_json::to_string(&event)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.events_path())?;
        writeln!(file, "{line}")?;
        Ok(event)
    }
}

struct SchedulerDaemonLock {
    path: PathBuf,
}

impl SchedulerDaemonLock {
    fn acquire(path: PathBuf) -> io::Result<Self> {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                writeln!(file, "{}", std::process::id())?;
                Ok(Self { path })
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("scheduler lock already exists: {}", path.display()),
            )),
            Err(error) => Err(error),
        }
    }
}

impl Drop for SchedulerDaemonLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Debug, Clone)]
pub struct DurableTaskScheduler {
    registry: TaskRegistry,
    verification_runner: VerificationRunner,
    worker_registry: Option<WorkerRegistry>,
}

impl DurableTaskScheduler {
    #[must_use]
    pub fn new(registry: TaskRegistry, verification_runner: VerificationRunner) -> Self {
        Self {
            registry,
            verification_runner,
            worker_registry: None,
        }
    }

    #[must_use]
    pub fn with_workers(
        registry: TaskRegistry,
        verification_runner: VerificationRunner,
        worker_registry: WorkerRegistry,
    ) -> Self {
        Self {
            registry,
            verification_runner,
            worker_registry: Some(worker_registry),
        }
    }

    pub fn tick(&self) -> Result<DurableSchedulerTick, String> {
        let queue_before = self.queue();
        let Some(snapshot) = queue_before
            .iter()
            .find(|task| {
                matches!(
                    task.status,
                    DurableSchedulerStatus::Pending | DurableSchedulerStatus::Running
                )
            })
            .cloned()
        else {
            return Ok(DurableSchedulerTick {
                status: DurableSchedulerStatus::Idle,
                selected_task_id: None,
                task: None,
                outcome: None,
                queue: queue_before,
                message: "no runnable tasks".to_string(),
            });
        };

        let engine = if let Some(worker_registry) = self.worker_registry.clone() {
            TaskExecutionEngine::with_workers(
                self.registry.clone(),
                self.verification_runner.clone(),
                worker_registry,
            )
        } else {
            TaskExecutionEngine::new(self.registry.clone(), self.verification_runner.clone())
        };
        let _ = engine.assign_team(&snapshot.task_id)?;
        let outcome = engine.execute(&snapshot.task_id, snapshot.current_node.as_deref())?;
        let task = self
            .registry
            .get(&snapshot.task_id)
            .ok_or_else(|| format!("task not found after scheduler tick: {}", snapshot.task_id))?;
        let status = durable_status_for_task(&task);
        let message = match status {
            DurableSchedulerStatus::Completed => "task completed".to_string(),
            DurableSchedulerStatus::Blocked => outcome.message.clone(),
            DurableSchedulerStatus::Running => "task still running".to_string(),
            DurableSchedulerStatus::Pending => "task remains pending".to_string(),
            DurableSchedulerStatus::Idle => "scheduler idle".to_string(),
        };
        Ok(DurableSchedulerTick {
            status,
            selected_task_id: Some(snapshot.task_id),
            task: Some(task),
            outcome: Some(outcome),
            queue: self.queue(),
            message,
        })
    }

    #[must_use]
    pub fn queue(&self) -> Vec<DurableSchedulerTaskSnapshot> {
        let mut tasks = self.registry.list(None);
        tasks.sort_by_key(|task| (task.created_at, task.task_id.clone()));
        tasks
            .into_iter()
            .map(|task| DurableSchedulerTaskSnapshot {
                task_id: task.task_id.clone(),
                status: durable_status_for_task(&task),
                task_status: task.status,
                current_node: task
                    .plan
                    .as_ref()
                    .and_then(|plan| plan.resume_cursor.as_ref())
                    .and_then(|cursor| cursor.node_id.clone()),
            })
            .collect()
    }
}

#[must_use]
pub fn durable_status_for_task(task: &RegistryTask) -> DurableSchedulerStatus {
    match task.status {
        TaskStatus::Created | TaskStatus::Planning | TaskStatus::WaitingForPermission => {
            DurableSchedulerStatus::Pending
        }
        TaskStatus::Running | TaskStatus::Recovering | TaskStatus::WaitingForVerification => {
            DurableSchedulerStatus::Running
        }
        TaskStatus::Blocked | TaskStatus::Failed => DurableSchedulerStatus::Blocked,
        TaskStatus::Completed => DurableSchedulerStatus::Completed,
        TaskStatus::Stopped | TaskStatus::Cancelled => DurableSchedulerStatus::Idle,
    }
}

impl<'a> ExecutionScheduler<'a> {
    #[must_use]
    pub fn new(dag: &'a PlanDag, execution: &'a mut PlanExecution) -> Self {
        Self { dag, execution }
    }

    pub fn start_ready_node_for_tool(&mut self, tool_name: &str) -> Option<SchedulerNodeSelection> {
        let node_id = self
            .execution
            .ready_nodes()
            .into_iter()
            .filter(|node_id| node_id != &self.dag.root_id)
            .find(|node_id| {
                self.execution.nodes.get(node_id).is_some_and(|node| {
                    node.candidate_tools
                        .iter()
                        .any(|candidate| candidate == tool_name)
                })
            })?;
        let event_offset = self.execution.events.len();
        self.execution.start_node(&node_id).ok()?;
        Some(SchedulerNodeSelection {
            node_id,
            event_offset,
        })
    }

    pub fn fail_node(
        &mut self,
        node_id: &str,
        failure_class: impl Into<String>,
    ) -> SchedulerNodeOutcome {
        let event_offset = self.execution.events.len();
        let _ = self.execution.fail_node(self.dag, node_id, failure_class);
        self.outcome(node_id, event_offset)
    }

    pub fn finish_node(
        &mut self,
        node_id: &str,
        succeeded: bool,
        summary: Option<String>,
        failure_class: impl Into<String>,
    ) -> SchedulerNodeOutcome {
        let event_offset = self.execution.events.len();
        let outcome = if succeeded {
            self.execution.succeed_node(self.dag, node_id, summary)
        } else {
            self.execution.fail_node(self.dag, node_id, failure_class)
        };
        if outcome.is_err() && self.execution.nodes.contains_key(node_id) {
            if let Some(node) = self.execution.nodes.get_mut(node_id) {
                node.retry_count = node.retry_count.saturating_add(1);
            }
        }
        self.outcome(node_id, event_offset)
    }

    #[must_use]
    pub fn events_since(&self, offset: usize) -> &[PlanExecutionEvent] {
        self.execution.events_since(offset)
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.execution.is_finished()
    }

    #[must_use]
    fn outcome(&self, node_id: &str, event_offset: usize) -> SchedulerNodeOutcome {
        SchedulerNodeOutcome {
            node_id: node_id.to_string(),
            event_offset,
            events: self.execution.events_since(event_offset).to_vec(),
        }
    }

    #[must_use]
    pub fn status(&self, node_id: &str) -> Option<PlanNodeStatus> {
        self.execution.nodes.get(node_id).map(|node| node.status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PlanDagEdge, PlanDagEdgeKind, PlanDagNode, PlanNodeKind};

    fn sample_dag() -> PlanDag {
        PlanDag {
            task_id: "task-1".to_string(),
            root_id: "task-1".to_string(),
            nodes: vec![
                PlanDagNode {
                    kind: PlanNodeKind::Task,
                    id: "task-1".to_string(),
                    title: "Do work".to_string(),
                    parallelizable: false,
                    estimated_effort: 2,
                    candidate_tools: Vec::new(),
                    notes: Vec::new(),
                },
                PlanDagNode {
                    kind: PlanNodeKind::Step,
                    id: "task-1-read".to_string(),
                    title: "Read".to_string(),
                    parallelizable: false,
                    estimated_effort: 1,
                    candidate_tools: vec!["read_file".to_string()],
                    notes: Vec::new(),
                },
            ],
            edges: vec![PlanDagEdge {
                from: "task-1".to_string(),
                to: "task-1-read".to_string(),
                kind: PlanDagEdgeKind::Contains,
            }],
        }
    }

    fn registry_with_planned_task() -> (TaskRegistry, String) {
        let registry = TaskRegistry::new();
        let task = registry.create("scheduled task", Some("durable scheduler"));
        let dag = PlanDag {
            task_id: task.task_id.clone(),
            root_id: task.task_id.clone(),
            nodes: vec![PlanDagNode {
                kind: PlanNodeKind::Step,
                id: "node-1".to_string(),
                title: "Node".to_string(),
                parallelizable: false,
                estimated_effort: 1,
                candidate_tools: Vec::new(),
                notes: Vec::new(),
            }],
            edges: Vec::new(),
        };
        registry
            .record_plan(&task.task_id, dag.clone(), PlanExecution::new(&dag))
            .expect("plan should record");
        (registry, task.task_id)
    }

    #[test]
    fn starts_ready_node_for_matching_tool() {
        let dag = sample_dag();
        let mut execution = PlanExecution::new(&dag);
        let mut scheduler = ExecutionScheduler::new(&dag, &mut execution);
        let selection = scheduler
            .start_ready_node_for_tool("read_file")
            .expect("matching node should start");
        assert_eq!(selection.node_id, "task-1-read");
        assert_eq!(
            scheduler.status("task-1-read"),
            Some(PlanNodeStatus::Running)
        );
        assert!(!scheduler.events_since(selection.event_offset).is_empty());
    }

    #[test]
    fn durable_scheduler_tick_completes_planned_task() {
        let (registry, task_id) = registry_with_planned_task();
        let scheduler = DurableTaskScheduler::new(registry.clone(), VerificationRunner::new(None));

        let tick = scheduler.tick().expect("tick should run");

        assert_eq!(tick.status, DurableSchedulerStatus::Completed);
        assert_eq!(tick.selected_task_id.as_deref(), Some(task_id.as_str()));
        assert!(tick.outcome.expect("outcome").completed);
        assert_eq!(
            registry.get(&task_id).expect("task").status,
            TaskStatus::Completed
        );
    }

    #[test]
    fn scheduler_daemon_persists_state_and_events() {
        let (registry, task_id) = registry_with_planned_task();
        let scheduler = DurableTaskScheduler::new(registry.clone(), VerificationRunner::new(None));
        let state_dir = std::env::temp_dir().join(format!(
            "himalaya-scheduler-daemon-{}",
            scheduler_now_secs()
        ));
        let _ = std::fs::remove_dir_all(&state_dir);
        let daemon = SchedulerDaemon::new(scheduler, &state_dir);

        let first = daemon.run_once().expect("daemon should tick once");
        assert_eq!(first.state.tick_count, 1);
        assert_eq!(
            first.tick.selected_task_id.as_deref(),
            Some(task_id.as_str())
        );
        assert!(daemon.state_path().exists());
        assert!(daemon.events_path().exists());
        assert!(!daemon.lock_path().exists());

        let second = daemon.run_once().expect("daemon should tick twice");
        assert_eq!(second.state.tick_count, 2);
        assert_eq!(second.tick.status, DurableSchedulerStatus::Idle);

        let events = std::fs::read_to_string(daemon.events_path()).expect("events should read");
        assert_eq!(events.lines().count(), 2);
        let loaded = daemon.load_state().expect("state should load");
        assert_eq!(loaded.tick_count, 2);
        assert_eq!(loaded.status, SchedulerDaemonStatus::Idle);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn scheduler_daemon_refuses_existing_lock() {
        let (registry, _) = registry_with_planned_task();
        let scheduler = DurableTaskScheduler::new(registry, VerificationRunner::new(None));
        let state_dir = std::env::temp_dir().join(format!(
            "himalaya-scheduler-daemon-lock-{}",
            scheduler_now_secs()
        ));
        let _ = std::fs::remove_dir_all(&state_dir);
        std::fs::create_dir_all(&state_dir).expect("state dir should create");
        std::fs::write(state_dir.join("scheduler.lock"), "other").expect("lock should write");
        let daemon = SchedulerDaemon::new(scheduler, &state_dir);

        let error = daemon.run_once().expect_err("existing lock should block");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn durable_scheduler_tick_blocks_task_without_plan() {
        let registry = TaskRegistry::new();
        let task = registry.create("scheduled task", Some("missing plan"));
        let scheduler = DurableTaskScheduler::new(registry.clone(), VerificationRunner::new(None));

        let tick = scheduler.tick().expect("tick should run");

        assert_eq!(tick.status, DurableSchedulerStatus::Blocked);
        assert_eq!(
            tick.selected_task_id.as_deref(),
            Some(task.task_id.as_str())
        );
        assert!(tick.outcome.expect("outcome").blocked);
        assert_eq!(
            registry.get(&task.task_id).expect("task").status,
            TaskStatus::Blocked
        );

        let second_tick = scheduler.tick().expect("second tick should run");
        assert_eq!(second_tick.status, DurableSchedulerStatus::Idle);
    }
}
