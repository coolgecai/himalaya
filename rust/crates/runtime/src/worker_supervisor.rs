use serde::{Deserialize, Serialize};

use crate::{
    DurableSchedulerStatus, DurableSchedulerTaskSnapshot, DurableSchedulerTick,
    DurableTaskScheduler, TaskRegistry, VerificationRunner, Worker, WorkerEvent, WorkerRegistry,
    WorkerStatus, DEFAULT_WORKER_LEASE_SECS,
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
pub struct WorkerSupervisorCapacity {
    pub max_workers: usize,
    pub active_workers: usize,
    pub available_slots: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerEventIndexEntry {
    pub worker_id: String,
    pub event: WorkerEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerSupervisorTick {
    pub status: WorkerSupervisorStatus,
    pub workers: Vec<Worker>,
    pub active_workers: usize,
    pub blocked_workers: usize,
    pub restarted_workers: usize,
    pub trust_queue: Vec<String>,
    pub capacity: WorkerSupervisorCapacity,
    pub event_index: Vec<WorkerEventIndexEntry>,
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
        self.tick_inner(true)
    }

    pub fn observe(&self) -> Result<WorkerSupervisorTick, String> {
        self.tick_inner(false)
    }

    fn tick_inner(&self, drive_scheduler: bool) -> Result<WorkerSupervisorTick, String> {
        let restarted_workers = self
            .workers
            .restart_stale_workers_now(DEFAULT_WORKER_LEASE_SECS);
        let scheduler = DurableTaskScheduler::with_workers(
            self.tasks.clone(),
            self.verification_runner.clone(),
            self.workers.clone(),
        );
        let queue_before = scheduler.queue();
        let scheduler_tick = (drive_scheduler
            && queue_before.iter().any(|task| {
                matches!(
                    task.status,
                    DurableSchedulerStatus::Pending | DurableSchedulerStatus::Running
                )
            }))
        .then(|| scheduler.tick())
        .transpose()?;
        let scheduler_queue = scheduler.queue();
        let workers = self.workers.list();
        let active_workers = workers
            .iter()
            .filter(|worker| is_active_worker(worker.status))
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
        let trust_queue = workers
            .iter()
            .filter(|worker| worker.status == WorkerStatus::TrustRequired)
            .map(|worker| worker.worker_id.clone())
            .collect::<Vec<_>>();
        let capacity = WorkerSupervisorCapacity {
            max_workers: max_workers_capacity(),
            active_workers,
            available_slots: max_workers_capacity().saturating_sub(active_workers),
        };
        let event_index = workers
            .iter()
            .flat_map(|worker| {
                worker
                    .events
                    .iter()
                    .rev()
                    .take(3)
                    .cloned()
                    .map(|event| WorkerEventIndexEntry {
                        worker_id: worker.worker_id.clone(),
                        event,
                    })
            })
            .collect::<Vec<_>>();
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
        let message = match (status, restarted_workers.len()) {
            (_, count) if count > 0 => {
                format!("worker supervisor restarted {count} stale worker(s)")
            }
            (WorkerSupervisorStatus::Idle, _) => "worker supervisor idle".to_string(),
            (WorkerSupervisorStatus::Running, _) => "worker supervisor has active work".to_string(),
            (WorkerSupervisorStatus::Blocked, _) => {
                "worker supervisor found blocked work".to_string()
            }
        };
        Ok(WorkerSupervisorTick {
            status,
            workers,
            active_workers,
            blocked_workers,
            restarted_workers: restarted_workers.len(),
            trust_queue,
            capacity,
            event_index,
            scheduler_tick,
            scheduler_queue,
            message,
        })
    }
}

fn is_active_worker(status: WorkerStatus) -> bool {
    matches!(
        status,
        WorkerStatus::Spawning
            | WorkerStatus::ReadyForPrompt
            | WorkerStatus::PromptAccepted
            | WorkerStatus::Running
    )
}

fn max_workers_capacity() -> usize {
    4
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
        let supervisor = WorkerSupervisor::new(
            workers.clone(),
            tasks.clone(),
            VerificationRunner::new(None),
        );

        let tick = supervisor.tick().expect("supervisor tick should run");

        assert_eq!(tick.status, WorkerSupervisorStatus::Running);
        assert!(tick
            .scheduler_tick
            .expect("scheduler tick")
            .outcome
            .is_some());
        assert_eq!(
            tasks.get(&task.task_id).expect("task").status,
            TaskStatus::Running
        );
        assert_eq!(workers.list().len(), 1);
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
        assert_eq!(tick.trust_queue, vec![worker.worker_id]);
    }

    #[test]
    fn supervisor_restarts_stale_workers_and_indexes_events() {
        let workers = WorkerRegistry::new();
        let worker = workers.create("/tmp/repo-stale-supervisor", &[], true);
        workers
            .observe(&worker.worker_id, "Ready for input\n>")
            .expect("ready observe should succeed");
        workers
            .send_prompt(&worker.worker_id, Some("Run stale supervisor test"))
            .expect("prompt send should succeed");
        workers
            .expire_lease_for_test(&worker.worker_id)
            .expect("worker lease should expire for test");

        let supervisor = WorkerSupervisor::new(
            workers.clone(),
            TaskRegistry::new(),
            VerificationRunner::new(None),
        );

        let tick = supervisor.tick().expect("supervisor tick should run");

        assert_eq!(tick.restarted_workers, 1);
        assert!(tick.capacity.max_workers >= tick.active_workers);
        assert!(tick
            .event_index
            .iter()
            .any(|entry| entry.worker_id == worker.worker_id));
    }
}
