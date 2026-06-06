use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::task_registry::Task as RegistryTask;
use crate::{
    PlanDag, PlanExecution, PlanExecutionEvent, PlanNodeStatus, TaskExecutionEngine,
    TaskExecutionOutcome, TaskExecutionReport, TaskRegistry, TaskStatus, VerificationRunner,
    WorkerRegistry, WorkerStatus,
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
    pub priority: i32,
    pub runnable: bool,
    pub skip_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableSchedulerDecisionTrace {
    pub task_id: String,
    pub status: DurableSchedulerStatus,
    pub priority: i32,
    pub selected: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DurableSchedulerTick {
    pub status: DurableSchedulerStatus,
    pub selected_task_id: Option<String>,
    pub task: Option<RegistryTask>,
    pub outcome: Option<TaskExecutionOutcome>,
    pub report: Option<TaskExecutionReport>,
    pub queue: Vec<DurableSchedulerTaskSnapshot>,
    pub decision_trace: Vec<DurableSchedulerDecisionTrace>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DurableSchedulerExplain {
    pub task_id: String,
    pub selected_task_id: Option<String>,
    pub would_select: bool,
    pub task: DurableSchedulerTaskSnapshot,
    pub decision_trace: Vec<DurableSchedulerDecisionTrace>,
    pub queue: Vec<DurableSchedulerTaskSnapshot>,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerDaemonStatus {
    Idle,
    Running,
    Blocked,
    Stopped,
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
    pub recovery_event: Option<SchedulerDaemonEvent>,
    pub recovered_stale_lock: bool,
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
        let recovered_stale_lock = lock.recovered_stale_lock;
        let previous_state = self.load_state().ok();
        let tick = self.scheduler.tick().map_err(io::Error::other)?;
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
        let recovery_event = if recovered_stale_lock {
            Some(self.append_recover_stale_lock_event(&state)?)
        } else {
            None
        };
        let event = self.append_event(&state, &tick)?;
        drop(lock);
        Ok(SchedulerDaemonRun {
            state,
            tick,
            event,
            recovery_event,
            recovered_stale_lock,
        })
    }

    pub fn load_state(&self) -> io::Result<SchedulerDaemonState> {
        let contents = fs::read_to_string(self.state_path())?;
        serde_json::from_str(&contents)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    pub fn stop(&self) -> io::Result<SchedulerDaemonState> {
        fs::create_dir_all(&self.state_dir)?;
        let _lock = SchedulerDaemonLock::acquire(self.lock_path())?;
        let previous_state = self.load_state().ok();
        let now = scheduler_now_secs();
        let state = SchedulerDaemonState {
            version: SCHEDULER_DAEMON_STATE_VERSION,
            status: SchedulerDaemonStatus::Stopped,
            pid: std::process::id(),
            started_at: previous_state
                .as_ref()
                .map_or(now, |state| state.started_at),
            updated_at: now,
            tick_count: previous_state.as_ref().map_or(0, |state| state.tick_count),
            lock_path: self.lock_path(),
            last_tick: previous_state.and_then(|state| state.last_tick),
            message: "scheduler daemon stopped".to_string(),
        };
        self.save_state(&state)?;
        let event = SchedulerDaemonEvent {
            seq: self.next_event_seq()?,
            timestamp: state.updated_at,
            event: "stop".to_string(),
            status: state.status,
            selected_task_id: None,
            message: state.message.clone(),
        };
        self.append_event_line(&event)?;
        Ok(state)
    }

    pub fn load_events(&self) -> io::Result<Vec<SchedulerDaemonEvent>> {
        let contents = fs::read_to_string(self.events_path())?;
        contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                serde_json::from_str(line)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            })
            .collect()
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
            seq: self.next_event_seq()?,
            timestamp: state.updated_at,
            event: "tick".to_string(),
            status: state.status,
            selected_task_id: tick.selected_task_id.clone(),
            message: tick.message.clone(),
        };
        self.append_event_line(&event)?;
        Ok(event)
    }

    fn append_recover_stale_lock_event(
        &self,
        state: &SchedulerDaemonState,
    ) -> io::Result<SchedulerDaemonEvent> {
        let event = SchedulerDaemonEvent {
            seq: self.next_event_seq()?,
            timestamp: state.updated_at,
            event: "recover_stale_lock".to_string(),
            status: state.status,
            selected_task_id: None,
            message: format!(
                "recovered stale scheduler lock: {}",
                state.lock_path.display()
            ),
        };
        self.append_event_line(&event)?;
        Ok(event)
    }

    fn append_event_line(&self, event: &SchedulerDaemonEvent) -> io::Result<()> {
        let line = serde_json::to_string(event)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.events_path())?;
        writeln!(file, "{line}")?;
        Ok(())
    }

    fn next_event_seq(&self) -> io::Result<u64> {
        match fs::read_to_string(self.events_path()) {
            Ok(contents) => Ok(contents
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count()
                .saturating_add(1) as u64),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(1),
            Err(error) => Err(error),
        }
    }
}

struct SchedulerDaemonLock {
    path: PathBuf,
    recovered_stale_lock: bool,
}

impl SchedulerDaemonLock {
    fn acquire(path: PathBuf) -> io::Result<Self> {
        match Self::create(path.clone()) {
            Ok(lock) => Ok(lock),
            Err(error)
                if error.kind() == io::ErrorKind::AlreadyExists && lock_owner_is_stale(&path) =>
            {
                let _ = fs::remove_file(&path);
                let mut lock = Self::create(path)?;
                lock.recovered_stale_lock = true;
                Ok(lock)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("scheduler lock already exists: {}", path.display()),
            )),
            Err(error) => Err(error),
        }
    }

    fn create(path: PathBuf) -> io::Result<Self> {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                writeln!(file, "{}", std::process::id())?;
                Ok(Self {
                    path,
                    recovered_stale_lock: false,
                })
            }
            Err(error) => Err(error),
        }
    }
}

fn lock_owner_is_stale(path: &PathBuf) -> bool {
    let Ok(contents) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(pid) = contents.trim().parse::<u32>() else {
        return false;
    };
    !process_is_alive(pid)
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    std::path::Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    true
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
    permission_mode: crate::PermissionMode,
}

impl DurableTaskScheduler {
    #[must_use]
    pub fn new(registry: TaskRegistry, verification_runner: VerificationRunner) -> Self {
        Self {
            registry,
            verification_runner,
            worker_registry: None,
            permission_mode: crate::PermissionMode::DangerFullAccess,
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
            permission_mode: crate::PermissionMode::DangerFullAccess,
        }
    }

    #[must_use]
    pub fn with_permission_mode(mut self, permission_mode: crate::PermissionMode) -> Self {
        self.permission_mode = permission_mode;
        self
    }

    pub fn tick(&self) -> Result<DurableSchedulerTick, String> {
        let queue_before = self.queue();
        let selected = selected_scheduler_task(&queue_before);
        let decision_trace = scheduler_decision_trace(&queue_before, selected.as_ref());
        let Some(snapshot) = selected else {
            return Ok(DurableSchedulerTick {
                status: DurableSchedulerStatus::Idle,
                selected_task_id: None,
                task: None,
                outcome: None,
                report: None,
                queue: queue_before,
                decision_trace,
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
        let report = engine.execute_with_recovery(
            &snapshot.task_id,
            snapshot.current_node.as_deref(),
            self.permission_mode,
        )?;
        let outcome = report.outcome.clone();
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
            report: Some(report),
            queue: self.queue(),
            decision_trace,
            message,
        })
    }

    pub fn explain(&self, task_id: &str) -> Result<DurableSchedulerExplain, String> {
        let queue = self.queue();
        let selected = selected_scheduler_task(&queue);
        let decision_trace = scheduler_decision_trace(&queue, selected.as_ref());
        let task = queue
            .iter()
            .find(|task| task.task_id == task_id)
            .cloned()
            .ok_or_else(|| format!("task not found: {task_id}"))?;
        let reason = decision_trace
            .iter()
            .find(|trace| trace.task_id == task_id)
            .map(|trace| trace.reason.clone())
            .unwrap_or_else(|| "task not present in scheduler decision trace".to_string());
        Ok(DurableSchedulerExplain {
            task_id: task_id.to_string(),
            selected_task_id: selected.as_ref().map(|task| task.task_id.clone()),
            would_select: selected
                .as_ref()
                .is_some_and(|selected| selected.task_id == task_id),
            task,
            decision_trace,
            queue,
            reason,
        })
    }

    #[must_use]
    pub fn queue(&self) -> Vec<DurableSchedulerTaskSnapshot> {
        let mut tasks = self.registry.list(None);
        tasks.sort_by_key(|task| (task.created_at, task.task_id.clone()));
        tasks
            .into_iter()
            .map(|task| task_snapshot(task, self.worker_registry.as_ref()))
            .collect()
    }
}

fn selected_scheduler_task(
    queue: &[DurableSchedulerTaskSnapshot],
) -> Option<DurableSchedulerTaskSnapshot> {
    queue
        .iter()
        .filter(|task| task.runnable)
        .max_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| right.task_id.cmp(&left.task_id))
        })
        .cloned()
}

fn task_snapshot(
    task: RegistryTask,
    worker_registry: Option<&WorkerRegistry>,
) -> DurableSchedulerTaskSnapshot {
    let status = durable_status_for_task(&task);
    let current_node = task
        .plan
        .as_ref()
        .and_then(|plan| plan.resume_cursor.as_ref())
        .and_then(|cursor| cursor.node_id.clone());
    let waiting_for_worker = task.plan.as_ref().is_some_and(|plan| {
        plan.execution.nodes.values().any(|node| {
            node.status == PlanNodeStatus::Running
                && node.worker_id.as_deref().is_some_and(|worker_id| {
                    worker_registry
                        .and_then(|registry| registry.get(worker_id))
                        .map_or(true, |worker| {
                            matches!(
                                worker.status,
                                WorkerStatus::Spawning
                                    | WorkerStatus::ReadyForPrompt
                                    | WorkerStatus::PromptAccepted
                                    | WorkerStatus::Running
                            )
                        })
                })
        })
    });
    let has_executable_recovery = task.plan.is_some()
        && task
            .recovery_action_executions
            .last()
            .is_some_and(|execution| {
                execution.results.iter().any(|result| {
                    result.executed
                        && matches!(
                            result.action.kind,
                            crate::RecoveryActionKind::RetryNode
                                | crate::RecoveryActionKind::RerunVerification
                                | crate::RecoveryActionKind::SwitchModel
                                | crate::RecoveryActionKind::RetryMcpHandshake
                                | crate::RecoveryActionKind::RestartPlugin
                        )
                })
            });
    let (priority, runnable, skip_reason) = match task.status {
        TaskStatus::Running | TaskStatus::Recovering => {
            if waiting_for_worker {
                (40, false, Some("waiting for worker completion".to_string()))
            } else {
                (90, true, None)
            }
        }
        TaskStatus::WaitingForVerification => (80, true, None),
        TaskStatus::Created | TaskStatus::Planning => (70, true, None),
        TaskStatus::Blocked | TaskStatus::Failed if has_executable_recovery => (60, true, None),
        TaskStatus::Blocked | TaskStatus::Failed => (
            10,
            false,
            Some("blocked without executable recovery action".to_string()),
        ),
        TaskStatus::WaitingForPermission => {
            (5, false, Some("waiting for user permission".to_string()))
        }
        TaskStatus::Completed | TaskStatus::Stopped | TaskStatus::Cancelled => {
            (0, false, Some("terminal task".to_string()))
        }
    };
    DurableSchedulerTaskSnapshot {
        task_id: task.task_id,
        status,
        task_status: task.status,
        current_node,
        priority,
        runnable,
        skip_reason,
    }
}

fn scheduler_decision_trace(
    queue: &[DurableSchedulerTaskSnapshot],
    selected: Option<&DurableSchedulerTaskSnapshot>,
) -> Vec<DurableSchedulerDecisionTrace> {
    queue
        .iter()
        .map(|task| {
            let selected_task = selected.is_some_and(|selected| selected.task_id == task.task_id);
            DurableSchedulerDecisionTrace {
                task_id: task.task_id.clone(),
                status: task.status,
                priority: task.priority,
                selected: selected_task,
                reason: if selected_task {
                    "selected highest-priority runnable task".to_string()
                } else {
                    task.skip_reason
                        .clone()
                        .unwrap_or_else(|| "lower priority than selected task".to_string())
                },
            }
        })
        .collect()
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

    fn registry_with_packet_task(acceptance_tests: Vec<String>) -> (TaskRegistry, String) {
        let registry = TaskRegistry::new();
        let task = registry
            .create_from_packet(crate::TaskPacket {
                objective: "scheduled packet task".to_string(),
                scope: "runtime".to_string(),
                repo: ".".to_string(),
                branch_policy: "current branch".to_string(),
                acceptance_tests,
                commit_policy: "no commit".to_string(),
                reporting_contract: "return scheduler status".to_string(),
                escalation_policy: "block on failure".to_string(),
            })
            .expect("packet task should create");
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
        let report = tick.report.expect("report");
        assert!(report.completed);
        assert_eq!(report.final_status, TaskStatus::Completed);
        assert_eq!(
            registry.get(&task_id).expect("task").status,
            TaskStatus::Completed
        );
    }

    #[test]
    fn durable_scheduler_tick_uses_recovery_execution_report() {
        let (registry, task_id) =
            registry_with_packet_task(vec!["python3 -c 'import sys; sys.exit(1)'".to_string()]);
        let scheduler = DurableTaskScheduler::new(registry.clone(), VerificationRunner::new(None));

        let tick = scheduler.tick().expect("tick should run");

        assert_eq!(tick.status, DurableSchedulerStatus::Blocked);
        let report = tick.report.expect("report");
        assert_eq!(report.task_id, task_id);
        assert!(report.blocked);
        assert!(report.failure.is_some());
        assert!(report.recovery.is_some());
        assert!(registry
            .get(&report.task_id)
            .expect("task")
            .route_feedback
            .iter()
            .any(|feedback| feedback.recovery_triggered));
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
    fn scheduler_daemon_recovers_stale_lock_from_dead_process() {
        let (registry, task_id) = registry_with_planned_task();
        let scheduler = DurableTaskScheduler::new(registry, VerificationRunner::new(None));
        let state_dir = std::env::temp_dir().join(format!(
            "himalaya-scheduler-daemon-stale-lock-{}",
            scheduler_now_secs()
        ));
        let _ = std::fs::remove_dir_all(&state_dir);
        std::fs::create_dir_all(&state_dir).expect("state dir should create");
        std::fs::write(state_dir.join("scheduler.lock"), "0").expect("stale lock should write");
        let daemon = SchedulerDaemon::new(scheduler, &state_dir);

        let run = daemon.run_once().expect("stale lock should be recovered");

        assert_eq!(run.tick.selected_task_id.as_deref(), Some(task_id.as_str()));
        assert_eq!(run.state.tick_count, 1);
        assert!(run.recovered_stale_lock);
        assert_eq!(
            run.recovery_event
                .as_ref()
                .map(|event| event.event.as_str()),
            Some("recover_stale_lock")
        );
        assert!(daemon
            .load_events()
            .expect("events should load")
            .iter()
            .any(|event| event.event == "recover_stale_lock"));
        assert!(!daemon.lock_path().exists());
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn scheduler_daemon_stop_persists_recoverable_terminal_state() {
        let (registry, _) = registry_with_planned_task();
        let scheduler = DurableTaskScheduler::new(registry, VerificationRunner::new(None));
        let state_dir = std::env::temp_dir().join(format!(
            "himalaya-scheduler-daemon-stop-{}",
            scheduler_now_secs()
        ));
        let _ = std::fs::remove_dir_all(&state_dir);
        let daemon = SchedulerDaemon::new(scheduler, &state_dir);
        let first = daemon.run_once().expect("daemon should run before stop");

        let stopped = daemon.stop().expect("daemon should stop");

        assert_eq!(stopped.status, SchedulerDaemonStatus::Stopped);
        assert_eq!(stopped.tick_count, first.state.tick_count);
        let loaded = daemon.load_state().expect("stopped state should load");
        assert_eq!(loaded.status, SchedulerDaemonStatus::Stopped);
        let events = daemon.load_events().expect("events should load");
        assert!(events.iter().any(|event| event.event == "stop"));
        assert!(!daemon.lock_path().exists());
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn scheduler_daemon_rejects_corrupt_event_log() {
        let (registry, _) = registry_with_planned_task();
        let scheduler = DurableTaskScheduler::new(registry, VerificationRunner::new(None));
        let state_dir = std::env::temp_dir().join(format!(
            "himalaya-scheduler-daemon-corrupt-events-{}",
            scheduler_now_secs()
        ));
        let _ = std::fs::remove_dir_all(&state_dir);
        std::fs::create_dir_all(&state_dir).expect("state dir should create");
        std::fs::write(state_dir.join("events.jsonl"), "not-json\n")
            .expect("corrupt event log should write");
        let daemon = SchedulerDaemon::new(scheduler, &state_dir);

        let error = daemon.load_events().expect_err("corrupt log should fail");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn durable_scheduler_tick_blocks_task_without_plan() {
        let registry = TaskRegistry::new();
        let task = registry.create("scheduled task", Some("missing plan"));
        let scheduler = DurableTaskScheduler::new(registry.clone(), VerificationRunner::new(None));

        let initial_explain = scheduler
            .explain(&task.task_id)
            .expect("scheduler explain should work before tick");
        assert!(initial_explain.would_select);
        assert!(initial_explain.reason.contains("selected"));

        let tick = scheduler.tick().expect("tick should run");

        assert_eq!(tick.status, DurableSchedulerStatus::Blocked);
        assert_eq!(
            tick.selected_task_id.as_deref(),
            Some(task.task_id.as_str())
        );
        assert!(tick.outcome.expect("outcome").blocked);
        assert!(tick.report.expect("report").blocked);
        assert_eq!(
            registry.get(&task.task_id).expect("task").status,
            TaskStatus::Blocked
        );

        let second_tick = scheduler.tick().expect("second tick should run");
        assert_eq!(second_tick.status, DurableSchedulerStatus::Idle);
        assert_eq!(second_tick.selected_task_id, None);
        assert!(second_tick.decision_trace.iter().any(|trace| {
            trace.task_id == task.task_id
                && !trace.selected
                && trace.reason.contains("blocked without executable recovery")
        }));
        let blocked_explain = scheduler
            .explain(&task.task_id)
            .expect("scheduler explain should work after block");
        assert!(!blocked_explain.would_select);
        assert!(blocked_explain
            .reason
            .contains("blocked without executable recovery"));
    }

    #[test]
    fn scheduler_trace_skips_worker_waiting_task_and_selects_runnable_task() {
        let (registry, waiting_task_id) =
            registry_with_packet_task(vec!["python3 --version".to_string()]);
        registry
            .set_status(&waiting_task_id, TaskStatus::Running)
            .expect("waiting task should run first");
        let (_, runnable_task_id) = {
            let task = registry.create("second", Some("runnable"));
            let dag = PlanDag {
                task_id: task.task_id.clone(),
                root_id: task.task_id.clone(),
                nodes: vec![crate::PlanDagNode {
                    kind: crate::PlanNodeKind::Step,
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
                .expect("plan");
            (registry.clone(), task.task_id)
        };
        let workers = WorkerRegistry::new();
        let scheduler = DurableTaskScheduler::with_workers(
            registry.clone(),
            VerificationRunner::new(None),
            workers,
        );

        let first = scheduler.tick().expect("first tick should dispatch worker");
        assert_eq!(
            first.selected_task_id.as_deref(),
            Some(waiting_task_id.as_str())
        );
        let second = scheduler
            .tick()
            .expect("second tick should skip worker wait");

        assert_eq!(
            second.selected_task_id.as_deref(),
            Some(runnable_task_id.as_str())
        );
        assert!(second.decision_trace.iter().any(|trace| {
            trace.task_id == waiting_task_id
                && !trace.selected
                && trace.reason.contains("waiting for worker")
        }));
        assert!(second
            .decision_trace
            .iter()
            .any(|trace| { trace.task_id == runnable_task_id && trace.selected }));
    }
}
