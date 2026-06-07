use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{
    DurableSchedulerStatus, DurableSchedulerTaskSnapshot, PermissionMode, RecoveryActionKind,
    RecoveryActionRisk, SchedulerDaemon, SchedulerDaemonRun, SchedulerDaemonState, TaskMemoryStore,
    TaskRegistry, TaskStatus, WorkerSupervisor, WorkerSupervisorStatus, WorkerSupervisorTick,
};

const DEFAULT_AUTONOMOUS_SUMMARY_LIMIT: usize = 20;
const MAX_AUTONOMOUS_FREQUENCIES: usize = 5;

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
    #[serde(default)]
    pub prior_summary: Option<AutonomousRunHistorySummary>,
    #[serde(default)]
    pub policy_recommendation: Option<AutonomousPolicyRecommendation>,
    pub worker_supervisor_ticks: Vec<WorkerSupervisorTick>,
    pub scheduler_runs: Vec<SchedulerDaemonRun>,
    pub policy_audit: Vec<AutonomousRecoveryPolicyAudit>,
    pub final_queue: Vec<DurableSchedulerTaskSnapshot>,
    pub latest_daemon_state: Option<SchedulerDaemonState>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomousRunReadWarning {
    pub line: usize,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutonomousRunLoad {
    pub runs_path: PathBuf,
    pub reports: Vec<AutonomousRunReport>,
    pub malformed_lines: usize,
    pub warnings: Vec<AutonomousRunReadWarning>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AutonomousRunStatusCounts {
    pub idle: usize,
    pub running: usize,
    pub blocked: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutonomousRunFrequency {
    pub value: String,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutonomousRunHistorySummary {
    pub runs_path: PathBuf,
    pub considered_runs: usize,
    pub malformed_lines: usize,
    pub latest_run_id: Option<String>,
    pub status_counts: AutonomousRunStatusCounts,
    pub idle_rate: f64,
    pub running_rate: f64,
    pub blocked_rate: f64,
    pub average_ticks: f64,
    pub average_ticks_to_idle: Option<f64>,
    pub average_ticks_to_blocked: Option<f64>,
    pub consecutive_blocked_runs: usize,
    pub repeated_blocked_actions: Vec<AutonomousRunFrequency>,
    pub repeated_blocked_risks: Vec<AutonomousRunFrequency>,
    pub repeated_blocked_reasons: Vec<AutonomousRunFrequency>,
    pub repeated_task_types: Vec<AutonomousRunFrequency>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutonomousPolicyAction {
    Continue,
    ReduceTicks,
    CoolDown,
    RequestReview,
}

impl AutonomousPolicyAction {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::ReduceTicks => "reduce_ticks",
            Self::CoolDown => "cool_down",
            Self::RequestReview => "request_review",
        }
    }
}

impl std::fmt::Display for AutonomousPolicyAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutonomousPolicyRecommendation {
    pub requested_max_ticks: usize,
    pub recommended_max_ticks: usize,
    pub permission_mode: String,
    pub conservative_permission_mode: String,
    pub action: AutonomousPolicyAction,
    pub review_required: bool,
    pub cool_down: bool,
    pub reasons: Vec<String>,
}

impl AutonomousPolicyRecommendation {
    #[must_use]
    pub fn action_label(&self) -> &'static str {
        self.action.as_str()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutonomousPolicyReview {
    pub summary: AutonomousRunHistorySummary,
    pub recommendation: AutonomousPolicyRecommendation,
}

#[derive(Debug, Clone)]
pub struct AutonomousRunStore {
    state_dir: PathBuf,
}

impl AutonomousRunStore {
    #[must_use]
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
        }
    }

    #[must_use]
    pub fn runs_path(&self) -> PathBuf {
        autonomous_runs_path(&self.state_dir)
    }

    pub fn append(&self, report: &AutonomousRunReport) -> io::Result<()> {
        append_autonomous_run_report(&self.state_dir, report)
    }

    pub fn load(&self, limit: usize) -> io::Result<AutonomousRunLoad> {
        load_autonomous_run_reports_with_diagnostics(&self.state_dir, limit)
    }

    pub fn summarize(&self, limit: usize) -> io::Result<AutonomousRunHistorySummary> {
        let load = self.load(limit)?;
        Ok(summarize_autonomous_run_load(load))
    }

    pub fn review(
        &self,
        limit: usize,
        requested_max_ticks: usize,
        permission_mode: PermissionMode,
    ) -> io::Result<AutonomousPolicyReview> {
        let summary = self.summarize(limit)?;
        let recommendation =
            recommend_autonomous_policy(&summary, requested_max_ticks, permission_mode);
        Ok(AutonomousPolicyReview {
            summary,
            recommendation,
        })
    }
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
        let requested_max_ticks = max_ticks.max(1);
        let prior_review =
            self.review_policy(DEFAULT_AUTONOMOUS_SUMMARY_LIMIT, requested_max_ticks)?;
        let max_ticks = prior_review
            .recommendation
            .recommended_max_ticks
            .min(requested_max_ticks)
            .max(1);
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
            prior_summary: Some(prior_review.summary),
            policy_recommendation: Some(prior_review.recommendation),
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

    pub fn review_policy(
        &self,
        limit: usize,
        requested_max_ticks: usize,
    ) -> io::Result<AutonomousPolicyReview> {
        AutonomousRunStore::new(&self.state_dir).review(
            limit,
            requested_max_ticks,
            self.permission_mode,
        )
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
    Ok(load_autonomous_run_reports_with_diagnostics(state_dir, limit)?.reports)
}

pub fn load_autonomous_run_reports_with_diagnostics(
    state_dir: &Path,
    limit: usize,
) -> io::Result<AutonomousRunLoad> {
    let path = autonomous_runs_path(state_dir);
    if !path.exists() {
        return Ok(AutonomousRunLoad {
            runs_path: path,
            reports: Vec::new(),
            malformed_lines: 0,
            warnings: Vec::new(),
        });
    }
    let contents = fs::read_to_string(path)?;
    let mut warnings = Vec::new();
    let mut reports = contents
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .filter_map(
            |(index, line)| match serde_json::from_str::<AutonomousRunReport>(line) {
                Ok(report) => Some(report),
                Err(error) => {
                    warnings.push(AutonomousRunReadWarning {
                        line: index.saturating_add(1),
                        message: error.to_string(),
                    });
                    None
                }
            },
        )
        .collect::<Vec<_>>();
    if limit > 0 && reports.len() > limit {
        let start = reports.len() - limit;
        reports = reports.split_off(start);
    }
    let malformed_lines = warnings.len();
    Ok(AutonomousRunLoad {
        runs_path: autonomous_runs_path(state_dir),
        reports,
        malformed_lines,
        warnings,
    })
}

pub fn latest_autonomous_run_report(state_dir: &Path) -> io::Result<Option<AutonomousRunReport>> {
    Ok(load_autonomous_run_reports(state_dir, 1)?.pop())
}

pub fn summarize_autonomous_runs(
    state_dir: &Path,
    limit: usize,
) -> io::Result<AutonomousRunHistorySummary> {
    let load = load_autonomous_run_reports_with_diagnostics(state_dir, limit)?;
    Ok(summarize_autonomous_run_load(load))
}

#[must_use]
pub fn summarize_autonomous_run_reports(
    reports: Vec<AutonomousRunReport>,
    runs_path: PathBuf,
    malformed_lines: usize,
) -> AutonomousRunHistorySummary {
    summarize_autonomous_run_load(AutonomousRunLoad {
        runs_path,
        reports,
        malformed_lines,
        warnings: Vec::new(),
    })
}

pub fn review_autonomous_policy(
    state_dir: &Path,
    limit: usize,
    requested_max_ticks: usize,
    permission_mode: PermissionMode,
) -> io::Result<AutonomousPolicyReview> {
    AutonomousRunStore::new(state_dir).review(limit, requested_max_ticks, permission_mode)
}

#[must_use]
pub fn recommend_autonomous_policy_for_summary(
    summary: &AutonomousRunHistorySummary,
    requested_max_ticks: usize,
    permission_mode: PermissionMode,
) -> AutonomousPolicyRecommendation {
    recommend_autonomous_policy(summary, requested_max_ticks, permission_mode)
}

fn summarize_autonomous_run_load(load: AutonomousRunLoad) -> AutonomousRunHistorySummary {
    let mut status_counts = AutonomousRunStatusCounts::default();
    let mut total_ticks = 0_usize;
    let mut idle_ticks = Vec::new();
    let mut blocked_ticks = Vec::new();
    let mut blocked_actions = BTreeMap::new();
    let mut blocked_risks = BTreeMap::new();
    let mut blocked_reasons = BTreeMap::new();
    let mut task_types = BTreeMap::new();

    for report in &load.reports {
        total_ticks = total_ticks.saturating_add(report.tick_count);
        match report.status {
            AutonomousRunStatus::Idle => {
                status_counts.idle = status_counts.idle.saturating_add(1);
                idle_ticks.push(report.tick_count);
            }
            AutonomousRunStatus::Running => {
                status_counts.running = status_counts.running.saturating_add(1);
            }
            AutonomousRunStatus::Blocked => {
                status_counts.blocked = status_counts.blocked.saturating_add(1);
                blocked_ticks.push(report.tick_count);
            }
        }
        for audit in &report.policy_audit {
            for action in &audit.blocked_actions {
                increment_frequency(
                    &mut blocked_actions,
                    recovery_action_kind_label(&action.kind),
                );
                increment_frequency(&mut blocked_risks, recovery_action_risk_label(action.risk));
                if !action.reason.trim().is_empty() {
                    increment_frequency(&mut blocked_reasons, action.reason.clone());
                }
            }
        }
        for task in report
            .scheduler_runs
            .iter()
            .filter_map(|run| run.tick.task.as_ref())
        {
            increment_frequency(&mut task_types, TaskMemoryStore::task_type_for(task));
        }
    }

    let considered_runs = load.reports.len();
    let consecutive_blocked_runs = load
        .reports
        .iter()
        .rev()
        .take_while(|report| report.status == AutonomousRunStatus::Blocked)
        .count();
    AutonomousRunHistorySummary {
        runs_path: load.runs_path,
        considered_runs,
        malformed_lines: load.malformed_lines,
        latest_run_id: load.reports.last().map(|report| report.run_id.clone()),
        status_counts,
        idle_rate: rate(
            load.reports
                .iter()
                .filter(|report| report.status == AutonomousRunStatus::Idle)
                .count(),
            considered_runs,
        ),
        running_rate: rate(
            load.reports
                .iter()
                .filter(|report| report.status == AutonomousRunStatus::Running)
                .count(),
            considered_runs,
        ),
        blocked_rate: rate(
            load.reports
                .iter()
                .filter(|report| report.status == AutonomousRunStatus::Blocked)
                .count(),
            considered_runs,
        ),
        average_ticks: average(total_ticks, considered_runs),
        average_ticks_to_idle: average_option(&idle_ticks),
        average_ticks_to_blocked: average_option(&blocked_ticks),
        consecutive_blocked_runs,
        repeated_blocked_actions: top_frequencies(blocked_actions),
        repeated_blocked_risks: top_frequencies(blocked_risks),
        repeated_blocked_reasons: top_frequencies(blocked_reasons),
        repeated_task_types: top_frequencies(task_types),
    }
}

fn recommend_autonomous_policy(
    summary: &AutonomousRunHistorySummary,
    requested_max_ticks: usize,
    permission_mode: PermissionMode,
) -> AutonomousPolicyRecommendation {
    let requested_max_ticks = requested_max_ticks.max(1);
    let mut recommended_max_ticks = requested_max_ticks;
    let mut action = AutonomousPolicyAction::Continue;
    let mut review_required = false;
    let mut cool_down = false;
    let mut conservative_permission_mode = permission_mode.as_str().to_string();
    let mut reasons = Vec::new();

    if summary.considered_runs == 0 {
        reasons.push("no prior autonomous runs; using requested max_ticks".to_string());
    }

    if summary.malformed_lines > 0 {
        review_required = true;
        reasons.push(format!(
            "{} malformed autonomous run log line(s) were skipped",
            summary.malformed_lines
        ));
    }

    if summary.consecutive_blocked_runs >= 2 {
        review_required = true;
        cool_down = true;
        recommended_max_ticks = 1;
        action = AutonomousPolicyAction::RequestReview;
        conservative_permission_mode = PermissionMode::ReadOnly.as_str().to_string();
        reasons.push(format!(
            "{} consecutive autonomous run(s) ended blocked",
            summary.consecutive_blocked_runs
        ));
    } else if summary.considered_runs >= 3 && summary.blocked_rate >= 0.5 {
        review_required = true;
        recommended_max_ticks = 1;
        action = AutonomousPolicyAction::CoolDown;
        conservative_permission_mode = PermissionMode::ReadOnly.as_str().to_string();
        reasons.push(format!(
            "blocked rate {:.0}% across recent autonomous runs",
            summary.blocked_rate * 100.0
        ));
    }

    if let Some(blocker) = summary.repeated_blocked_actions.first() {
        if blocker.count >= 2 {
            review_required = true;
            if action == AutonomousPolicyAction::Continue {
                action = AutonomousPolicyAction::RequestReview;
            }
            reasons.push(format!(
                "blocked recovery action '{}' repeated {} time(s)",
                blocker.value, blocker.count
            ));
        }
    }

    if action == AutonomousPolicyAction::Continue
        && requested_max_ticks > 1
        && summary.considered_runs >= 2
        && summary.blocked_rate == 0.0
    {
        if let Some(avg_idle) = summary.average_ticks_to_idle {
            if summary.idle_rate >= 0.6 && avg_idle < requested_max_ticks as f64 {
                recommended_max_ticks =
                    ((avg_idle.ceil() as usize).saturating_add(1)).min(requested_max_ticks);
                if recommended_max_ticks < requested_max_ticks {
                    action = AutonomousPolicyAction::ReduceTicks;
                    reasons.push(format!(
                        "recent runs usually reached idle in {:.1} tick(s)",
                        avg_idle
                    ));
                }
            }
        }
    }

    if reasons.is_empty() {
        reasons
            .push("recent autonomous runs do not require a conservative policy change".to_string());
    }

    AutonomousPolicyRecommendation {
        requested_max_ticks,
        recommended_max_ticks: recommended_max_ticks.max(1),
        permission_mode: permission_mode.as_str().to_string(),
        conservative_permission_mode,
        action,
        review_required,
        cool_down,
        reasons,
    }
}

fn increment_frequency(map: &mut BTreeMap<String, usize>, value: String) {
    *map.entry(value).or_default() += 1;
}

fn top_frequencies(map: BTreeMap<String, usize>) -> Vec<AutonomousRunFrequency> {
    let mut frequencies = map
        .into_iter()
        .map(|(value, count)| AutonomousRunFrequency { value, count })
        .collect::<Vec<_>>();
    frequencies.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.value.cmp(&right.value))
    });
    frequencies.truncate(MAX_AUTONOMOUS_FREQUENCIES);
    frequencies
}

fn rate(count: usize, total: usize) -> f64 {
    if total == 0 {
        0.0
    } else {
        count as f64 / total as f64
    }
}

fn average(total: usize, count: usize) -> f64 {
    if count == 0 {
        0.0
    } else {
        total as f64 / count as f64
    }
}

fn average_option(values: &[usize]) -> Option<f64> {
    (!values.is_empty()).then(|| average(values.iter().sum(), values.len()))
}

fn recovery_action_kind_label(kind: &RecoveryActionKind) -> String {
    match kind {
        RecoveryActionKind::RerunVerification => "rerun_verification",
        RecoveryActionKind::RetryNode => "retry_node",
        RecoveryActionKind::RequestPermission => "request_permission",
        RecoveryActionKind::SwitchModel => "switch_model",
        RecoveryActionKind::RestartPlugin => "restart_plugin",
        RecoveryActionKind::RetryMcpHandshake => "retry_mcp_handshake",
        RecoveryActionKind::MarkBlocked => "mark_blocked",
        RecoveryActionKind::Escalate => "escalate",
    }
    .to_string()
}

fn recovery_action_risk_label(risk: RecoveryActionRisk) -> String {
    match risk {
        RecoveryActionRisk::Safe => "safe",
        RecoveryActionRisk::NeedsWorkspaceWrite => "needs_workspace_write",
        RecoveryActionRisk::NeedsDangerFullAccess => "needs_danger_full_access",
        RecoveryActionRisk::NeedsHuman => "needs_human",
    }
    .to_string()
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

    fn minimal_report(
        run_id: &str,
        status: AutonomousRunStatus,
        tick_count: usize,
    ) -> AutonomousRunReport {
        AutonomousRunReport {
            run_id: run_id.to_string(),
            started_at: 1,
            updated_at: 1,
            status,
            permission_mode: PermissionMode::ReadOnly.as_str().to_string(),
            max_ticks: tick_count.max(1),
            tick_count,
            prior_summary: None,
            policy_recommendation: None,
            worker_supervisor_ticks: Vec::new(),
            scheduler_runs: Vec::new(),
            policy_audit: Vec::new(),
            final_queue: Vec::new(),
            latest_daemon_state: None,
            message: status.to_string(),
        }
    }

    fn blocked_report(run_id: &str) -> AutonomousRunReport {
        let mut report = minimal_report(run_id, AutonomousRunStatus::Blocked, 1);
        report.policy_audit = vec![AutonomousRecoveryPolicyAudit {
            task_id: format!("{run_id}-task"),
            task_status: TaskStatus::WaitingForPermission,
            permission_mode: PermissionMode::ReadOnly.as_str().to_string(),
            recovery_triggered: true,
            executed_actions: 0,
            blocked_actions: vec![AutonomousBlockedRecoveryAction {
                kind: RecoveryActionKind::RequestPermission,
                risk: RecoveryActionRisk::NeedsHuman,
                reason: "needs human approval".to_string(),
            }],
            requires_user: true,
            message: "recovery is waiting for user permission".to_string(),
        }];
        report
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
        assert_eq!(
            report
                .prior_summary
                .as_ref()
                .expect("prior summary")
                .considered_runs,
            0
        );
        assert_eq!(
            report
                .policy_recommendation
                .as_ref()
                .expect("policy recommendation")
                .recommended_max_ticks,
            2
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

    #[test]
    fn run_store_summarizes_history_and_skips_malformed_lines() {
        let dir = state_dir("summary");
        append_autonomous_run_report(&dir, &blocked_report("blocked-1"))
            .expect("first report should append");
        {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(autonomous_runs_path(&dir))
                .expect("runs file should open");
            writeln!(file, "{{not-json").expect("malformed line should write");
        }
        append_autonomous_run_report(&dir, &blocked_report("blocked-2"))
            .expect("second report should append");

        let store = AutonomousRunStore::new(&dir);
        let load = store.load(20).expect("reports should load");
        assert_eq!(load.reports.len(), 2);
        assert_eq!(load.malformed_lines, 1);

        let review = store
            .review(20, 4, PermissionMode::Prompt)
            .expect("review should summarize");
        assert_eq!(review.summary.considered_runs, 2);
        assert_eq!(review.summary.status_counts.blocked, 2);
        assert_eq!(review.summary.consecutive_blocked_runs, 2);
        assert_eq!(review.summary.malformed_lines, 1);
        assert_eq!(
            review.summary.repeated_blocked_actions[0].value,
            "request_permission"
        );
        assert_eq!(
            review.recommendation.action,
            AutonomousPolicyAction::RequestReview
        );
        assert!(review.recommendation.review_required);
        assert!(review.recommendation.cool_down);
        assert_eq!(review.recommendation.recommended_max_ticks, 1);
        assert_eq!(
            review.recommendation.conservative_permission_mode,
            PermissionMode::ReadOnly.as_str()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn policy_reduces_ticks_when_recent_runs_idle_quickly() {
        let dir = state_dir("idle");
        append_autonomous_run_report(
            &dir,
            &minimal_report("idle-1", AutonomousRunStatus::Idle, 1),
        )
        .expect("first report should append");
        append_autonomous_run_report(
            &dir,
            &minimal_report("idle-2", AutonomousRunStatus::Idle, 1),
        )
        .expect("second report should append");

        let review = AutonomousRunStore::new(&dir)
            .review(20, 5, PermissionMode::ReadOnly)
            .expect("review should summarize");

        assert_eq!(review.summary.status_counts.idle, 2);
        assert_eq!(
            review.recommendation.action,
            AutonomousPolicyAction::ReduceTicks
        );
        assert_eq!(review.recommendation.recommended_max_ticks, 2);
        assert!(!review.recommendation.review_required);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
