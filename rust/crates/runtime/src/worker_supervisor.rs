use serde::{Deserialize, Serialize};

use crate::{
    DurableSchedulerStatus, DurableSchedulerTaskSnapshot, DurableSchedulerTick,
    DurableTaskScheduler, TaskRegistry, VerificationRunner, Worker, WorkerRegistry, WorkerStatus,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerSupervisorStatus {
    Idle,
    Running,
    Blocked,
}

impl std::fmt::Display for WorkerSupervisorStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle => write!(f, "idle"),
            Self::Running => write!(f, "running"),
            Self::Blocked => write!(f, "blocked"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerSupervisorTick {
    pub status: WorkerSupervisorStatus,
    pub workers: Vec<Worker>,
    pub active_workers: usize,
    pub blocked_workers: usize,
    pub scheduler_tick: Option<DurableSchedulerTick>,
    pub scheduler_queue: Vec<DurableSchedulerTaskSnapshot>,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct WorkerSupervisor {
    workers: WorkerRegistry,
    tasks: TaskRegistry,
    verification_runner: VerificationRunner,
}

impl WorkerSupervisor {
    #[must_use]
    pub fn new(
        workers: WorkerRegistry,
        tasks: TaskRegistry,
        verification_runner: VerificationRunner,
    ) -> Self {
        Self {
            workers,
            tasks,
            verification_runner,
        }
    }

    pub fn tick(&self) -> Result<WorkerSupervisorTick, String> {
        let scheduler =
            DurableTaskScheduler::new(self.tasks.clone(), self.verification_runner.clone());
        let queue_before = scheduler.queue();
        let scheduler_tick = queue_before
            .iter()
            .any(|task| {
                matches!(
                    task.status,
                    DurableSchedulerStatus::Pending | DurableSchedulerStatus::Running
                )
            })
            .then(|| scheduler.tick())
            .transpose()?;
        let scheduler_queue = scheduler.queue();
        let workers = self.workers.list();
        let active_workers = workers
            .iter()
            .filter(|worker| {
                matches!(
                    worker.status,
                    WorkerStatus::Spawning
                        | WorkerStatus::ReadyForPrompt
                        | WorkerStatus::PromptAccepted
                        | WorkerStatus::Running
                )
            })
            .count();
        let blocked_workers = workers
            .iter()
            .filter(|worker| {
                matches!(
                    worker.status,
                    WorkerStatus::TrustRequired | WorkerStatus::Failed
                )
            })
            .count();
        let scheduler_blocked = scheduler_queue
            .iter()
            .any(|task| task.status == DurableSchedulerStatus::Blocked)
            || scheduler_tick
                .as_ref()
                .is_some_and(|tick| tick.status == DurableSchedulerStatus::Blocked);
        let status = if blocked_workers > 0 || scheduler_blocked {
            WorkerSupervisorStatus::Blocked
        } else if active_workers > 0
            || scheduler_queue.iter().any(|task| {
                matches!(
                    task.status,
                    DurableSchedulerStatus::Pending | DurableSchedulerStatus::Running
                )
            })
        {
            WorkerSupervisorStatus::Running
        } else {
            WorkerSupervisorStatus::Idle
        };
        let message = match status {
            WorkerSupervisorStatus::Idle => "worker supervisor idle".to_string(),
            WorkerSupervisorStatus::Running => "worker supervisor has active work".to_string(),
            WorkerSupervisorStatus::Blocked => "worker supervisor found blocked work".to_string(),
        };
        Ok(WorkerSupervisorTick {
            status,
            workers,
            active_workers,
            blocked_workers,
            scheduler_tick,
            scheduler_queue,
            message,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PlanDag, PlanDagNode, PlanExecution, PlanNodeKind, TaskStatus};

    #[test]
    fn supervisor_ticks_runnable_durable_task() {
        let workers = WorkerRegistry::new();
        let tasks = TaskRegistry::new();
        let task = tasks.create("scheduled task", Some("worker supervisor"));
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
        let supervisor =
            WorkerSupervisor::new(workers, tasks.clone(), VerificationRunner::new(None));

        let tick = supervisor.tick().expect("supervisor tick should run");

        assert_eq!(tick.status, WorkerSupervisorStatus::Idle);
        assert!(tick
            .scheduler_tick
            .expect("scheduler tick")
            .outcome
            .is_some());
        assert_eq!(
            tasks.get(&task.task_id).expect("task").status,
            TaskStatus::Completed
        );
    }

    #[test]
    fn supervisor_surfaces_blocked_workers() {
        let workers = WorkerRegistry::new();
        let worker = workers.create("/tmp/repo", &[], true);
        workers
            .observe(
                &worker.worker_id,
                "Do you trust the files in this folder?\n1. Yes, proceed\n2. No",
            )
            .expect("trust prompt should be observed");
        let supervisor =
            WorkerSupervisor::new(workers, TaskRegistry::new(), VerificationRunner::new(None));

        let tick = supervisor.tick().expect("supervisor tick should run");

        assert_eq!(tick.status, WorkerSupervisorStatus::Blocked);
        assert_eq!(tick.blocked_workers, 1);
    }
}
