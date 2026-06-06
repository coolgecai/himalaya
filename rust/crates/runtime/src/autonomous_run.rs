use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{
    DurableSchedulerStatus, DurableSchedulerTaskSnapshot, PermissionMode, RecoveryActionKind,
    RecoveryActionRisk, SchedulerDaemon, SchedulerDaemonRun, SchedulerDaemonState, TaskRegistry,
    TaskStatus, WorkerSupervisor, WorkerSupervisorStatus, WorkerSupervisorTick,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutonomousRunStatus {
    Idle,
    Running,
    Blocked,
}

impl std::fmt::Display for AutonomousRunStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle => write!(f, "idle"),
            Self::Running => write!(f, "running"),
            Self::Blocked => write!(f, "blocked"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomousBlockedRecoveryAction {
    pub kind: RecoveryActionKind,
    pub risk: RecoveryActionRisk,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomousRecoveryPolicyAudit {
    pub task_id: String,
    pub task_status: TaskStatus,
    pub permission_mode: String,
    pub recovery_triggered: bool,
    pub executed_actions: usize,
    pub blocked_actions: Vec<AutonomousBlockedRecoveryAction>,
    pub requires_user: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomousRunStep {
    pub seq: u64,
    pub status: AutonomousRunStatus,
    pub worker_supervisor: WorkerSupervisorTick,
    pub scheduler: SchedulerDaemonRun,
    pub policy_audit: Vec<AutonomousRecoveryPolicyAudit>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomousRunReport {
    pub run_id: String,
    pub started_at: u64,
    pub updated_at: u64,
    pub status: AutonomousRunStatus,
    pub permission_mode: String,
    pub max_ticks: usize,
    pub tick_count: usize,
    pub worker_supervisor_ticks: Vec<WorkerSupervisorTick>,
    pub scheduler_runs: Vec<SchedulerDaemonRun>,
    pub policy_audit: Vec<AutonomousRecoveryPolicyAudit>,
    pub final_queue: Vec<DurableSchedulerTaskSnapshot>,
    pub latest_daemon_state: Option<SchedulerDaemonState>,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct AutonomousRunCoordinator {
    daemon: SchedulerDaemon,
    supervisor: WorkerSupervisor,
    tasks: TaskRegistry,
    state_dir: PathBuf,
    permission_mode: PermissionMode,
}

impl AutonomousRunCoordinator {
    #[must_use]
    pub fn new(
        daemon: SchedulerDaemon,
        supervisor: WorkerSupervisor,
        tasks: TaskRegistry,
        state_dir: impl Into<PathBuf>,
        permission_mode: PermissionMode,
    ) -> Self {
        Self {
            daemon,
            supervisor,
            tasks,
            state_dir: state_dir.into(),
            permission_mode,
        }
    }

    pub fn run(&self, max_ticks: usize) -> io::Result<AutonomousRunReport> {
        self.run_with_persist(max_ticks, |_| Ok(()))
    }

    pub fn run_with_persist<F>(
        &self,
        max_ticks: usize,
        mut persist_tick: F,
    ) -> io::Result<AutonomousRunReport>
    where
        F: FnMut(&AutonomousRunStep) -> io::Result<()>,
    {
        fs::create_dir_all(&self.state_dir)?;
        let max_ticks = max_ticks.max(1);
        let run_seq = next_run_seq(&self.state_dir)?;
        let started_at = now_secs();
        let run_id = format!("auto_run_{started_at}_{run_seq}");
        let mut steps = Vec::new();
        let mut worker_supervisor_ticks = Vec::new();
        let mut scheduler_runs = Vec::new();
        let mut policy_audit = Vec::new();
        let mut final_queue = Vec::new();
        let mut latest_daemon_state = None;
        let mut status = AutonomousRunStatus::Idle;
        let mut message = "autonomous run did not tick".to_string();

        for index in 0..max_ticks {
            let supervisor_tick = self.supervisor.observe().map_err(io::Error::other)?;
            let scheduler_run = self.daemon.run_once()?;
            let audit = recovery_policy_audit(&self.tasks, self.permission_mode);
            status = status_for_step(&supervisor_tick, &scheduler_run, &audit);
            message = message_for_status(status, &supervisor_tick, &scheduler_run, &audit);
            final_queue = scheduler_run.tick.queue.clone();
            latest_daemon_state = Some(scheduler_run.state.clone());
            let step = AutonomousRunStep {
                seq: index.saturating_add(1) as u64,
                status,
                worker_supervisor: supervisor_tick.clone(),
                scheduler: scheduler_run.clone(),
                policy_audit: audit.clone(),
                message: message.clone(),
            };
            persist_tick(&step)?;
            worker_supervisor_ticks.push(supervisor_tick);
            scheduler_runs.push(scheduler_run);
            policy_audit = audit;
            steps.push(step);
            if matches!(
                status,
                AutonomousRunStatus::Idle | AutonomousRunStatus::Blocked
            ) {
                break;
            }
        }

        let report = AutonomousRunReport {
            run_id,
            started_at,
            updated_at: now_secs(),
            status,
            permission_mode: self.permission_mode.as_str().to_string(),
            max_ticks,
            tick_count: steps.len(),
            worker_supervisor_ticks,
            scheduler_runs,
            policy_audit,
            final_queue,
            latest_daemon_state,
            message,
        };
        append_autonomous_run_report(&self.state_dir, &report)?;
        Ok(report)
    }

    #[must_use]
    pub fn runs_path(&self) -> PathBuf {
        autonomous_runs_path(&self.state_dir)
    }

    pub fn load_runs(&self, limit: usize) -> io::Result<Vec<AutonomousRunReport>> {
        load_autonomous_run_reports(&self.state_dir, limit)
    }

    pub fn latest_run(&self) -> io::Result<Option<AutonomousRunReport>> {
        latest_autonomous_run_report(&self.state_dir)
    }
}

#[must_use]
pub fn autonomous_runs_path(state_dir: &Path) -> PathBuf {
    state_dir.join("runs.jsonl")
}

pub fn append_autonomous_run_report(
    state_dir: &Path,
    report: &AutonomousRunReport,
) -> io::Result<()> {
    fs::create_dir_all(state_dir)?;
    let line = serde_json::to_string(report)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(autonomous_runs_path(state_dir))?;
    writeln!(file, "{line}")?;
    Ok(())
}

pub fn load_autonomous_run_reports(
    state_dir: &Path,
    limit: usize,
) -> io::Result<Vec<AutonomousRunReport>> {
    let path = autonomous_runs_path(state_dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(path)?;
    let mut reports = contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<AutonomousRunReport>(line)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        })
        .collect::<io::Result<Vec<_>>>()?;
    if limit > 0 && reports.len() > limit {
        let start = reports.len() - limit;
        reports = reports.split_off(start);
    }
    Ok(reports)
}

pub fn latest_autonomous_run_report(state_dir: &Path) -> io::Result<Option<AutonomousRunReport>> {
    Ok(load_autonomous_run_reports(state_dir, 1)?.pop())
}

fn next_run_seq(state_dir: &Path) -> io::Result<u64> {
    let path = autonomous_runs_path(state_dir);
    if !path.exists() {
        return Ok(1);
    }
    let contents = fs::read_to_string(path)?;
    Ok(contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
        .saturating_add(1) as u64)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn recovery_policy_audit(
    tasks: &TaskRegistry,
    permission_mode: PermissionMode,
) -> Vec<AutonomousRecoveryPolicyAudit> {
    tasks
        .list(None)
        .into_iter()
        .filter_map(|task| {
            let blocked_actions = task
                .recovery_action_executions
                .iter()
                .flat_map(|execution| execution.results.iter())
                .filter(|result| result.blocked)
                .map(|result| AutonomousBlockedRecoveryAction {
                    kind: result.action.kind.clone(),
                    risk: result.action.risk,
                    reason: result.reason.clone(),
                })
                .collect::<Vec<_>>();
            let executed_actions = task
                .recovery_action_executions
                .iter()
                .flat_map(|execution| execution.results.iter())
                .filter(|result| result.executed)
                .count();
            let recovery_triggered =
                !task.recovery_events.is_empty() || !task.recovery_action_executions.is_empty();
            let requires_user = task.status == TaskStatus::WaitingForPermission
                || blocked_actions
                    .iter()
                    .any(|action| action.risk == RecoveryActionRisk::NeedsHuman);
            if !recovery_triggered && !requires_user && blocked_actions.is_empty() {
                return None;
            }
            let message = if requires_user {
                "recovery is waiting for user permission".to_string()
            } else if blocked_actions.is_empty() {
                "recovery actions are allowed under current policy".to_string()
            } else {
                format!(
                    "{} recovery action(s) blocked by {}",
                    blocked_actions.len(),
                    permission_mode.as_str()
                )
            };
            Some(AutonomousRecoveryPolicyAudit {
                task_id: task.task_id,
                task_status: task.status,
                permission_mode: permission_mode.as_str().to_string(),
                recovery_triggered,
                executed_actions,
                blocked_actions,
                requires_user,
                message,
            })
        })
        .collect()
}

fn status_for_step(
    supervisor: &WorkerSupervisorTick,
    scheduler: &SchedulerDaemonRun,
    audit: &[AutonomousRecoveryPolicyAudit],
) -> AutonomousRunStatus {
    if supervisor.status == WorkerSupervisorStatus::Blocked
        || scheduler.tick.status == DurableSchedulerStatus::Blocked
        || audit.iter().any(|entry| {
            entry.requires_user
                || !entry.blocked_actions.is_empty()
                || matches!(
                    entry.task_status,
                    TaskStatus::Blocked | TaskStatus::Failed | TaskStatus::WaitingForPermission
                )
        })
    {
        return AutonomousRunStatus::Blocked;
    }
    if supervisor.status == WorkerSupervisorStatus::Running
        || matches!(
            scheduler.tick.status,
            DurableSchedulerStatus::Pending
                | DurableSchedulerStatus::Running
                | DurableSchedulerStatus::Completed
        )
    {
        return AutonomousRunStatus::Running;
    }
    AutonomousRunStatus::Idle
}

fn message_for_status(
    status: AutonomousRunStatus,
    supervisor: &WorkerSupervisorTick,
    scheduler: &SchedulerDaemonRun,
    audit: &[AutonomousRecoveryPolicyAudit],
) -> String {
    match status {
        AutonomousRunStatus::Idle => "autonomous run idle".to_string(),
        AutonomousRunStatus::Running => {
            if supervisor.status == WorkerSupervisorStatus::Running {
                "autonomous run has active workers".to_string()
            } else {
                scheduler.tick.message.clone()
            }
        }
        AutonomousRunStatus::Blocked => audit
            .iter()
            .find(|entry| entry.requires_user || !entry.blocked_actions.is_empty())
            .map(|entry| entry.message.clone())
            .unwrap_or_else(|| {
                if supervisor.status == WorkerSupervisorStatus::Blocked {
                    supervisor.message.clone()
                } else {
                    scheduler.tick.message.clone()
                }
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        DurableTaskScheduler, PlanDag, PlanDagNode, PlanExecution, PlanNodeKind, RecoveryAction,
        RecoveryActionExecution, RecoveryActionKind, RecoveryActionResult, RecoveryActionRisk,
        SchedulerDaemon, TaskRegistry, VerificationRunner, WorkerRegistry,
    };

    fn state_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("himalaya-autonomous-run-{label}-{}", now_secs()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn registry_with_planned_task() -> (TaskRegistry, String) {
        let tasks = TaskRegistry::new();
        let task = tasks.create("scheduled task", Some("autonomous run"));
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
        tasks
            .record_plan(&task.task_id, dag.clone(), PlanExecution::new(&dag))
            .expect("plan should record");
        (tasks, task.task_id)
    }

    #[test]
    fn coordinator_runs_scheduler_and_persists_report() {
        let (tasks, task_id) = registry_with_planned_task();
        let workers = WorkerRegistry::new();
        let runner = VerificationRunner::new(None);
        let scheduler =
            DurableTaskScheduler::with_workers(tasks.clone(), runner.clone(), workers.clone());
        let dir = state_dir("running");
        let coordinator = AutonomousRunCoordinator::new(
            SchedulerDaemon::new(scheduler, &dir),
            WorkerSupervisor::new(workers.clone(), tasks.clone(), runner),
            tasks.clone(),
            &dir,
            PermissionMode::ReadOnly,
        );

        let report = coordinator.run(2).expect("autonomous run should work");

        assert_eq!(report.status, AutonomousRunStatus::Running);
        assert!(!report.scheduler_runs.is_empty());
        assert!(report
            .scheduler_runs
            .iter()
            .any(|run| run.tick.selected_task_id.as_deref() == Some(task_id.as_str())));
        assert!(coordinator.runs_path().exists());
        assert_eq!(
            coordinator
                .latest_run()
                .expect("latest run should load")
                .expect("latest run")
                .run_id,
            report.run_id
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn coordinator_blocks_on_policy_gated_recovery() {
        let tasks = TaskRegistry::new();
        let task = tasks.create("blocked task", Some("autonomous recovery"));
        tasks
            .set_status(&task.task_id, TaskStatus::WaitingForPermission)
            .expect("status should update");
        tasks
            .record_recovery_action_execution(
                &task.task_id,
                RecoveryActionExecution {
                    task_id: task.task_id.clone(),
                    results: vec![RecoveryActionResult {
                        action: RecoveryAction {
                            kind: RecoveryActionKind::RequestPermission,
                            scenario: crate::FailureScenario::TrustPromptUnresolved,
                            risk: RecoveryActionRisk::NeedsHuman,
                            node_id: None,
                            message: "needs approval".to_string(),
                        },
                        executed: false,
                        blocked: true,
                        reason: "needs human approval".to_string(),
                    }],
                },
            )
            .expect("recovery action should record");
        let workers = WorkerRegistry::new();
        let runner = VerificationRunner::new(None);
        let scheduler =
            DurableTaskScheduler::with_workers(tasks.clone(), runner.clone(), workers.clone());
        let dir = state_dir("blocked");
        let coordinator = AutonomousRunCoordinator::new(
            SchedulerDaemon::new(scheduler, &dir),
            WorkerSupervisor::new(workers, tasks.clone(), runner),
            tasks,
            &dir,
            PermissionMode::ReadOnly,
        );

        let report = coordinator.run(4).expect("autonomous run should work");

        assert_eq!(report.status, AutonomousRunStatus::Blocked);
        assert_eq!(report.tick_count, 1);
        assert!(report
            .policy_audit
            .iter()
            .any(|audit| audit.requires_user && !audit.blocked_actions.is_empty()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
