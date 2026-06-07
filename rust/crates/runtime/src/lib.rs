//! Core runtime primitives for the `Himalaya` CLI and supporting crates.
//!
//! This crate owns session persistence, permission evaluation, prompt assembly,
//! MCP plumbing, tool-facing file operations, and the core conversation loop
//! that drives interactive and one-shot turns.

pub mod autonomous_run;
mod bash;
pub mod bash_validation;
pub mod benchmark;
mod bootstrap;
pub mod branch_lock;
mod compact;
mod config;
pub mod config_validate;
mod conversation;
pub mod cron_schedule;
mod decisioning;
pub mod execution_scheduler;
pub mod failure_classifier;
mod file_ops;
mod git_context;
pub mod green_contract;
mod hooks;
mod json;
mod lane_events;
pub mod lsp_client;
mod mcp;
mod mcp_client;
pub mod mcp_lifecycle_hardened;
pub mod mcp_server;
pub mod mcp_stdio;
pub mod mcp_tool_bridge;
pub mod model_router;
mod oauth;
pub mod permission_enforcer;
mod permissions;
pub mod plan_executor;
pub mod plugin_lifecycle;
mod policy_engine;
mod prompt;
pub mod recovery_actions;
pub mod recovery_orchestrator;
pub mod recovery_recipes;
mod remote;
pub mod route_feedback_store;
pub mod runtime_events;
pub mod sandbox;
mod session;
pub mod session_control;
pub use session_control::SessionStore;
mod sse;
pub mod stale_base;
pub mod stale_branch;
pub mod structured_execution;
pub mod summary_compression;
pub mod task_execution_engine;
pub mod task_memory_store;
pub mod task_packet;
pub mod task_registry;
pub mod team_convergence;
pub mod team_coordinator;
pub mod team_cron_registry;
pub mod team_execution;
#[cfg(test)]
mod trust_resolver;
mod usage;
pub mod verification_runner;
pub mod verifier;
pub mod worker_boot;
pub mod worker_supervisor;

pub use autonomous_run::{
    append_autonomous_run_report, autonomous_runs_path, latest_autonomous_run_report,
    load_autonomous_run_reports, load_autonomous_run_reports_with_diagnostics,
    review_autonomous_policy, summarize_autonomous_runs, AutonomousBlockedRecoveryAction,
    AutonomousPolicyAction, AutonomousPolicyRecommendation, AutonomousPolicyReview,
    AutonomousRecoveryPolicyAudit, AutonomousRunCoordinator, AutonomousRunFrequency,
    AutonomousRunHistorySummary, AutonomousRunLoad, AutonomousRunReadWarning, AutonomousRunReport,
    AutonomousRunStatus, AutonomousRunStatusCounts, AutonomousRunStep, AutonomousRunStore,
};
pub use bash::{execute_bash, BashCommandInput, BashCommandOutput};
pub use benchmark::{
    complex_coding_benchmark_suite, run_benchmark_suite, run_complex_coding_benchmark,
    BenchmarkRun, BenchmarkScore, BenchmarkSuite, BenchmarkSummary, BenchmarkTaskResult,
    BenchmarkTaskSpec, COMPLEX_CODING_BENCHMARK_SUITE_ID, COMPLEX_CODING_BENCHMARK_VERSION,
};
pub use bootstrap::{BootstrapPhase, BootstrapPlan};
pub use branch_lock::{detect_branch_lock_collisions, BranchLockCollision, BranchLockIntent};
pub use compact::{
    compact_session, estimate_session_tokens, format_compact_summary,
    get_compact_continuation_message, should_compact, CompactionConfig, CompactionResult,
};
pub use config::{
    ConfigEntry, ConfigError, ConfigLoader, ConfigSource, DecisioningConfig,
    DecisioningSafetyPolicyConfig, Himalaya_SETTINGS_SCHEMA_NAME, McpConfigCollection,
    McpManagedProxyServerConfig, McpOAuthConfig, McpRemoteServerConfig, McpSdkServerConfig,
    McpServerConfig, McpStdioServerConfig, McpTransport, McpWebSocketServerConfig,
    ModelRoutingConfig, OAuthConfig, ProviderFallbackConfig, ResolvedPermissionMode, RuntimeConfig,
    RuntimeFeatureConfig, RuntimeHookConfig, RuntimePermissionRuleConfig, RuntimePluginConfig,
    ScopedMcpServerConfig,
};
pub use config_validate::{
    check_unsupported_format, format_diagnostics, validate_config_file, ConfigDiagnostic,
    DiagnosticKind, ValidationResult,
};
pub use conversation::{
    auto_compaction_threshold_from_env, AlternativeApproach, ApiClient, ApiRequest, AssistantEvent,
    AutoCompactionEvent, ChainOfThought, ConversationRuntime, DecisioningEventReporter,
    ExecutionPlan, LongTermMemory, MemoryEntry, MemoryKind, ModelRouteEventReporter,
    PlanExecutionEventReporter, ProblemAnalysis, PromptCacheEvent, ReasoningStep, RuntimeError,
    SelfCritique, StaticToolExecutor, StrategyAdjustment, TaskLedgerEventReporter,
    TeamExecutionEventReporter, ToolError, ToolExecutor, TurnSummary,
};
pub use decisioning::{
    build_plan_dag, build_plan_tree, infer_tool_capabilities, tool_from_profile, DecisioningEngine,
    DecisioningEvent, DecisioningEventKind, DecisioningSnapshot, ExecutionMode, PlanAdjustment,
    PlanDag, PlanDagEdge, PlanDagEdgeKind, PlanDagNode, PlanNodeKind, ReasoningContext,
    RiskAssessment, SafetyOutcome, SafetyPolicy, StepOutcome, Subtask, Task, TaskPlan, TaskPlanner,
    Tool, ToolHistoryEntry, ToolSelector,
};
pub use execution_scheduler::{
    durable_status_for_task, DurableSchedulerExplain, DurableSchedulerStatus,
    DurableSchedulerTaskSnapshot, DurableSchedulerTick, DurableTaskScheduler, ExecutionScheduler,
    SchedulerDaemon, SchedulerDaemonEvent, SchedulerDaemonRun, SchedulerDaemonState,
    SchedulerDaemonStatus, SchedulerNodeOutcome, SchedulerNodeSelection,
};
pub use failure_classifier::{FailureClassification, FailureClassifier};
pub use file_ops::{
    edit_file, edit_file_in_workspace, generate_file, generate_file_in_workspace, glob_search,
    grep_search, read_file, read_file_in_workspace, write_file, write_file_in_workspace,
    EditFileOutput, GenerateFileOutput, GlobSearchOutput, GrepSearchInput, GrepSearchOutput,
    ReadFileOutput, StructuredPatchHunk, TextFilePayload, WriteFileOutput,
};
pub use git_context::{GitCommitEntry, GitContext};
pub use hooks::{
    HookAbortSignal, HookEvent, HookProgressEvent, HookProgressReporter, HookRunResult, HookRunner,
};
pub use lane_events::{
    dedupe_superseded_commit_events, LaneCommitProvenance, LaneEvent, LaneEventBlocker,
    LaneEventName, LaneEventStatus, LaneFailureClass,
};
pub use mcp::{
    mcp_server_signature, mcp_tool_name, mcp_tool_prefix, normalize_name_for_mcp,
    scoped_mcp_config_hash, unwrap_ccr_proxy_url,
};
pub use mcp_client::{
    McpClientAuth, McpClientBootstrap, McpClientTransport, McpManagedProxyTransport,
    McpRemoteTransport, McpSdkTransport, McpStdioTransport,
};
pub use mcp_lifecycle_hardened::{
    McpDegradedReport, McpErrorSurface, McpFailedServer, McpLifecyclePhase, McpLifecycleState,
    McpLifecycleValidator, McpPhaseResult,
};
pub use mcp_server::{McpServer, McpServerSpec, ToolCallHandler, MCP_SERVER_PROTOCOL_VERSION};
pub use mcp_stdio::{
    spawn_mcp_stdio_process, JsonRpcError, JsonRpcId, JsonRpcRequest, JsonRpcResponse,
    ManagedMcpTool, McpDiscoveryFailure, McpInitializeClientInfo, McpInitializeParams,
    McpInitializeResult, McpInitializeServerInfo, McpListResourcesParams, McpListResourcesResult,
    McpListToolsParams, McpListToolsResult, McpReadResourceParams, McpReadResourceResult,
    McpResource, McpResourceContents, McpServerManager, McpServerManagerError, McpStdioProcess,
    McpTool, McpToolCallContent, McpToolCallParams, McpToolCallResult, McpToolDiscoveryReport,
    UnsupportedMcpServer,
};
pub use model_router::{
    MoERoutingPolicy, ModelCapability, ModelRoute, ModelRouteDecision, ModelRouteFeedback,
    ModelRoutePhase, ModelRouter,
};
pub use oauth::{
    clear_oauth_credentials, code_challenge_s256, credentials_path, generate_pkce_pair,
    generate_state, load_oauth_credentials, loopback_redirect_uri, parse_oauth_callback_query,
    parse_oauth_callback_request_target, save_oauth_credentials, OAuthAuthorizationRequest,
    OAuthCallbackParams, OAuthRefreshRequest, OAuthTokenExchangeRequest, OAuthTokenSet,
    PkceChallengeMethod, PkceCodePair,
};
pub use permissions::{
    PermissionContext, PermissionMode, PermissionModeParseError, PermissionOutcome,
    PermissionOverride, PermissionPolicy, PermissionPromptDecision, PermissionPrompter,
    PermissionRequest,
};
pub use plan_executor::{
    dependency_map, reverse_dependency_map, NodeVerificationGate, PlanExecution,
    PlanExecutionEvent, PlanExecutionEventKind, PlanNodeExecution, PlanNodeStatus,
};
pub use plugin_lifecycle::{
    DegradedMode, DiscoveryResult, PluginHealthcheck, PluginLifecycle, PluginLifecycleEvent,
    PluginState, ResourceInfo, ServerHealth, ServerStatus, ToolInfo,
};
pub use policy_engine::{
    evaluate, DiffScope, GreenLevel, LaneBlocker, LaneContext, PolicyAction, PolicyCondition,
    PolicyEngine, PolicyRule, ReconcileReason, ReviewStatus,
};
pub use prompt::{
    load_system_prompt, prepend_bullets, ContextFile, ProjectContext, PromptBuildError,
    SystemPromptBuilder, FRONTIER_MODEL_NAME, SYSTEM_PROMPT_DYNAMIC_BOUNDARY,
};
pub use recovery_actions::{
    RecoveryAction, RecoveryActionEngine, RecoveryActionExecution, RecoveryActionKind,
    RecoveryActionPlan, RecoveryActionResult, RecoveryActionRisk,
};
pub use recovery_orchestrator::{
    RecoveryOrchestrator, RecoveryOrchestratorDecision, RecoveryOrchestratorOutcome,
};
pub use recovery_recipes::{
    attempt_recovery, recipe_for, EscalationPolicy, FailureScenario, RecoveryContext,
    RecoveryEvent, RecoveryRecipe, RecoveryResult, RecoveryStep,
};
pub use remote::{
    inherited_upstream_proxy_env, no_proxy_list, read_token, upstream_proxy_ws_url,
    RemoteSessionContext, UpstreamProxyBootstrap, UpstreamProxyState, DEFAULT_REMOTE_BASE_URL,
    DEFAULT_SESSION_TOKEN_PATH, DEFAULT_SYSTEM_CA_BUNDLE, NO_PROXY_HOSTS, UPSTREAM_PROXY_ENV_KEYS,
};
pub use route_feedback_store::{RouteFeedbackSnapshot, RouteFeedbackStore, RouteFeedbackSummary};
pub use runtime_events::{
    append_runtime_event_log, read_runtime_event_log, RuntimeEvent, RuntimeEventEnvelope,
    RuntimeEventLog, RuntimeEventReporter, RuntimeEventSeverity,
};
pub use sandbox::{
    build_linux_sandbox_command, detect_container_environment, detect_container_environment_from,
    resolve_sandbox_status, resolve_sandbox_status_for_request, ContainerEnvironment,
    FilesystemIsolationMode, LinuxSandboxCommand, SandboxConfig, SandboxDetectionInputs,
    SandboxRequest, SandboxStatus,
};
pub use session::{
    ContentBlock, ConversationMessage, MessageRole, Session, SessionCompaction, SessionError,
    SessionFork, SessionPromptEntry,
};
pub use sse::{IncrementalSseParser, SseEvent};
pub use stale_base::{
    check_base_commit, format_stale_base_warning, read_Himalaya_base_file, resolve_expected_base,
    BaseCommitSource, BaseCommitState,
};
pub use stale_branch::{
    apply_policy, check_freshness, BranchFreshness, StaleBranchAction, StaleBranchEvent,
    StaleBranchPolicy,
};
pub use task_execution_engine::{
    task_execution_report_from_parts, task_plan_progress, TaskExecutionEngine,
    TaskExecutionOutcome, TaskExecutionReport, TaskExecutionStep, TaskExecutionStepKind,
    TaskPlanProgress,
};
pub use task_memory_store::{
    RecoveryActionMemorySummary, TaskMemoryContext, TaskMemoryEntry, TaskMemorySnapshot,
    TaskMemoryStore, TaskMemorySummary, TaskRecoveryActionSignal,
};
pub use task_packet::{validate_packet, TaskPacket, TaskPacketValidationError, ValidatedPacket};
pub use task_registry::{
    ProgressLedgerEntry, TaskCheckpoint, TaskEventLogEntry, TaskPlanSnapshot, TaskRegistry,
    TaskResumeCursor, TaskStatus,
};
pub use team_coordinator::{TeamCoordinationPlan, TeamCoordinator, WorkerAssignment};
pub use team_execution::{
    TeamExecutionEvent, TeamExecutionEventKind, TeamExecutionLedger, TeamExecutionSummary, TeamRole,
};
#[cfg(test)]
pub use trust_resolver::{TrustConfig, TrustDecision, TrustEvent, TrustPolicy, TrustResolver};
pub use usage::{
    format_usd, pricing_for_model, ModelPricing, TokenUsage, UsageCostEstimate, UsageTracker,
};
pub use verification_runner::{VerificationCommandResult, VerificationRunner};
pub use verifier::{
    build_verification_request, evaluate_verification_result, infer_verification_policy,
    VerificationDecision, VerificationPolicy, VerificationRequest, VerificationResult,
};
pub use worker_boot::{
    Worker, WorkerCleanupReport, WorkerEvent, WorkerEventKind, WorkerEventPayload, WorkerFailure,
    WorkerFailureKind, WorkerIsolation, WorkerIsolationKind, WorkerIsolationSpec, WorkerProcess,
    WorkerProcessHandle, WorkerProcessSpec, WorkerPromptTarget, WorkerReadySnapshot,
    WorkerRegistry, WorkerStatus, WorkerTrustResolution, DEFAULT_WORKER_LEASE_SECS,
    DEFAULT_WORKER_MAX_RESTARTS,
};
pub use worker_supervisor::{
    WorkerEventIndexEntry, WorkerSupervisor, WorkerSupervisorCapacity, WorkerSupervisorStatus,
    WorkerSupervisorTick,
};

#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
