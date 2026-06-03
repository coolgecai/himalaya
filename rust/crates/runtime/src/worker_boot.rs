#![allow(
    clippy::struct_excessive_bools,
    clippy::too_many_lines,
    clippy::question_mark,
    clippy::redundant_closure,
    clippy::map_unwrap_or
)]
//! In-memory worker-boot state machine and control registry.
//!
//! This provides a foundational control plane for reliable worker startup:
//! trust-gate detection, ready-for-prompt handshakes, and prompt-misdelivery
//! detection/recovery all live above raw terminal transport.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    Spawning,
    TrustRequired,
    ReadyForPrompt,
    PromptAccepted,
    Running,
    Finished,
    Failed,
}

impl std::fmt::Display for WorkerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawning => write!(f, "spawning"),
            Self::TrustRequired => write!(f, "trust_required"),
            Self::ReadyForPrompt => write!(f, "ready_for_prompt"),
            Self::PromptAccepted => write!(f, "prompt_accepted"),
            Self::Running => write!(f, "running"),
            Self::Finished => write!(f, "finished"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerFailureKind {
    TrustGate,
    PromptDelivery,
    Protocol,
    Provider,
    LeaseExpired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerFailure {
    pub kind: WorkerFailureKind,
    pub message: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerEventKind {
    Spawning,
    TrustRequired,
    TrustResolved,
    ReadyForPrompt,
    PromptMisdelivery,
    PromptReplayArmed,
    PromptAccepted,
    Running,
    Heartbeat,
    LeaseExpired,
    WorktreeCreated,
    ProcessStarted,
    ProcessExited,
    Restarted,
    Finished,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerTrustResolution {
    AutoAllowlisted,
    ManualApproval,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerPromptTarget {
    Shell,
    WrongTarget,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerEventPayload {
    TrustPrompt {
        cwd: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        resolution: Option<WorkerTrustResolution>,
    },
    PromptDelivery {
        prompt_preview: String,
        observed_target: WorkerPromptTarget,
        #[serde(skip_serializing_if = "Option::is_none")]
        observed_cwd: Option<String>,
        recovery_armed: bool,
    },
    PromptAccepted {
        prompt_preview: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerEvent {
    pub seq: u64,
    pub kind: WorkerEventKind,
    pub status: WorkerStatus,
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<WorkerEventPayload>,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerIsolationKind {
    GitWorktree,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerIsolation {
    pub kind: WorkerIsolationKind,
    pub source_cwd: String,
    pub worktree_path: String,
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerProcess {
    pub pid: u32,
    pub command: Vec<String>,
    pub started_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exited_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Worker {
    pub worker_id: String,
    pub cwd: String,
    pub status: WorkerStatus,
    pub trust_auto_resolve: bool,
    pub trust_gate_cleared: bool,
    pub auto_recover_prompt_misdelivery: bool,
    pub prompt_delivery_attempts: u32,
    pub prompt_in_flight: bool,
    pub last_prompt: Option<String>,
    pub replay_prompt: Option<String>,
    pub last_error: Option<WorkerFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<WorkerProcess>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<WorkerIsolation>,
    #[serde(default)]
    pub heartbeat_at: u64,
    #[serde(default)]
    pub lease_expires_at: u64,
    #[serde(default)]
    pub restart_count: u32,
    #[serde(default = "default_worker_max_restarts")]
    pub max_restarts: u32,
    #[serde(default)]
    pub last_restart_at: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
    pub events: Vec<WorkerEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerCleanupReport {
    pub removed_workers: Vec<Worker>,
    pub retained_workers: usize,
    pub removed_worktrees: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkerRegistrySnapshot {
    version: u32,
    workers: Vec<Worker>,
    counter: u64,
}

const WORKER_REGISTRY_SNAPSHOT_VERSION: u32 = 1;
pub const DEFAULT_WORKER_LEASE_SECS: u64 = 120;
pub const DEFAULT_WORKER_MAX_RESTARTS: u32 = 2;

const fn default_worker_max_restarts() -> u32 {
    DEFAULT_WORKER_MAX_RESTARTS
}

#[derive(Debug, Clone, Default)]
pub struct WorkerRegistry {
    inner: Arc<Mutex<WorkerRegistryInner>>,
}

#[derive(Debug)]
pub struct WorkerProcessHandle {
    pub worker: Worker,
    pub child: Child,
    /// Thread-safe buffer of lines recently read from the child's stdout.
    pub stdout_buffer: Arc<Mutex<Vec<String>>>,
}

impl WorkerProcessHandle {
    /// Write a prompt line to the child's stdin if a piped handle exists.
    pub fn send_prompt(&mut self, prompt: &str) -> io::Result<()> {
        let Some(stdin) = self.child.stdin.as_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "worker process stdin is not available (process may have exited)",
            ));
        };
        stdin.write_all(prompt.as_bytes())?;
        stdin.write_all(b"\n")?;
        stdin.flush()
    }

    /// Drain any available lines from the child's stdout into the buffer.
    pub fn drain_stdout(&mut self) -> io::Result<Vec<String>> {
        let mut new_lines = Vec::new();
        if let Some(stdout) = self.child.stdout.as_mut() {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim_end_matches(['\n', '\r']).to_string();
                        if !trimmed.is_empty() {
                            new_lines.push(trimmed);
                        }
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }
        if !new_lines.is_empty() {
            let mut buf = self
                .stdout_buffer
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            buf.extend(new_lines.clone());
        }
        Ok(new_lines)
    }

    /// Consume the handle and return the Child for final wait/exit-status collection.
    #[must_use]
    pub fn into_child(self) -> Child {
        self.child
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerIsolationSpec {
    GitWorktree { root: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerProcessSpec {
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub trusted_roots: Vec<String>,
    pub auto_recover_prompt_misdelivery: bool,
    pub isolation: Option<WorkerIsolationSpec>,
}

impl WorkerProcessSpec {
    #[must_use]
    pub fn new(command: Vec<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            command,
            cwd: cwd.into(),
            trusted_roots: Vec::new(),
            auto_recover_prompt_misdelivery: true,
            isolation: None,
        }
    }

    #[must_use]
    pub fn with_trusted_roots(mut self, trusted_roots: Vec<String>) -> Self {
        self.trusted_roots = trusted_roots;
        self
    }

    #[must_use]
    pub fn with_isolation(mut self, isolation: WorkerIsolationSpec) -> Self {
        self.isolation = Some(isolation);
        self
    }
}

#[derive(Debug, Default)]
struct WorkerRegistryInner {
    workers: HashMap<String, Worker>,
    counter: u64,
}

impl WorkerRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn create(
        &self,
        cwd: &str,
        trusted_roots: &[String],
        auto_recover_prompt_misdelivery: bool,
    ) -> Worker {
        let mut inner = self.inner.lock().expect("worker registry lock poisoned");
        inner.counter += 1;
        let ts = now_secs();
        let worker_id = format!("worker_{:08x}_{}", ts, inner.counter);
        let trust_auto_resolve = trusted_roots
            .iter()
            .any(|root| path_matches_allowlist(cwd, root));
        let mut worker = Worker {
            worker_id: worker_id.clone(),
            cwd: cwd.to_owned(),
            status: WorkerStatus::Spawning,
            trust_auto_resolve,
            trust_gate_cleared: false,
            auto_recover_prompt_misdelivery,
            prompt_delivery_attempts: 0,
            prompt_in_flight: false,
            last_prompt: None,
            replay_prompt: None,
            last_error: None,
            process: None,
            isolation: None,
            heartbeat_at: ts,
            lease_expires_at: ts + DEFAULT_WORKER_LEASE_SECS,
            restart_count: 0,
            max_restarts: DEFAULT_WORKER_MAX_RESTARTS,
            last_restart_at: None,
            created_at: ts,
            updated_at: ts,
            events: Vec::new(),
        };
        push_event(
            &mut worker,
            WorkerEventKind::Spawning,
            WorkerStatus::Spawning,
            Some("worker created".to_string()),
            None,
        );
        inner.workers.insert(worker_id, worker.clone());
        worker
    }

    pub fn spawn_process(&self, spec: WorkerProcessSpec) -> io::Result<WorkerProcessHandle> {
        if spec.command.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "worker process command cannot be empty",
            ));
        }
        let (effective_cwd, isolation) =
            prepare_worker_isolation(&spec.cwd, spec.isolation.as_ref())?;
        let mut command = Command::new(&spec.command[0]);
        command
            .args(&spec.command[1..])
            .current_dir(&effective_cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                cleanup_worker_isolation(isolation.as_ref());
                return Err(error);
            }
        };
        let pid = child.id();
        let ts = now_secs();
        let mut trusted_roots = spec.trusted_roots.clone();
        if isolation.is_some() {
            trusted_roots.push(effective_cwd.to_string_lossy().to_string());
        }
        let mut worker = self.create(
            &effective_cwd.to_string_lossy(),
            &trusted_roots,
            spec.auto_recover_prompt_misdelivery,
        );
        {
            let mut inner = self.inner.lock().expect("worker registry lock poisoned");
            let stored = inner
                .workers
                .get_mut(&worker.worker_id)
                .expect("created worker should be stored");
            stored.process = Some(WorkerProcess {
                pid,
                command: spec.command.clone(),
                started_at: ts,
                exited_at: None,
                exit_status: None,
            });
            stored.isolation = isolation;
            stored.trust_gate_cleared = true;
            refresh_worker_lease(stored, ts, DEFAULT_WORKER_LEASE_SECS);
            if stored.isolation.is_some() {
                push_event(
                    stored,
                    WorkerEventKind::WorktreeCreated,
                    WorkerStatus::Spawning,
                    Some("worker isolated in git worktree".to_string()),
                    None,
                );
            }
            push_event(
                stored,
                WorkerEventKind::ProcessStarted,
                WorkerStatus::Running,
                Some(format!("worker process started with pid {pid}")),
                None,
            );
            worker = stored.clone();
        }
        Ok(WorkerProcessHandle {
            worker,
            child,
            stdout_buffer: Arc::new(Mutex::new(Vec::new())),
        })
    }

    #[must_use]
    pub fn get(&self, worker_id: &str) -> Option<Worker> {
        let inner = self.inner.lock().expect("worker registry lock poisoned");
        inner.workers.get(worker_id).cloned()
    }

    pub fn observe_process(&self, worker_id: &str, child: &mut Child) -> io::Result<Worker> {
        match child.try_wait()? {
            Some(status) => self
                .record_process_exit(worker_id, status.code())
                .map_err(io::Error::other),
            None => self
                .heartbeat(worker_id, DEFAULT_WORKER_LEASE_SECS)
                .map_err(io::Error::other),
        }
    }

    pub fn probe_process(&self, worker_id: &str) -> Result<Worker, String> {
        let mut inner = self.inner.lock().expect("worker registry lock poisoned");
        let worker = inner
            .workers
            .get_mut(worker_id)
            .ok_or_else(|| format!("worker not found: {worker_id}"))?;
        let Some(process) = worker.process.as_ref() else {
            return Err(format!("worker {worker_id} has no process backend"));
        };
        if process.exited_at.is_some() {
            return Ok(worker.clone());
        }
        let pid = process.pid;
        if process_is_alive(pid) {
            refresh_worker_lease(worker, now_secs(), DEFAULT_WORKER_LEASE_SECS);
            push_event(
                worker,
                WorkerEventKind::Heartbeat,
                worker.status,
                Some(format!("worker process {pid} is alive")),
                None,
            );
            return Ok(worker.clone());
        }
        if let Some(process) = worker.process.as_mut() {
            process.exited_at = Some(now_secs());
        }
        worker.status = WorkerStatus::Finished;
        worker.prompt_in_flight = false;
        push_event(
            worker,
            WorkerEventKind::ProcessExited,
            WorkerStatus::Finished,
            Some("worker process is no longer running".to_string()),
            None,
        );
        Ok(worker.clone())
    }

    pub fn record_process_exit(
        &self,
        worker_id: &str,
        exit_status: Option<i32>,
    ) -> Result<Worker, String> {
        let mut inner = self.inner.lock().expect("worker registry lock poisoned");
        let worker = inner
            .workers
            .get_mut(worker_id)
            .ok_or_else(|| format!("worker not found: {worker_id}"))?;
        let Some(process) = worker.process.as_mut() else {
            return Err(format!("worker {worker_id} has no process backend"));
        };
        let ts = now_secs();
        process.exited_at = Some(ts);
        process.exit_status = exit_status;
        worker.prompt_in_flight = false;
        if exit_status == Some(0) {
            worker.status = WorkerStatus::Finished;
            worker.last_error = None;
            push_event(
                worker,
                WorkerEventKind::ProcessExited,
                WorkerStatus::Finished,
                Some("worker process exited successfully".to_string()),
                None,
            );
        } else {
            worker.status = WorkerStatus::Failed;
            worker.last_error = Some(WorkerFailure {
                kind: WorkerFailureKind::Provider,
                message: format!("worker process exited with status {exit_status:?}"),
                created_at: ts,
            });
            push_event(
                worker,
                WorkerEventKind::ProcessExited,
                WorkerStatus::Failed,
                Some("worker process exited unsuccessfully".to_string()),
                None,
            );
        }
        Ok(worker.clone())
    }

    #[must_use]
    pub fn list(&self) -> Vec<Worker> {
        let inner = self.inner.lock().expect("worker registry lock poisoned");
        let mut workers = inner.workers.values().cloned().collect::<Vec<_>>();
        workers.sort_by_key(|worker| (worker.created_at, worker.worker_id.clone()));
        workers
    }

    #[must_use]
    pub fn cleanup_finished(&self) -> WorkerCleanupReport {
        self.cleanup(false, now_secs())
    }

    #[must_use]
    pub fn cleanup_stale(&self, now: u64) -> WorkerCleanupReport {
        self.cleanup(true, now)
    }

    fn cleanup(&self, include_stale: bool, now: u64) -> WorkerCleanupReport {
        let mut inner = self.inner.lock().expect("worker registry lock poisoned");
        let mut removed_workers = Vec::new();
        let mut removed_worktrees = Vec::new();
        let remove_ids = inner
            .workers
            .iter()
            .filter_map(|(worker_id, worker)| {
                if should_cleanup_worker(worker, include_stale, now) {
                    Some(worker_id.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        for worker_id in remove_ids {
            if let Some(worker) = inner.workers.remove(&worker_id) {
                if let Some(isolation) = worker.isolation.as_ref() {
                    cleanup_worker_isolation(Some(isolation));
                    removed_worktrees.push(isolation.worktree_path.clone());
                }
                removed_workers.push(worker);
            }
        }

        removed_workers.sort_by_key(|worker| (worker.created_at, worker.worker_id.clone()));
        removed_worktrees.sort();
        WorkerCleanupReport {
            removed_workers,
            retained_workers: inner.workers.len(),
            removed_worktrees,
        }
    }

    pub fn save_to_dir(&self, dir: &Path) -> io::Result<()> {
        let inner = self.inner.lock().expect("worker registry lock poisoned");
        fs::create_dir_all(dir)?;
        let snapshot = WorkerRegistrySnapshot {
            version: WORKER_REGISTRY_SNAPSHOT_VERSION,
            workers: inner.workers.values().cloned().collect(),
            counter: inner.counter,
        };
        let json = serde_json::to_string_pretty(&snapshot)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        fs::write(dir.join("workers.json"), format!("{json}\n"))
    }

    pub fn load_from_dir(dir: &Path) -> io::Result<Self> {
        let contents = fs::read_to_string(dir.join("workers.json"))?;
        let snapshot = serde_json::from_str::<WorkerRegistrySnapshot>(&contents)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let workers = snapshot
            .workers
            .into_iter()
            .map(|worker| {
                let worker = normalize_loaded_worker(worker);
                (worker.worker_id.clone(), worker)
            })
            .collect();
        Ok(Self {
            inner: Arc::new(Mutex::new(WorkerRegistryInner {
                workers,
                counter: snapshot.counter,
            })),
        })
    }

    pub fn observe(&self, worker_id: &str, screen_text: &str) -> Result<Worker, String> {
        let mut inner = self.inner.lock().expect("worker registry lock poisoned");
        let worker = inner
            .workers
            .get_mut(worker_id)
            .ok_or_else(|| format!("worker not found: {worker_id}"))?;
        refresh_worker_lease(worker, now_secs(), DEFAULT_WORKER_LEASE_SECS);
        let lowered = screen_text.to_ascii_lowercase();

        if !worker.trust_gate_cleared && detect_trust_prompt(&lowered) {
            worker.status = WorkerStatus::TrustRequired;
            worker.last_error = Some(WorkerFailure {
                kind: WorkerFailureKind::TrustGate,
                message: "worker boot blocked on trust prompt".to_string(),
                created_at: now_secs(),
            });
            push_event(
                worker,
                WorkerEventKind::TrustRequired,
                WorkerStatus::TrustRequired,
                Some("trust prompt detected".to_string()),
                Some(WorkerEventPayload::TrustPrompt {
                    cwd: worker.cwd.clone(),
                    resolution: None,
                }),
            );

            if worker.trust_auto_resolve {
                worker.trust_gate_cleared = true;
                worker.last_error = None;
                worker.status = WorkerStatus::Spawning;
                push_event(
                    worker,
                    WorkerEventKind::TrustResolved,
                    WorkerStatus::Spawning,
                    Some("allowlisted repo auto-resolved trust prompt".to_string()),
                    Some(WorkerEventPayload::TrustPrompt {
                        cwd: worker.cwd.clone(),
                        resolution: Some(WorkerTrustResolution::AutoAllowlisted),
                    }),
                );
            } else {
                return Ok(worker.clone());
            }
        }

        if let Some(observation) = prompt_misdelivery_is_relevant(worker)
            .then(|| {
                detect_prompt_misdelivery(
                    screen_text,
                    &lowered,
                    worker.last_prompt.as_deref(),
                    &worker.cwd,
                )
            })
            .flatten()
        {
            let prompt_preview = prompt_preview(worker.last_prompt.as_deref().unwrap_or_default());
            let message = match observation.target {
                WorkerPromptTarget::Shell => {
                    format!(
                        "worker prompt landed in shell instead of coding agent: {prompt_preview}"
                    )
                }
                WorkerPromptTarget::WrongTarget => format!(
                    "worker prompt landed in the wrong target instead of {}: {}",
                    worker.cwd, prompt_preview
                ),
                WorkerPromptTarget::Unknown => format!(
                    "worker prompt delivery failed before reaching coding agent: {prompt_preview}"
                ),
            };
            worker.last_error = Some(WorkerFailure {
                kind: WorkerFailureKind::PromptDelivery,
                message,
                created_at: now_secs(),
            });
            worker.prompt_in_flight = false;
            push_event(
                worker,
                WorkerEventKind::PromptMisdelivery,
                WorkerStatus::Failed,
                Some(prompt_misdelivery_detail(&observation).to_string()),
                Some(WorkerEventPayload::PromptDelivery {
                    prompt_preview: prompt_preview.clone(),
                    observed_target: observation.target,
                    observed_cwd: observation.observed_cwd.clone(),
                    recovery_armed: false,
                }),
            );
            if worker.auto_recover_prompt_misdelivery {
                worker.replay_prompt = worker.last_prompt.clone();
                worker.status = WorkerStatus::ReadyForPrompt;
                push_event(
                    worker,
                    WorkerEventKind::PromptReplayArmed,
                    WorkerStatus::ReadyForPrompt,
                    Some("prompt replay armed after prompt misdelivery".to_string()),
                    Some(WorkerEventPayload::PromptDelivery {
                        prompt_preview,
                        observed_target: observation.target,
                        observed_cwd: observation.observed_cwd,
                        recovery_armed: true,
                    }),
                );
            } else {
                worker.status = WorkerStatus::Failed;
            }
            return Ok(worker.clone());
        }

        if detect_running_cue(&lowered) && worker.prompt_in_flight {
            worker.prompt_in_flight = false;
            worker.status = WorkerStatus::Running;
            worker.last_error = None;
            push_event(
                worker,
                WorkerEventKind::Running,
                WorkerStatus::Running,
                Some("worker started processing accepted prompt".to_string()),
                None,
            );
        }

        if detect_ready_for_prompt(screen_text, &lowered)
            && worker.status != WorkerStatus::ReadyForPrompt
        {
            worker.status = WorkerStatus::ReadyForPrompt;
            worker.prompt_in_flight = false;
            if matches!(
                worker.last_error.as_ref().map(|failure| failure.kind),
                Some(WorkerFailureKind::TrustGate)
            ) {
                worker.last_error = None;
            }
            push_event(
                worker,
                WorkerEventKind::ReadyForPrompt,
                WorkerStatus::ReadyForPrompt,
                Some("worker is ready for prompt delivery".to_string()),
                None,
            );
        }

        Ok(worker.clone())
    }

    pub fn heartbeat(&self, worker_id: &str, lease_secs: u64) -> Result<Worker, String> {
        let mut inner = self.inner.lock().expect("worker registry lock poisoned");
        let worker = inner
            .workers
            .get_mut(worker_id)
            .ok_or_else(|| format!("worker not found: {worker_id}"))?;
        if is_terminal(worker.status) {
            return Err(format!(
                "worker {worker_id} cannot heartbeat after terminal status: {}",
                worker.status
            ));
        }
        refresh_worker_lease(worker, now_secs(), lease_secs);
        push_event(
            worker,
            WorkerEventKind::Heartbeat,
            worker.status,
            Some(format!("worker heartbeat; lease extended by {lease_secs}s")),
            None,
        );
        Ok(worker.clone())
    }

    #[must_use]
    pub fn restart_stale_workers(&self, now: u64, lease_secs: u64) -> Vec<Worker> {
        let mut inner = self.inner.lock().expect("worker registry lock poisoned");
        inner
            .workers
            .values_mut()
            .filter_map(|worker| restart_stale_worker(worker, now, lease_secs))
            .collect()
    }

    #[must_use]
    pub fn restart_stale_workers_now(&self, lease_secs: u64) -> Vec<Worker> {
        self.restart_stale_workers(now_secs(), lease_secs)
    }

    #[cfg(test)]
    pub(crate) fn expire_lease_for_test(&self, worker_id: &str) -> Result<Worker, String> {
        let mut inner = self.inner.lock().expect("worker registry lock poisoned");
        let worker = inner
            .workers
            .get_mut(worker_id)
            .ok_or_else(|| format!("worker not found: {worker_id}"))?;
        let now = now_secs();
        worker.heartbeat_at = now.saturating_sub(DEFAULT_WORKER_LEASE_SECS + 1);
        worker.lease_expires_at = now.saturating_sub(1);
        Ok(worker.clone())
    }

    pub fn resolve_trust(&self, worker_id: &str) -> Result<Worker, String> {
        let mut inner = self.inner.lock().expect("worker registry lock poisoned");
        let worker = inner
            .workers
            .get_mut(worker_id)
            .ok_or_else(|| format!("worker not found: {worker_id}"))?;

        if worker.status != WorkerStatus::TrustRequired {
            return Err(format!(
                "worker {worker_id} is not waiting on trust; current status: {}",
                worker.status
            ));
        }

        worker.trust_gate_cleared = true;
        worker.last_error = None;
        worker.status = WorkerStatus::Spawning;
        push_event(
            worker,
            WorkerEventKind::TrustResolved,
            WorkerStatus::Spawning,
            Some("trust prompt resolved manually".to_string()),
            Some(WorkerEventPayload::TrustPrompt {
                cwd: worker.cwd.clone(),
                resolution: Some(WorkerTrustResolution::ManualApproval),
            }),
        );
        Ok(worker.clone())
    }

    pub fn send_prompt(&self, worker_id: &str, prompt: Option<&str>) -> Result<Worker, String> {
        let mut inner = self.inner.lock().expect("worker registry lock poisoned");
        let worker = inner
            .workers
            .get_mut(worker_id)
            .ok_or_else(|| format!("worker not found: {worker_id}"))?;

        if worker.status != WorkerStatus::ReadyForPrompt {
            return Err(format!(
                "worker {worker_id} is not ready for prompt delivery; current status: {}",
                worker.status
            ));
        }

        let next_prompt = prompt
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or_else(|| worker.replay_prompt.clone())
            .ok_or_else(|| format!("worker {worker_id} has no prompt to send or replay"))?;

        refresh_worker_lease(worker, now_secs(), DEFAULT_WORKER_LEASE_SECS);
        worker.prompt_delivery_attempts += 1;
        worker.prompt_in_flight = true;
        worker.last_prompt = Some(next_prompt.clone());
        worker.replay_prompt = None;
        worker.last_error = None;
        let prompt_preview = prompt_preview(&next_prompt);
        worker.status = WorkerStatus::PromptAccepted;
        push_event(
            worker,
            WorkerEventKind::PromptAccepted,
            WorkerStatus::PromptAccepted,
            Some(format!("prompt accepted by worker: {prompt_preview}")),
            Some(WorkerEventPayload::PromptAccepted { prompt_preview }),
        );
        Ok(worker.clone())
    }

    pub fn await_ready(&self, worker_id: &str) -> Result<WorkerReadySnapshot, String> {
        let worker = self
            .get(worker_id)
            .ok_or_else(|| format!("worker not found: {worker_id}"))?;

        Ok(WorkerReadySnapshot {
            worker_id: worker.worker_id.clone(),
            status: worker.status,
            ready: worker.status == WorkerStatus::ReadyForPrompt,
            blocked: matches!(
                worker.status,
                WorkerStatus::TrustRequired | WorkerStatus::Failed
            ),
            replay_prompt_ready: worker.replay_prompt.is_some(),
            last_error: worker.last_error.clone(),
        })
    }

    pub fn restart(&self, worker_id: &str) -> Result<Worker, String> {
        let mut inner = self.inner.lock().expect("worker registry lock poisoned");
        let worker = inner
            .workers
            .get_mut(worker_id)
            .ok_or_else(|| format!("worker not found: {worker_id}"))?;
        worker.status = WorkerStatus::Spawning;
        worker.trust_gate_cleared = false;
        worker.last_prompt = None;
        worker.replay_prompt = None;
        worker.last_error = None;
        worker.prompt_delivery_attempts = 0;
        worker.prompt_in_flight = false;
        push_event(
            worker,
            WorkerEventKind::Restarted,
            WorkerStatus::Spawning,
            Some("worker restarted".to_string()),
            None,
        );
        Ok(worker.clone())
    }

    pub fn terminate(&self, worker_id: &str) -> Result<Worker, String> {
        let mut inner = self.inner.lock().expect("worker registry lock poisoned");
        let worker = inner
            .workers
            .get_mut(worker_id)
            .ok_or_else(|| format!("worker not found: {worker_id}"))?;
        worker.status = WorkerStatus::Finished;
        worker.prompt_in_flight = false;
        push_event(
            worker,
            WorkerEventKind::Finished,
            WorkerStatus::Finished,
            Some("worker terminated by control plane".to_string()),
            None,
        );
        Ok(worker.clone())
    }

    /// Classify session completion and transition worker to appropriate terminal state.
    /// Detects degraded completions (finish="unknown" with zero tokens) as provider failures.
    pub fn observe_completion(
        &self,
        worker_id: &str,
        finish_reason: &str,
        tokens_output: u64,
    ) -> Result<Worker, String> {
        let mut inner = self.inner.lock().expect("worker registry lock poisoned");
        let worker = inner
            .workers
            .get_mut(worker_id)
            .ok_or_else(|| format!("worker not found: {worker_id}"))?;

        let is_provider_failure =
            (finish_reason == "unknown" && tokens_output == 0) || finish_reason == "error";

        if is_provider_failure {
            let message = if finish_reason == "unknown" && tokens_output == 0 {
                "session completed with finish='unknown' and zero output — provider degraded or context exhausted".to_string()
            } else {
                format!("session failed with finish='{finish_reason}' — provider error")
            };

            worker.last_error = Some(WorkerFailure {
                kind: WorkerFailureKind::Provider,
                message,
                created_at: now_secs(),
            });
            worker.status = WorkerStatus::Failed;
            worker.prompt_in_flight = false;
            push_event(
                worker,
                WorkerEventKind::Failed,
                WorkerStatus::Failed,
                Some("provider failure classified".to_string()),
                None,
            );
        } else {
            worker.status = WorkerStatus::Finished;
            worker.prompt_in_flight = false;
            worker.last_error = None;
            push_event(
                worker,
                WorkerEventKind::Finished,
                WorkerStatus::Finished,
                Some(format!(
                    "session completed: finish='{finish_reason}', tokens={tokens_output}"
                )),
                None,
            );
        }

        Ok(worker.clone())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerReadySnapshot {
    pub worker_id: String,
    pub status: WorkerStatus,
    pub ready: bool,
    pub blocked: bool,
    pub replay_prompt_ready: bool,
    pub last_error: Option<WorkerFailure>,
}

fn prepare_worker_isolation(
    source_cwd: &Path,
    isolation: Option<&WorkerIsolationSpec>,
) -> io::Result<(PathBuf, Option<WorkerIsolation>)> {
    match isolation {
        None => Ok((source_cwd.to_path_buf(), None)),
        Some(WorkerIsolationSpec::GitWorktree { root }) => {
            create_git_worktree_isolation(source_cwd, root)
        }
    }
}

fn create_git_worktree_isolation(
    source_cwd: &Path,
    root: &Path,
) -> io::Result<(PathBuf, Option<WorkerIsolation>)> {
    let check = Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(source_cwd)
        .output()?;
    if !check.status.success() || String::from_utf8_lossy(&check.stdout).trim() != "true" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker worktree isolation requires a git work tree",
        ));
    }

    let root = if root.is_absolute() {
        root.to_path_buf()
    } else {
        source_cwd.join(root)
    };
    fs::create_dir_all(&root)?;
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let worktree_path = root.join(format!("worker-worktree-{}-{suffix}", std::process::id()));
    let output = Command::new("git")
        .args([
            "worktree",
            "add",
            "--detach",
            worktree_path.to_string_lossy().as_ref(),
            "HEAD",
        ])
        .current_dir(source_cwd)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(io::Error::other(format!(
            "failed to create worker git worktree: {stderr}"
        )));
    }

    Ok((
        worktree_path.clone(),
        Some(WorkerIsolation {
            kind: WorkerIsolationKind::GitWorktree,
            source_cwd: source_cwd.to_string_lossy().to_string(),
            worktree_path: worktree_path.to_string_lossy().to_string(),
            created_at: now_secs(),
        }),
    ))
}

fn cleanup_worker_isolation(isolation: Option<&WorkerIsolation>) {
    let Some(isolation) = isolation else {
        return;
    };
    match isolation.kind {
        WorkerIsolationKind::GitWorktree => {
            let _ = Command::new("git")
                .args(["worktree", "remove", "--force", &isolation.worktree_path])
                .current_dir(&isolation.source_cwd)
                .output();
        }
    }
}

fn normalize_loaded_worker(mut worker: Worker) -> Worker {
    if worker.heartbeat_at == 0 {
        worker.heartbeat_at = worker.updated_at.max(worker.created_at);
    }
    if worker.lease_expires_at == 0 {
        worker.lease_expires_at = worker.heartbeat_at + DEFAULT_WORKER_LEASE_SECS;
    }
    if worker.max_restarts == 0 {
        worker.max_restarts = DEFAULT_WORKER_MAX_RESTARTS;
    }
    worker
}

fn refresh_worker_lease(worker: &mut Worker, now: u64, lease_secs: u64) {
    worker.heartbeat_at = now;
    worker.lease_expires_at = now + lease_secs;
}

fn is_terminal(status: WorkerStatus) -> bool {
    matches!(status, WorkerStatus::Finished | WorkerStatus::Failed)
}

fn should_cleanup_worker(worker: &Worker, include_stale: bool, now: u64) -> bool {
    is_terminal(worker.status)
        || (include_stale
            && worker.status != WorkerStatus::TrustRequired
            && worker.lease_expires_at > 0
            && now > worker.lease_expires_at
            && !worker_has_live_process(worker))
}

fn worker_has_live_process(worker: &Worker) -> bool {
    worker
        .process
        .as_ref()
        .is_some_and(|process| process.exited_at.is_none() && process_is_alive(process.pid))
}

fn restart_stale_worker(worker: &mut Worker, now: u64, lease_secs: u64) -> Option<Worker> {
    if is_terminal(worker.status) || worker.status == WorkerStatus::TrustRequired {
        return None;
    }
    if worker.lease_expires_at == 0 || now <= worker.lease_expires_at {
        return None;
    }

    let stale_message = format!(
        "worker lease expired at {}; last heartbeat at {}",
        worker.lease_expires_at, worker.heartbeat_at
    );
    worker.last_error = Some(WorkerFailure {
        kind: WorkerFailureKind::LeaseExpired,
        message: stale_message.clone(),
        created_at: now,
    });
    worker.prompt_in_flight = false;

    if worker.restart_count >= worker.max_restarts {
        push_event(
            worker,
            WorkerEventKind::LeaseExpired,
            WorkerStatus::Failed,
            Some(format!("{stale_message}; restart budget exhausted")),
            None,
        );
        return Some(worker.clone());
    }

    worker.last_error = None;
    worker.restart_count += 1;
    worker.last_restart_at = Some(now);
    worker.replay_prompt = worker
        .last_prompt
        .clone()
        .or_else(|| worker.replay_prompt.clone());
    worker.status = WorkerStatus::Spawning;
    worker.trust_gate_cleared = false;
    worker.prompt_delivery_attempts = 0;
    refresh_worker_lease(worker, now, lease_secs);
    push_event(
        worker,
        WorkerEventKind::LeaseExpired,
        WorkerStatus::Failed,
        Some(stale_message),
        None,
    );
    push_event(
        worker,
        WorkerEventKind::Restarted,
        WorkerStatus::Spawning,
        Some(format!(
            "stale worker restarted (attempt {})",
            worker.restart_count
        )),
        None,
    );
    Some(worker.clone())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PromptDeliveryObservation {
    target: WorkerPromptTarget,
    observed_cwd: Option<String>,
}

fn push_event(
    worker: &mut Worker,
    kind: WorkerEventKind,
    status: WorkerStatus,
    detail: Option<String>,
    payload: Option<WorkerEventPayload>,
) {
    let timestamp = now_secs();
    let seq = worker.events.len() as u64 + 1;
    worker.updated_at = timestamp;
    worker.status = status;
    worker.events.push(WorkerEvent {
        seq,
        kind,
        status,
        detail,
        payload,
        timestamp,
    });
    emit_state_file(worker);
}

/// Write current worker state to `.Himalaya/worker-state.json` under the worker's cwd.
/// This is the file-based observability surface: external observers (Himalayahip, orchestrators)
/// poll this file instead of requiring an HTTP route on the opencode binary.
fn emit_state_file(worker: &Worker) {
    let state_dir = std::path::Path::new(&worker.cwd).join(".Himalaya");
    if std::fs::create_dir_all(&state_dir).is_err() {
        return;
    }
    let state_path = state_dir.join("worker-state.json");
    let tmp_path = state_dir.join("worker-state.json.tmp");

    #[derive(serde::Serialize)]
    struct StateSnapshot<'a> {
        worker_id: &'a str,
        status: WorkerStatus,
        is_ready: bool,
        trust_gate_cleared: bool,
        prompt_in_flight: bool,
        last_event: Option<&'a WorkerEvent>,
        heartbeat_at: u64,
        lease_expires_at: u64,
        restart_count: u32,
        max_restarts: u32,
        last_restart_at: Option<u64>,
        process: Option<&'a WorkerProcess>,
        isolation: Option<&'a WorkerIsolation>,
        updated_at: u64,
        /// Seconds since last state transition. Himalayahip uses this to detect
        /// stalled workers without computing epoch deltas.
        seconds_since_update: u64,
    }

    let now = now_secs();
    let snapshot = StateSnapshot {
        worker_id: &worker.worker_id,
        status: worker.status,
        is_ready: worker.status == WorkerStatus::ReadyForPrompt,
        trust_gate_cleared: worker.trust_gate_cleared,
        prompt_in_flight: worker.prompt_in_flight,
        last_event: worker.events.last(),
        heartbeat_at: worker.heartbeat_at,
        lease_expires_at: worker.lease_expires_at,
        restart_count: worker.restart_count,
        max_restarts: worker.max_restarts,
        last_restart_at: worker.last_restart_at,
        process: worker.process.as_ref(),
        isolation: worker.isolation.as_ref(),
        updated_at: worker.updated_at,
        seconds_since_update: now.saturating_sub(worker.updated_at),
    };

    if let Ok(json) = serde_json::to_string_pretty(&snapshot) {
        let _ = std::fs::write(&tmp_path, json);
        let _ = std::fs::rename(&tmp_path, &state_path);
    }
}

fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    process_is_alive_impl(pid)
}

#[cfg(unix)]
fn process_is_alive_impl(pid: u32) -> bool {
    std::path::Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(not(unix))]
fn process_is_alive_impl(_pid: u32) -> bool {
    true
}

fn path_matches_allowlist(cwd: &str, trusted_root: &str) -> bool {
    let cwd = normalize_path(cwd);
    let trusted_root = normalize_path(trusted_root);
    cwd == trusted_root || cwd.starts_with(&trusted_root)
}

fn normalize_path(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| Path::new(path).to_path_buf())
}

fn detect_trust_prompt(lowered: &str) -> bool {
    [
        "do you trust the files in this folder",
        "trust the files in this folder",
        "trust this folder",
        "allow and continue",
        "yes, proceed",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
}

fn detect_ready_for_prompt(screen_text: &str, lowered: &str) -> bool {
    if [
        "ready for input",
        "ready for your input",
        "ready for prompt",
        "send a message",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
    {
        return true;
    }

    let Some(last_non_empty) = screen_text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
    else {
        return false;
    };
    let trimmed = last_non_empty.trim();
    if is_shell_prompt(trimmed) {
        return false;
    }

    trimmed == ">"
        || trimmed == "›"
        || trimmed == "❯"
        || trimmed.starts_with("> ")
        || trimmed.starts_with("› ")
        || trimmed.starts_with("❯ ")
        || trimmed.contains("│ >")
        || trimmed.contains("│ ›")
        || trimmed.contains("│ ❯")
}

fn detect_running_cue(lowered: &str) -> bool {
    [
        "thinking",
        "working",
        "running tests",
        "inspecting",
        "analyzing",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
}

fn is_shell_prompt(trimmed: &str) -> bool {
    trimmed.ends_with('$')
        || trimmed.ends_with('%')
        || trimmed.ends_with('#')
        || trimmed.starts_with('$')
        || trimmed.starts_with('%')
        || trimmed.starts_with('#')
}

fn detect_prompt_misdelivery(
    screen_text: &str,
    lowered: &str,
    prompt: Option<&str>,
    expected_cwd: &str,
) -> Option<PromptDeliveryObservation> {
    let Some(prompt) = prompt else {
        return None;
    };

    let prompt_snippet = prompt
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_ascii_lowercase())
        .unwrap_or_default();
    if prompt_snippet.is_empty() {
        return None;
    }
    let prompt_visible = lowered.contains(&prompt_snippet);

    if let Some(observed_cwd) = detect_observed_shell_cwd(screen_text) {
        if prompt_visible && !cwd_matches_observed_target(expected_cwd, &observed_cwd) {
            return Some(PromptDeliveryObservation {
                target: WorkerPromptTarget::WrongTarget,
                observed_cwd: Some(observed_cwd),
            });
        }
    }

    let shell_error = [
        "command not found",
        "syntax error near unexpected token",
        "parse error near",
        "no such file or directory",
        "unknown command",
    ]
    .iter()
    .any(|needle| lowered.contains(needle));

    (shell_error && prompt_visible).then_some(PromptDeliveryObservation {
        target: WorkerPromptTarget::Shell,
        observed_cwd: None,
    })
}

fn prompt_misdelivery_is_relevant(worker: &Worker) -> bool {
    worker.prompt_in_flight && worker.last_prompt.is_some()
}

fn prompt_preview(prompt: &str) -> String {
    let trimmed = prompt.trim();
    if trimmed.chars().count() <= 48 {
        return trimmed.to_string();
    }
    let preview = trimmed.chars().take(48).collect::<String>();
    format!("{}…", preview.trim_end())
}

fn prompt_misdelivery_detail(observation: &PromptDeliveryObservation) -> &'static str {
    match observation.target {
        WorkerPromptTarget::Shell => "shell misdelivery detected",
        WorkerPromptTarget::WrongTarget => "prompt landed in wrong target",
        WorkerPromptTarget::Unknown => "prompt delivery failure detected",
    }
}

fn detect_observed_shell_cwd(screen_text: &str) -> Option<String> {
    screen_text.lines().find_map(|line| {
        let tokens = line.split_whitespace().collect::<Vec<_>>();
        tokens
            .iter()
            .position(|token| is_shell_prompt_token(token))
            .and_then(|index| index.checked_sub(1).map(|cwd_index| tokens[cwd_index]))
            .filter(|candidate| looks_like_cwd_label(candidate))
            .map(ToOwned::to_owned)
    })
}

fn is_shell_prompt_token(token: &&str) -> bool {
    matches!(*token, "$" | "%" | "#" | ">" | "›" | "❯")
}

fn looks_like_cwd_label(candidate: &str) -> bool {
    candidate.starts_with('/')
        || candidate.starts_with('~')
        || candidate.starts_with('.')
        || candidate.contains('/')
}

fn cwd_matches_observed_target(expected_cwd: &str, observed_cwd: &str) -> bool {
    let expected = normalize_path(expected_cwd);
    let expected_base = expected
        .file_name()
        .map(|segment| segment.to_string_lossy().into_owned())
        .unwrap_or_else(|| expected.to_string_lossy().into_owned());
    let observed_base = Path::new(observed_cwd)
        .file_name()
        .map(|segment| segment.to_string_lossy().into_owned())
        .unwrap_or_else(|| observed_cwd.trim_matches(':').to_string());

    expected.to_string_lossy().ends_with(observed_cwd)
        || observed_cwd.ends_with(expected.to_string_lossy().as_ref())
        || expected_base == observed_base
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "runtime-worker-{}-{}-{nanos}",
            label,
            std::process::id()
        ))
    }

    fn git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git should run");
        assert!(
            output.status.success(),
            "git {:?} failed\nstdout:\n{}\nstderr:\n{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn allowlisted_trust_prompt_auto_resolves_then_reaches_ready_state() {
        let registry = WorkerRegistry::new();
        let worker = registry.create(
            "/tmp/worktrees/repo-a",
            &["/tmp/worktrees".to_string()],
            true,
        );

        let after_trust = registry
            .observe(
                &worker.worker_id,
                "Do you trust the files in this folder?\n1. Yes, proceed\n2. No",
            )
            .expect("trust observe should succeed");
        assert_eq!(after_trust.status, WorkerStatus::Spawning);
        assert!(after_trust.trust_gate_cleared);
        let trust_required = after_trust
            .events
            .iter()
            .find(|event| event.kind == WorkerEventKind::TrustRequired)
            .expect("trust required event should exist");
        assert_eq!(
            trust_required.payload,
            Some(WorkerEventPayload::TrustPrompt {
                cwd: "/tmp/worktrees/repo-a".to_string(),
                resolution: None,
            })
        );
        let trust_resolved = after_trust
            .events
            .iter()
            .find(|event| event.kind == WorkerEventKind::TrustResolved)
            .expect("trust resolved event should exist");
        assert_eq!(
            trust_resolved.payload,
            Some(WorkerEventPayload::TrustPrompt {
                cwd: "/tmp/worktrees/repo-a".to_string(),
                resolution: Some(WorkerTrustResolution::AutoAllowlisted),
            })
        );

        let ready = registry
            .observe(&worker.worker_id, "Ready for your input\n>")
            .expect("ready observe should succeed");
        assert_eq!(ready.status, WorkerStatus::ReadyForPrompt);
        assert!(ready.last_error.is_none());
    }

    #[test]
    fn trust_prompt_blocks_non_allowlisted_worker_until_resolved() {
        let registry = WorkerRegistry::new();
        let worker = registry.create("/tmp/repo-b", &[], true);

        let blocked = registry
            .observe(
                &worker.worker_id,
                "Do you trust the files in this folder?\n1. Yes, proceed\n2. No",
            )
            .expect("trust observe should succeed");
        assert_eq!(blocked.status, WorkerStatus::TrustRequired);
        assert_eq!(
            blocked.last_error.expect("trust error should exist").kind,
            WorkerFailureKind::TrustGate
        );

        let send_before_resolve = registry.send_prompt(&worker.worker_id, Some("ship it"));
        assert!(send_before_resolve
            .expect_err("prompt delivery should be gated")
            .contains("not ready for prompt delivery"));

        let resolved = registry
            .resolve_trust(&worker.worker_id)
            .expect("manual trust resolution should succeed");
        assert_eq!(resolved.status, WorkerStatus::Spawning);
        assert!(resolved.trust_gate_cleared);
        let trust_resolved = resolved
            .events
            .iter()
            .find(|event| event.kind == WorkerEventKind::TrustResolved)
            .expect("manual trust resolve event should exist");
        assert_eq!(
            trust_resolved.payload,
            Some(WorkerEventPayload::TrustPrompt {
                cwd: "/tmp/repo-b".to_string(),
                resolution: Some(WorkerTrustResolution::ManualApproval),
            })
        );
    }

    #[test]
    fn ready_detection_ignores_plain_shell_prompts() {
        assert!(!detect_ready_for_prompt("bellman@host %", "bellman@host %"));
        assert!(!detect_ready_for_prompt("/tmp/repo $", "/tmp/repo $"));
        assert!(detect_ready_for_prompt("│ >", "│ >"));
    }

    #[test]
    fn prompt_misdelivery_is_detected_and_replay_can_be_rearmed() {
        let registry = WorkerRegistry::new();
        let worker = registry.create("/tmp/repo-c", &[], true);
        registry
            .observe(&worker.worker_id, "Ready for input\n>")
            .expect("ready observe should succeed");

        let accepted = registry
            .send_prompt(&worker.worker_id, Some("Implement worker handshake"))
            .expect("prompt send should succeed");
        assert_eq!(accepted.status, WorkerStatus::PromptAccepted);
        assert_eq!(accepted.prompt_delivery_attempts, 1);
        assert!(accepted.prompt_in_flight);
        assert!(accepted
            .events
            .iter()
            .any(|event| event.kind == WorkerEventKind::PromptAccepted));

        let recovered = registry
            .observe(
                &worker.worker_id,
                "% Implement worker handshake\nzsh: command not found: Implement",
            )
            .expect("misdelivery observe should succeed");
        assert_eq!(recovered.status, WorkerStatus::ReadyForPrompt);
        assert_eq!(
            recovered
                .last_error
                .expect("misdelivery error should exist")
                .kind,
            WorkerFailureKind::PromptDelivery
        );
        assert_eq!(
            recovered.replay_prompt.as_deref(),
            Some("Implement worker handshake")
        );
        let misdelivery = recovered
            .events
            .iter()
            .find(|event| event.kind == WorkerEventKind::PromptMisdelivery)
            .expect("misdelivery event should exist");
        assert_eq!(misdelivery.status, WorkerStatus::Failed);
        assert_eq!(
            misdelivery.payload,
            Some(WorkerEventPayload::PromptDelivery {
                prompt_preview: "Implement worker handshake".to_string(),
                observed_target: WorkerPromptTarget::Shell,
                observed_cwd: None,
                recovery_armed: false,
            })
        );
        let replay = recovered
            .events
            .iter()
            .find(|event| event.kind == WorkerEventKind::PromptReplayArmed)
            .expect("replay event should exist");
        assert_eq!(replay.status, WorkerStatus::ReadyForPrompt);
        assert_eq!(
            replay.payload,
            Some(WorkerEventPayload::PromptDelivery {
                prompt_preview: "Implement worker handshake".to_string(),
                observed_target: WorkerPromptTarget::Shell,
                observed_cwd: None,
                recovery_armed: true,
            })
        );

        let replayed = registry
            .send_prompt(&worker.worker_id, None)
            .expect("replay send should succeed");
        assert_eq!(replayed.status, WorkerStatus::PromptAccepted);
        assert!(replayed.replay_prompt.is_none());
        assert_eq!(replayed.prompt_delivery_attempts, 2);
        let running = registry
            .observe(&worker.worker_id, "Thinking about tests")
            .expect("running cue should advance accepted prompt");
        assert_eq!(running.status, WorkerStatus::Running);
        assert!(running
            .events
            .iter()
            .any(|event| event.kind == WorkerEventKind::Running));
    }

    #[test]
    fn prompt_delivery_detects_wrong_target_and_replays_to_expected_worker() {
        let registry = WorkerRegistry::new();
        let worker = registry.create("/tmp/repo-target-a", &[], true);
        registry
            .observe(&worker.worker_id, "Ready for input\n>")
            .expect("ready observe should succeed");
        registry
            .send_prompt(&worker.worker_id, Some("Run the worker bootstrap tests"))
            .expect("prompt send should succeed");

        let recovered = registry
            .observe(
                &worker.worker_id,
                "/tmp/repo-target-b % Run the worker bootstrap tests\nzsh: command not found: Run",
            )
            .expect("wrong target should be detected");

        assert_eq!(recovered.status, WorkerStatus::ReadyForPrompt);
        assert_eq!(
            recovered.replay_prompt.as_deref(),
            Some("Run the worker bootstrap tests")
        );
        assert!(recovered
            .last_error
            .expect("wrong target error should exist")
            .message
            .contains("wrong target"));
        let misdelivery = recovered
            .events
            .iter()
            .find(|event| event.kind == WorkerEventKind::PromptMisdelivery)
            .expect("wrong-target event should exist");
        assert_eq!(
            misdelivery.payload,
            Some(WorkerEventPayload::PromptDelivery {
                prompt_preview: "Run the worker bootstrap tests".to_string(),
                observed_target: WorkerPromptTarget::WrongTarget,
                observed_cwd: Some("/tmp/repo-target-b".to_string()),
                recovery_armed: false,
            })
        );
    }

    #[test]
    fn await_ready_surfaces_blocked_or_ready_worker_state() {
        let registry = WorkerRegistry::new();
        let worker = registry.create("/tmp/repo-d", &[], false);

        let initial = registry
            .await_ready(&worker.worker_id)
            .expect("await should succeed");
        assert!(!initial.ready);
        assert!(!initial.blocked);

        registry
            .observe(
                &worker.worker_id,
                "Do you trust the files in this folder?\n1. Yes, proceed\n2. No",
            )
            .expect("trust observe should succeed");
        let blocked = registry
            .await_ready(&worker.worker_id)
            .expect("await should succeed");
        assert!(!blocked.ready);
        assert!(blocked.blocked);

        registry
            .resolve_trust(&worker.worker_id)
            .expect("manual trust resolution should succeed");
        registry
            .observe(&worker.worker_id, "Ready for your input\n>")
            .expect("ready observe should succeed");
        let ready = registry
            .await_ready(&worker.worker_id)
            .expect("await should succeed");
        assert!(ready.ready);
        assert!(!ready.blocked);
        assert!(ready.last_error.is_none());
    }

    #[test]
    fn restart_and_terminate_reset_or_finish_worker() {
        let registry = WorkerRegistry::new();
        let worker = registry.create("/tmp/repo-e", &[], true);
        registry
            .observe(&worker.worker_id, "Ready for input\n>")
            .expect("ready observe should succeed");
        registry
            .send_prompt(&worker.worker_id, Some("Run tests"))
            .expect("prompt send should succeed");

        let restarted = registry
            .restart(&worker.worker_id)
            .expect("restart should succeed");
        assert_eq!(restarted.status, WorkerStatus::Spawning);
        assert_eq!(restarted.prompt_delivery_attempts, 0);
        assert!(restarted.last_prompt.is_none());
        assert!(!restarted.prompt_in_flight);

        let finished = registry
            .terminate(&worker.worker_id)
            .expect("terminate should succeed");
        assert_eq!(finished.status, WorkerStatus::Finished);
        assert!(finished
            .events
            .iter()
            .any(|event| event.kind == WorkerEventKind::Finished));
    }

    #[test]
    fn observe_completion_classifies_provider_failure_on_unknown_finish_zero_tokens() {
        let registry = WorkerRegistry::new();
        let worker = registry.create("/tmp/repo-f", &[], true);
        registry
            .observe(&worker.worker_id, "Ready for input\n>")
            .expect("ready observe should succeed");
        registry
            .send_prompt(&worker.worker_id, Some("Run tests"))
            .expect("prompt send should succeed");

        let failed = registry
            .observe_completion(&worker.worker_id, "unknown", 0)
            .expect("completion observe should succeed");

        assert_eq!(failed.status, WorkerStatus::Failed);
        let error = failed.last_error.expect("provider error should exist");
        assert_eq!(error.kind, WorkerFailureKind::Provider);
        assert!(error.message.contains("provider degraded"));
        assert!(failed
            .events
            .iter()
            .any(|event| event.kind == WorkerEventKind::Failed));
    }

    #[test]
    fn emit_state_file_writes_worker_status_on_transition() {
        let cwd_path = std::env::temp_dir().join(format!(
            "Himalaya-state-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&cwd_path).expect("test dir should create");
        let cwd = cwd_path.to_str().expect("test path should be utf8");
        let registry = WorkerRegistry::new();
        let worker = registry.create(cwd, &[], true);

        // After create the worker is Spawning — state file should exist
        let state_path = cwd_path.join(".Himalaya").join("worker-state.json");
        assert!(
            state_path.exists(),
            "state file should exist after worker creation"
        );

        let raw = std::fs::read_to_string(&state_path).expect("state file should be readable");
        let value: serde_json::Value =
            serde_json::from_str(&raw).expect("state file should be valid JSON");
        assert_eq!(
            value["status"].as_str(),
            Some("spawning"),
            "initial status should be spawning"
        );
        assert_eq!(value["is_ready"].as_bool(), Some(false));

        // Transition to ReadyForPrompt by observing trust-cleared text
        registry
            .observe(&worker.worker_id, "Ready for input\n>")
            .expect("observe ready should succeed");

        let raw = std::fs::read_to_string(&state_path)
            .expect("state file should be readable after observe");
        let value: serde_json::Value =
            serde_json::from_str(&raw).expect("state file should be valid JSON after observe");
        assert_eq!(
            value["status"].as_str(),
            Some("ready_for_prompt"),
            "status should be ready_for_prompt after observe"
        );
        assert_eq!(value["is_ready"].as_bool(), Some(true));
        registry
            .send_prompt(&worker.worker_id, Some("Run tests"))
            .expect("send prompt should succeed");
        let raw = std::fs::read_to_string(&state_path)
            .expect("state file should be readable after prompt acceptance");
        let value: serde_json::Value = serde_json::from_str(&raw)
            .expect("state file should be valid JSON after prompt acceptance");
        assert_eq!(
            value["status"].as_str(),
            Some("prompt_accepted"),
            "status should be prompt_accepted after prompt dispatch"
        );
        assert_eq!(value["is_ready"].as_bool(), Some(false));
        assert_eq!(value["prompt_in_flight"].as_bool(), Some(true));
        assert_eq!(
            value["last_event"]["kind"].as_str(),
            Some("prompt_accepted")
        );
    }

    #[test]
    fn heartbeat_extends_worker_lease() {
        let registry = WorkerRegistry::new();
        let worker = registry.create("/tmp/repo-heartbeat", &[], true);
        let original_lease = worker.lease_expires_at;

        let heartbeat = registry
            .heartbeat(&worker.worker_id, DEFAULT_WORKER_LEASE_SECS + 10)
            .expect("heartbeat should succeed");

        assert!(heartbeat.heartbeat_at >= worker.heartbeat_at);
        assert!(heartbeat.lease_expires_at >= original_lease);
        assert!(heartbeat
            .events
            .iter()
            .any(|event| event.kind == WorkerEventKind::Heartbeat));
    }

    #[test]
    fn stale_worker_restarts_with_prompt_replay_until_budget_exhausted() {
        let registry = WorkerRegistry::new();
        let worker = registry.create("/tmp/repo-stale", &[], true);
        registry
            .observe(&worker.worker_id, "Ready for input\n>")
            .expect("ready observe should succeed");
        let accepted = registry
            .send_prompt(&worker.worker_id, Some("Finish stale node"))
            .expect("prompt send should succeed");

        let restarted = registry.restart_stale_workers(accepted.lease_expires_at + 1, 30);

        assert_eq!(restarted.len(), 1);
        assert_eq!(restarted[0].status, WorkerStatus::Spawning);
        assert_eq!(restarted[0].restart_count, 1);
        assert_eq!(
            restarted[0].replay_prompt.as_deref(),
            Some("Finish stale node")
        );
        assert!(restarted[0]
            .events
            .iter()
            .any(|event| event.kind == WorkerEventKind::LeaseExpired));
        assert!(restarted[0]
            .events
            .iter()
            .any(|event| event.kind == WorkerEventKind::Restarted));
    }

    #[test]
    fn stale_worker_fails_after_restart_budget_exhausted() {
        let registry = WorkerRegistry::new();
        let worker = registry.create("/tmp/repo-stale-budget", &[], true);
        registry
            .observe(&worker.worker_id, "Ready for input\n>")
            .expect("ready observe should succeed");
        let mut current = registry
            .send_prompt(&worker.worker_id, Some("Finish stale node"))
            .expect("prompt send should succeed");

        for _ in 0..=DEFAULT_WORKER_MAX_RESTARTS {
            current = registry
                .restart_stale_workers(current.lease_expires_at + 1, 30)
                .pop()
                .expect("stale worker should be updated");
        }

        assert_eq!(current.status, WorkerStatus::Failed);
        assert_eq!(
            current.last_error.expect("lease failure should exist").kind,
            WorkerFailureKind::LeaseExpired
        );
    }

    #[test]
    fn process_backend_spawns_heartbeats_and_records_exit() {
        let registry = WorkerRegistry::new();
        let spec = WorkerProcessSpec::new(
            vec!["sh".to_string(), "-c".to_string(), "exit 0".to_string()],
            std::env::temp_dir(),
        );
        let mut handle = registry
            .spawn_process(spec)
            .expect("worker process should spawn");

        assert_eq!(handle.worker.status, WorkerStatus::Running);
        assert!(handle.worker.process.is_some());
        let observed = loop {
            let worker = registry
                .observe_process(&handle.worker.worker_id, &mut handle.child)
                .expect("process should observe");
            if worker.status == WorkerStatus::Finished {
                break worker;
            }
        };

        assert_eq!(
            observed
                .process
                .as_ref()
                .and_then(|process| process.exit_status),
            Some(0)
        );
        assert!(observed
            .events
            .iter()
            .any(|event| event.kind == WorkerEventKind::ProcessStarted));
        assert!(observed
            .events
            .iter()
            .any(|event| event.kind == WorkerEventKind::ProcessExited));
    }

    #[test]
    fn process_backend_can_spawn_in_isolated_git_worktree() {
        let root = unique_temp_dir("worker-isolated-worktree");
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).expect("repo dir should create");
        git(&repo, &["init", "--quiet", "--initial-branch=main"]);
        git(&repo, &["config", "user.email", "tests@example.com"]);
        git(&repo, &["config", "user.name", "Worker Tests"]);
        std::fs::write(repo.join("marker.txt"), "ok\n").expect("marker should write");
        git(&repo, &["add", "marker.txt"]);
        git(&repo, &["commit", "-m", "initial", "--quiet"]);

        let registry = WorkerRegistry::new();
        let spec = WorkerProcessSpec::new(
            vec![
                "sh".to_string(),
                "-c".to_string(),
                "test -f marker.txt".to_string(),
            ],
            &repo,
        )
        .with_isolation(WorkerIsolationSpec::GitWorktree {
            root: root.join("isolated"),
        });
        let mut handle = registry
            .spawn_process(spec)
            .expect("isolated worker process should spawn");

        let observed = loop {
            let worker = registry
                .observe_process(&handle.worker.worker_id, &mut handle.child)
                .expect("process should observe");
            if matches!(worker.status, WorkerStatus::Finished | WorkerStatus::Failed) {
                break worker;
            }
        };

        assert_eq!(observed.status, WorkerStatus::Finished);
        let isolation = observed
            .isolation
            .as_ref()
            .expect("isolation metadata should exist");
        assert_eq!(isolation.kind, WorkerIsolationKind::GitWorktree);
        assert!(isolation.worktree_path.contains("worker-worktree-"));
        assert_eq!(observed.cwd, isolation.worktree_path);
        assert!(std::path::Path::new(&isolation.worktree_path)
            .join("marker.txt")
            .exists());
        assert!(observed
            .events
            .iter()
            .any(|event| event.kind == WorkerEventKind::WorktreeCreated));
        let report = registry.cleanup_finished();
        assert_eq!(report.removed_workers.len(), 1);
        assert_eq!(report.removed_worktrees.len(), 1);
        assert!(!std::path::Path::new(&isolation.worktree_path).exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_stale_removes_expired_worker_without_live_process() {
        let registry = WorkerRegistry::new();
        let worker = registry.create("/tmp/repo-cleanup-stale", &[], true);
        let expired = registry
            .expire_lease_for_test(&worker.worker_id)
            .expect("lease should expire");

        let report = registry.cleanup_stale(expired.lease_expires_at + 1);

        assert_eq!(report.removed_workers.len(), 1);
        assert_eq!(report.removed_workers[0].worker_id, worker.worker_id);
        assert_eq!(report.retained_workers, 0);
        assert!(registry.get(&worker.worker_id).is_none());
    }

    #[test]
    fn process_backend_records_failed_exit() {
        let registry = WorkerRegistry::new();
        let spec = WorkerProcessSpec::new(
            vec!["sh".to_string(), "-c".to_string(), "exit 7".to_string()],
            std::env::temp_dir(),
        );
        let mut handle = registry
            .spawn_process(spec)
            .expect("worker process should spawn");

        let observed = loop {
            let worker = registry
                .observe_process(&handle.worker.worker_id, &mut handle.child)
                .expect("process should observe");
            if worker.status == WorkerStatus::Failed {
                break worker;
            }
        };

        assert_eq!(
            observed
                .process
                .as_ref()
                .and_then(|process| process.exit_status),
            Some(7)
        );
        assert_eq!(
            observed.last_error.expect("process failure").kind,
            WorkerFailureKind::Provider
        );
    }

    #[test]
    fn observe_completion_accepts_normal_finish_with_tokens() {
        let registry = WorkerRegistry::new();
        let worker = registry.create("/tmp/repo-g", &[], true);
        registry
            .observe(&worker.worker_id, "Ready for input\n>")
            .expect("ready observe should succeed");
        registry
            .send_prompt(&worker.worker_id, Some("Run tests"))
            .expect("prompt send should succeed");

        let finished = registry
            .observe_completion(&worker.worker_id, "stop", 150)
            .expect("completion observe should succeed");

        assert_eq!(finished.status, WorkerStatus::Finished);
        assert!(finished.last_error.is_none());
        assert!(finished
            .events
            .iter()
            .any(|event| event.kind == WorkerEventKind::Finished));
    }
}
