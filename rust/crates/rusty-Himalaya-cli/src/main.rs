#![allow(
    dead_code,
    unused_imports,
    unused_variables,
    clippy::unneeded_struct_pattern,
    clippy::unnecessary_wraps,
    clippy::unused_self
)]
mod autonomous_cli;
mod init;
mod input;
mod model_selector;
mod provider_config;
mod render;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::net::TcpListener;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, UNIX_EPOCH};

use api::{
    detect_provider_kind, model_supports_vision, oauth_token_is_expired,
    resolve_startup_auth_source, AnthropicClient, AuthSource, ContentBlockDelta, InputContentBlock,
    InputMessage, MessageRequest, MessageResponse, OutputContentBlock, PromptCache,
    ProviderClient as ApiProviderClient, ProviderKind, StreamEvent as ApiStreamEvent, ToolChoice,
    ToolDefinition, ToolResultContentBlock,
};

use autonomous_cli::{
    render_autonomous_benchmark_text, render_autonomous_daemon_report_text,
    render_autonomous_evaluation_text, render_autonomous_health_checkpoint_text,
    render_autonomous_integration_text, render_autonomous_policy_summary_text,
    render_autonomous_preflight_blocked_text, render_autonomous_replay_text,
};
use commands::{
    classify_skills_slash_command, handle_agents_slash_command, handle_agents_slash_command_json,
    handle_mcp_slash_command, handle_mcp_slash_command_json, handle_plugins_slash_command,
    handle_skills_slash_command, handle_skills_slash_command_json, is_stub_slash_command,
    render_slash_command_help, render_slash_command_help_filtered, resolve_skill_invocation,
    resume_supported_slash_commands, slash_command_specs, slash_command_status,
    validate_slash_command_input, SkillSlashDispatch, SlashCommand, SlashCommandStatus,
};
use compat_harness::{extract_manifest, UpstreamPaths};
use init::initialize_repo;
use plugins::{PluginHooks, PluginManager, PluginManagerConfig, PluginRegistry};
use render::{MarkdownStreamState, Spinner, TerminalRenderer};
use runtime::{
    check_base_commit, clear_oauth_credentials, format_stale_base_warning, format_usd,
    generate_pkce_pair, generate_state, load_oauth_credentials, load_system_prompt,
    parse_oauth_callback_request_target, pricing_for_model, resolve_expected_base,
    resolve_sandbox_status, save_oauth_credentials, tool_from_profile, ApiClient, ApiRequest,
    AssistantEvent, CompactionConfig, ConfigLoader, ConfigSource, ContentBlock,
    ConversationMessage, ConversationRuntime, McpServer, McpServerManager, McpServerSpec, McpTool,
    MessageRole, ModelPricing, OAuthAuthorizationRequest, OAuthConfig, OAuthTokenExchangeRequest,
    PermissionMode, PermissionPolicy, ProjectContext, PromptCacheEvent, ReasoningStep,
    RuntimeError, Session, TokenUsage, ToolError, ToolExecutor, UsageTracker,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tools::{
    execute_tool, mvp_tool_specs, GlobalToolRegistry, RuntimeToolDefinition, ToolSearchOutput,
};

const DEFAULT_MODEL: &str = "Himalaya-opus-4-6";
fn max_tokens_for_model(model: &str) -> u32 {
    if model.contains("opus") {
        32_000
    } else {
        64_000
    }
}
// Build-time constants injected by build.rs (fall back to static values when
// build.rs hasn't run, e.g. in doc-test or unusual toolchain environments).
const DEFAULT_DATE: &str = match option_env!("BUILD_DATE") {
    Some(d) => d,
    None => "unknown",
};
const STREAM_PROTOCOL_VERSION: u32 = 1;
const DEFAULT_OAUTH_CALLBACK_PORT: u16 = 4545;
const VERSION: &str = env!("CARGO_PKG_VERSION");
const BUILD_TARGET: Option<&str> = option_env!("TARGET");
const GIT_SHA: Option<&str> = option_env!("GIT_SHA");
const INTERNAL_PROGRESS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);
const POST_TOOL_STALL_TIMEOUT: Duration = Duration::from_secs(10);
const PRIMARY_SESSION_EXTENSION: &str = "jsonl";
const LEGACY_SESSION_EXTENSION: &str = "json";
const LATEST_SESSION_REFERENCE: &str = "latest";
const SESSION_REFERENCE_ALIASES: &[&str] = &[LATEST_SESSION_REFERENCE, "last", "recent"];
const CLI_OPTION_SUGGESTIONS: &[&str] = &[
    "--help",
    "-h",
    "--version",
    "-V",
    "--model",
    "--output-format",
    "--permission-mode",
    "--dangerously-skip-permissions",
    "--allowedTools",
    "--allowed-tools",
    "--resume",
    "--print",
    "--compact",
    "--base-commit",
    "-p",
];

type AllowedToolSet = BTreeSet<String>;
type RuntimePluginStateBuildOutput = (
    Option<Arc<Mutex<RuntimeMcpState>>>,
    Vec<RuntimeToolDefinition>,
);

fn main() {
    if let Err(error) = run() {
        let message = error.to_string();
        // When a machine-readable output format is active, emit errors as JSON
        // so downstream consumers can parse failures the same way they parse successes (ROADMAP #42).
        let argv: Vec<String> = std::env::args().collect();
        let json_output = argv
            .windows(2)
            .any(|w| w[0] == "--output-format" && w[1] == "json")
            || argv.iter().any(|a| a == "--output-format=json");
        let stream_json_output = argv
            .windows(2)
            .any(|w| w[0] == "--output-format" && w[1] == "stream-json")
            || argv.iter().any(|a| a == "--output-format=stream-json");
        if stream_json_output {
            eprintln!(
                "{}",
                stream_json_event(serde_json::json!({
                    "type": "error",
                    "error": message,
                }))
            );
        } else if json_output {
            eprintln!(
                "{}",
                serde_json::json!({
                    "type": "error",
                    "error": message,
                })
            );
        } else if message.contains("`Himalaya --help`") {
            eprintln!("error: {message}");
        } else {
            eprintln!(
                "error: {message}

Run `Himalaya --help` for usage."
            );
        }
        std::process::exit(1);
    }
}

fn stream_json_event(event: Value) -> Value {
    let Value::Object(mut object) = event else {
        return json!({
            "type": "error",
            "error": "stream-json event must be an object",
            "protocol_version": STREAM_PROTOCOL_VERSION,
        });
    };
    object.insert(
        "protocol_version".to_string(),
        Value::from(STREAM_PROTOCOL_VERSION),
    );
    Value::Object(object)
}

fn print_stream_json_event(event: Value) {
    println!("{}", stream_json_event(event));
}

fn is_permission_denial_output(output: &str) -> bool {
    let output = output.to_lowercase();
    if output.contains("unsupported tool") {
        return false;
    }
    ["permission", "denied", "not allowed", "requires"]
        .iter()
        .any(|keyword| output.contains(keyword))
}

fn permission_request_event(request: &runtime::PermissionRequest) -> Value {
    json!({
        "type": "permission_request",
        "tool": request.tool_name.as_str(),
        "input": request.input.as_str(),
        "current_mode": request.current_mode.as_str(),
        "required_mode": request.required_mode.as_str(),
        "reason": request.reason.as_deref().unwrap_or(""),
    })
}

fn recovery_suggestion_event(source_event: &str, tool: &str, reason: &str) -> Value {
    let (failure_class, action, suggestion) = if source_event == "permission_denial" {
        (
            "trust_gate",
            "retry_with_danger_full_access",
            "Retry with danger-full-access if you trust this request.",
        )
    } else {
        (
            "tool_runtime",
            "review_tool_error",
            "Review the tool error, adjust the request or tool input, then retry.",
        )
    };

    json!({
        "type": "recovery_suggestion",
        "source_event": source_event,
        "failure_class": failure_class,
        "tool": tool,
        "reason": reason,
        "action": action,
        "suggestion": suggestion,
    })
}

fn recovery_source_event_for_error(output: &str) -> &'static str {
    if is_permission_denial_output(output) {
        "permission_denial"
    } else {
        "tool_result"
    }
}

fn print_stream_json_tool_error(tool_name: &str, output: &str) {
    print_stream_json_event(
        json!({"type":"tool_result","name":tool_name,"output":output,"is_error":true}),
    );
    print_stream_json_event(recovery_suggestion_event(
        recovery_source_event_for_error(output),
        tool_name,
        output,
    ));
}

/// Read piped stdin content when stdin is not a terminal.
///
/// Returns `None` when stdin is attached to a terminal (interactive REPL use),
/// when reading fails, or when the piped content is empty after trimming.
/// Returns `Some(raw_content)` when a pipe delivered non-empty content.
fn read_piped_stdin() -> Option<String> {
    if io::stdin().is_terminal() {
        return None;
    }
    let mut buffer = String::new();
    if io::stdin().read_to_string(&mut buffer).is_err() {
        return None;
    }
    if buffer.trim().is_empty() {
        return None;
    }
    Some(buffer)
}

/// Merge a piped stdin payload into a prompt argument.
///
/// When `stdin_content` is `None` or empty after trimming, the prompt is
/// returned unchanged. Otherwise the trimmed stdin content is appended to the
/// prompt separated by a blank line so the model sees the prompt first and the
/// piped context immediately after it.
fn merge_prompt_with_stdin(prompt: &str, stdin_content: Option<&str>) -> String {
    let Some(raw) = stdin_content else {
        return prompt.to_string();
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return prompt.to_string();
    }
    if prompt.is_empty() {
        return trimmed.to_string();
    }
    format!("{prompt}\n\n{trimmed}")
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    match parse_args(&args)? {
        CliAction::DumpManifests { output_format } => dump_manifests(output_format)?,
        CliAction::BootstrapPlan { output_format } => print_bootstrap_plan(output_format)?,
        CliAction::MaturityMatrix { output_format } => print_maturity_matrix(output_format)?,
        CliAction::Benchmark {
            command,
            output_format,
            model,
        } => run_benchmark_command(command, output_format, &model)?,
        CliAction::Plan {
            prompt,
            output_format,
            permission_mode,
        } => run_plan_command(&prompt, output_format, permission_mode)?,
        CliAction::Agents {
            args,
            output_format,
        } => LiveCli::print_agents(args.as_deref(), output_format)?,
        CliAction::Mcp {
            args,
            output_format,
        } => LiveCli::print_mcp(args.as_deref(), output_format)?,
        CliAction::Skills {
            args,
            output_format,
        } => LiveCli::print_skills(args.as_deref(), output_format)?,
        CliAction::Tasks {
            command,
            output_format,
            model,
            allowed_tools,
            permission_mode,
            compact,
            reasoning_effort,
        } => run_task_command(
            command,
            output_format,
            model,
            allowed_tools,
            permission_mode,
            compact,
            reasoning_effort,
        )?,
        CliAction::Cron {
            command,
            output_format,
            permission_mode,
        } => run_cron_command(command, output_format, permission_mode)?,
        CliAction::Routes {
            command,
            output_format,
            model,
        } => run_route_command(command, output_format, &model)?,
        CliAction::Policy {
            command,
            output_format,
            permission_mode,
        } => run_policy_command(command, output_format, permission_mode)?,
        CliAction::Local {
            command,
            output_format,
        } => run_local_command(command, output_format)?,
        CliAction::PrintSystemPrompt {
            cwd,
            date,
            output_format,
        } => print_system_prompt(cwd, date, output_format)?,
        CliAction::Version { output_format } => print_version(output_format)?,
        CliAction::ResumeSession {
            session_path,
            commands,
            output_format,
        } => resume_session(&session_path, &commands, output_format),
        CliAction::ResumePrompt {
            session_path,
            prompt,
            model,
            output_format,
            allowed_tools,
            permission_mode,
            compact,
            base_commit,
            reasoning_effort,
            allow_broad_cwd,
            file_paths,
        } => {
            enforce_broad_cwd_policy(allow_broad_cwd, output_format)?;
            run_stale_base_preflight(base_commit.as_deref());
            let stdin_context = if matches!(permission_mode, PermissionMode::DangerFullAccess) {
                read_piped_stdin()
            } else {
                None
            };
            let effective_prompt = merge_prompt_with_stdin(&prompt, stdin_context.as_deref());
            let mut cli = LiveCli::from_existing_session(
                session_path,
                model.clone(),
                true,
                allowed_tools,
                permission_mode,
            )?;
            cli.set_reasoning_effort(reasoning_effort);
            if !file_paths.is_empty() {
                let blocks = load_files_as_content_blocks(&file_paths, &model)
                    .map_err(Box::<dyn std::error::Error>::from)?;
                cli.inject_file_blocks(blocks)?;
            }
            cli.run_turn_with_output(&effective_prompt, output_format, compact)?;
        }
        CliAction::Status {
            model,
            permission_mode,
            output_format,
        } => print_status_snapshot(&model, permission_mode, output_format)?,
        CliAction::Sandbox { output_format } => print_sandbox_status_snapshot(output_format)?,
        CliAction::Prompt {
            prompt,
            model,
            output_format,
            allowed_tools,
            permission_mode,
            compact,
            base_commit,
            reasoning_effort,
            allow_broad_cwd,
            file_paths,
        } => {
            enforce_broad_cwd_policy(allow_broad_cwd, output_format)?;
            run_stale_base_preflight(base_commit.as_deref());
            // Only consume piped stdin as prompt context when the permission
            // mode is fully unattended. In modes where the permission
            // prompter may invoke CliPermissionPrompter::decide(), stdin
            // must remain available for interactive approval; otherwise the
            // prompter's read_line() would hit EOF and deny every request.
            let stdin_context = if matches!(permission_mode, PermissionMode::DangerFullAccess) {
                read_piped_stdin()
            } else {
                None
            };
            let effective_prompt = merge_prompt_with_stdin(&prompt, stdin_context.as_deref());
            let mut cli = LiveCli::new(model.clone(), true, allowed_tools, permission_mode)?;
            cli.set_reasoning_effort(reasoning_effort);
            if !file_paths.is_empty() {
                let blocks = load_files_as_content_blocks(&file_paths, &model)
                    .map_err(Box::<dyn std::error::Error>::from)?;
                cli.inject_file_blocks(blocks)?;
            }
            cli.run_turn_with_output(&effective_prompt, output_format, compact)?;
        }
        CliAction::Login { output_format } => run_login(output_format)?,
        CliAction::Logout { output_format } => run_logout(output_format)?,
        CliAction::Doctor { output_format } => run_doctor(output_format)?,
        CliAction::State { output_format } => run_worker_state(output_format)?,
        CliAction::Workers {
            command,
            output_format,
        } => run_worker_command(command, output_format)?,
        CliAction::Init { output_format } => run_init(output_format)?,
        CliAction::Export {
            session_reference,
            output_path,
            output_format,
        } => run_export(&session_reference, output_path.as_deref(), output_format)?,
        CliAction::Repl {
            model,
            allowed_tools,
            permission_mode,
            allow_broad_cwd,
            resume_target,
        } => run_repl_ndjson(
            model,
            allowed_tools,
            permission_mode,
            allow_broad_cwd,
            resume_target,
        )?,
        CliAction::HelpTopic(topic) => print_help_topic(topic),
        CliAction::Help { output_format } => print_help(output_format)?,
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CliAction {
    DumpManifests {
        output_format: CliOutputFormat,
    },
    BootstrapPlan {
        output_format: CliOutputFormat,
    },
    MaturityMatrix {
        output_format: CliOutputFormat,
    },
    Benchmark {
        command: BenchmarkCliCommand,
        output_format: CliOutputFormat,
        model: String,
    },
    Plan {
        prompt: String,
        output_format: CliOutputFormat,
        permission_mode: PermissionMode,
    },
    Agents {
        args: Option<String>,
        output_format: CliOutputFormat,
    },
    Mcp {
        args: Option<String>,
        output_format: CliOutputFormat,
    },
    Skills {
        args: Option<String>,
        output_format: CliOutputFormat,
    },
    Tasks {
        command: TaskCliCommand,
        output_format: CliOutputFormat,
        model: String,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
        compact: bool,
        reasoning_effort: Option<String>,
    },
    Cron {
        command: CronCliCommand,
        output_format: CliOutputFormat,
        permission_mode: PermissionMode,
    },
    Routes {
        command: RouteCliCommand,
        output_format: CliOutputFormat,
        model: String,
    },
    Policy {
        command: PolicyCliCommand,
        output_format: CliOutputFormat,
        permission_mode: PermissionMode,
    },
    Local {
        command: LocalCliCommand,
        output_format: CliOutputFormat,
    },
    PrintSystemPrompt {
        cwd: PathBuf,
        date: String,
        output_format: CliOutputFormat,
    },
    Version {
        output_format: CliOutputFormat,
    },
    ResumeSession {
        session_path: PathBuf,
        commands: Vec<String>,
        output_format: CliOutputFormat,
    },
    ResumePrompt {
        session_path: PathBuf,
        prompt: String,
        model: String,
        output_format: CliOutputFormat,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
        compact: bool,
        base_commit: Option<String>,
        reasoning_effort: Option<String>,
        allow_broad_cwd: bool,
        file_paths: Vec<PathBuf>,
    },
    Status {
        model: String,
        permission_mode: PermissionMode,
        output_format: CliOutputFormat,
    },
    Sandbox {
        output_format: CliOutputFormat,
    },
    Prompt {
        prompt: String,
        model: String,
        output_format: CliOutputFormat,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
        compact: bool,
        base_commit: Option<String>,
        reasoning_effort: Option<String>,
        allow_broad_cwd: bool,
        file_paths: Vec<PathBuf>,
    },
    /// Persistent REPL mode: keep the process alive, read NDJSON prompts
    /// from stdin, and stream stream-json responses back. The session stays
    /// in memory between turns, avoiding startup overhead.
    Repl {
        model: String,
        permission_mode: PermissionMode,
        allowed_tools: Option<AllowedToolSet>,
        allow_broad_cwd: bool,
        resume_target: Option<PathBuf>,
    },
    Login {
        output_format: CliOutputFormat,
    },
    Logout {
        output_format: CliOutputFormat,
    },
    Doctor {
        output_format: CliOutputFormat,
    },
    State {
        output_format: CliOutputFormat,
    },
    Workers {
        command: WorkerCliCommand,
        output_format: CliOutputFormat,
    },
    Init {
        output_format: CliOutputFormat,
    },
    Export {
        session_reference: String,
        output_path: Option<PathBuf>,
        output_format: CliOutputFormat,
    },
    HelpTopic(LocalHelpTopic),
    // prompt-mode formatting is only supported for non-interactive runs
    Help {
        output_format: CliOutputFormat,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BenchmarkCliCommand {
    List,
    Show {
        task_id: String,
    },
    Run {
        record: bool,
        max_parallelism: usize,
    },
    Autonomous {
        record: bool,
        limit: usize,
        max_ticks: usize,
        optimize_routes: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LocalCliCommand {
    Test { filter: Option<String> },
    Lint { filter: Option<String> },
    Build { target: Option<String> },
    Review { scope: Option<String> },
    Diagnostics { path: Option<String> },
    Workspace { path: Option<PathBuf> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TaskCliCommand {
    List {
        status: Option<runtime::TaskStatus>,
    },
    Show {
        task_id: String,
    },
    Status {
        task_id: String,
    },
    Report {
        task_id: String,
    },
    Review {
        task_id: String,
    },
    Packet {
        command: TaskPacketCliCommand,
    },
    Scheduler {
        command: TaskSchedulerCliCommand,
    },
    Daemon {
        command: TaskDaemonCliCommand,
    },
    Resume {
        task_id: String,
        from_node: Option<String>,
        prompt: Option<String>,
    },
    Retry {
        task_id: String,
        node_id: String,
    },
    Verify {
        task_id: String,
        node_id: String,
        command: String,
    },
    Execute {
        task_id: String,
        from_node: Option<String>,
    },
    Recover {
        task_id: String,
    },
    VerifyTask {
        task_id: String,
    },
    Compact {
        task_id: String,
        keep_last: usize,
    },
    Cancel {
        task_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TaskPacketCliCommand {
    Create { path: PathBuf },
    Run { path: PathBuf },
    Status { task_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TaskSchedulerCliCommand {
    Tick,
    Queue,
    Explain { task_id: String },
    Run { max_ticks: usize },
    Status,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TaskDaemonCliCommand {
    Start { max_ticks: usize },
    Status,
    Stop,
    Logs { limit: usize },
    Report { limit: usize, max_ticks: usize },
    Evaluate { limit: usize, max_ticks: usize },
    Replay { limit: usize, max_ticks: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkerCliCommand {
    List,
    Create {
        cwd: Option<PathBuf>,
        trusted_roots: Vec<String>,
    },
    Spawn {
        cwd: Option<PathBuf>,
        trusted_roots: Vec<String>,
        isolate_worktree: bool,
        worktree_root: Option<PathBuf>,
        command: Vec<String>,
    },
    Probe {
        worker_id: String,
    },
    Observe {
        worker_id: String,
        screen_text: String,
    },
    Ready {
        worker_id: String,
    },
    ResolveTrust {
        worker_id: String,
    },
    Prompt {
        worker_id: String,
        prompt: Option<String>,
    },
    Complete {
        worker_id: String,
        finish_reason: String,
        tokens_output: u64,
    },
    Restart {
        worker_id: String,
    },
    Terminate {
        worker_id: String,
    },
    Cleanup {
        include_stale: bool,
    },
    Supervise,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CronCliCommand {
    List,
    Add {
        schedule: String,
        prompt: String,
        description: Option<String>,
    },
    Remove {
        cron_id: String,
    },
    /// Fire all cron entries due now (single tick). `max_fires` bounds how many
    /// prompts run this invocation as a runaway guard.
    Run {
        max_fires: usize,
        max_ticks: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RouteCliCommand {
    FeedbackSummary,
    List,
    Optimize {
        min_samples: usize,
        threshold_percent: u8,
    },
    Replay {
        min_samples: usize,
        threshold_percent: u8,
    },
    Propose {
        min_samples: usize,
        threshold_percent: u8,
    },
    Apply {
        proposal_id: String,
        dry_run: bool,
    },
    Rollback {
        proposal_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PolicyCliCommand {
    Review {
        limit: usize,
        max_ticks: usize,
        record: bool,
    },
    Ledger {
        limit: usize,
    },
    Replay {
        limit: usize,
    },
    Plan {
        limit: usize,
        max_ticks: usize,
    },
    Apply {
        limit: usize,
        max_ticks: usize,
        domain: Option<runtime::PolicyDomain>,
        proposal_id: Option<String>,
        dry_run: bool,
    },
    Rollback {
        limit: usize,
        max_ticks: usize,
        domain: Option<runtime::PolicyDomain>,
        proposal_id: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalHelpTopic {
    Status,
    Sandbox,
    Doctor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliOutputFormat {
    Text,
    Json,
    StreamJson,
}

impl CliOutputFormat {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            "stream-json" => Ok(Self::StreamJson),
            other => Err(format!(
                "unsupported value for --output-format: {other} (expected text, json, or stream-json)"
            )),
        }
    }
}

fn worker_spawn_command_delimiter_seen(rest: &[String]) -> bool {
    matches!(
        rest,
        [command, subcommand, args @ ..]
            if matches!(command.as_str(), "workers" | "worker")
                && subcommand == "spawn"
                && args.iter().any(|arg| arg == "--")
    )
}

#[allow(clippy::too_many_lines)]
fn parse_args(args: &[String]) -> Result<CliAction, String> {
    let mut model = DEFAULT_MODEL.to_string();
    let mut output_format = CliOutputFormat::Text;
    let mut permission_mode_override = None;
    let mut repl_mode = false;
    let mut force_new_session = false;
    let mut wants_help = false;
    let mut wants_version = false;
    let mut allowed_tool_values = Vec::new();
    let mut compact = false;
    let mut base_commit: Option<String> = None;
    let mut reasoning_effort: Option<String> = None;
    let mut allow_broad_cwd = false;
    let mut file_paths: Vec<PathBuf> = Vec::new();
    let mut rest: Vec<String> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if worker_spawn_command_delimiter_seen(&rest) {
            rest.push(args[index].clone());
            index += 1;
            continue;
        }

        match args[index].as_str() {
            "--help" | "-h" if rest.is_empty() => {
                wants_help = true;
                index += 1;
            }
            "--help" | "-h"
                if !rest.is_empty()
                    && matches!(
                        rest[0].as_str(),
                        "prompt"
                            | "login"
                            | "logout"
                            | "version"
                            | "state"
                            | "workers"
                            | "init"
                            | "export"
                            | "commit"
                            | "pr"
                            | "issue"
                    ) =>
            {
                // `--help` following a subcommand that would otherwise forward
                // the arg to the API (e.g. `Himalaya prompt --help`) should show
                // top-level help instead. Subcommands that consume their own
                // args (agents, mcp, plugins, skills) and local help-topic
                // subcommands (status, sandbox, doctor) must NOT be intercepted
                // here — they handle --help in their own dispatch paths.
                wants_help = true;
                index += 1;
            }
            "--version" | "-V" => {
                wants_version = true;
                index += 1;
            }
            "--repl" => {
                repl_mode = true;
                index += 1;
            }
            "--new" | "--new-session" => {
                force_new_session = true;
                index += 1;
            }
            "--model" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --model".to_string())?;
                model = resolve_model_alias_with_config(value);
                index += 2;
            }
            flag if flag.starts_with("--model=") => {
                model = resolve_model_alias_with_config(&flag[8..]);
                index += 1;
            }
            "--output-format" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --output-format".to_string())?;
                output_format = CliOutputFormat::parse(value)?;
                index += 2;
            }
            "--permission-mode" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --permission-mode".to_string())?;
                permission_mode_override = Some(parse_permission_mode_arg(value)?);
                index += 2;
            }
            flag if flag.starts_with("--output-format=") => {
                output_format = CliOutputFormat::parse(&flag[16..])?;
                index += 1;
            }
            flag if flag.starts_with("--permission-mode=") => {
                permission_mode_override = Some(parse_permission_mode_arg(&flag[18..])?);
                index += 1;
            }
            "--dangerously-skip-permissions" => {
                permission_mode_override = Some(PermissionMode::DangerFullAccess);
                index += 1;
            }
            "--compact" => {
                compact = true;
                index += 1;
            }
            "--base-commit" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --base-commit".to_string())?;
                base_commit = Some(value.clone());
                index += 2;
            }
            flag if flag.starts_with("--base-commit=") => {
                base_commit = Some(flag[14..].to_string());
                index += 1;
            }
            "--reasoning-effort" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --reasoning-effort".to_string())?;
                if !matches!(value.as_str(), "low" | "medium" | "high") {
                    return Err(format!(
                        "invalid value for --reasoning-effort: '{value}'; must be low, medium, or high"
                    ));
                }
                reasoning_effort = Some(value.clone());
                index += 2;
            }
            flag if flag.starts_with("--reasoning-effort=") => {
                let value = &flag[19..];
                if !matches!(value, "low" | "medium" | "high") {
                    return Err(format!(
                        "invalid value for --reasoning-effort: '{value}'; must be low, medium, or high"
                    ));
                }
                reasoning_effort = Some(value.to_string());
                index += 1;
            }
            "--allow-broad-cwd" => {
                allow_broad_cwd = true;
                index += 1;
            }
            "--file" => {
                let path = args
                    .get(index + 1)
                    .ok_or_else(|| "--file requires a path argument".to_string())?;
                file_paths.push(PathBuf::from(path));
                index += 2;
            }
            flag if flag.starts_with("--file=") => {
                file_paths.push(PathBuf::from(&flag[7..]));
                index += 1;
            }
            "-p" => {
                // Himalaya Code compat: -p "prompt" = one-shot prompt
                let prompt = args[index + 1..].join(" ");
                if prompt.trim().is_empty() {
                    return Err("-p requires a prompt string".to_string());
                }
                return Ok(CliAction::Prompt {
                    prompt,
                    model: resolve_model_alias_with_config(&model),
                    output_format,
                    allowed_tools: normalize_allowed_tools(&allowed_tool_values)?,
                    permission_mode: permission_mode_override
                        .unwrap_or_else(default_permission_mode),
                    compact,
                    base_commit: base_commit.clone(),
                    reasoning_effort: reasoning_effort.clone(),
                    allow_broad_cwd,
                    file_paths: file_paths.clone(),
                });
            }
            "--print" => {
                // Himalaya Code compat: --print makes output non-interactive
                output_format = CliOutputFormat::Text;
                index += 1;
            }
            "--resume" if rest.is_empty() => {
                rest.push("--resume".to_string());
                index += 1;
            }
            flag if rest.is_empty() && flag.starts_with("--resume=") => {
                rest.push("--resume".to_string());
                rest.push(flag[9..].to_string());
                index += 1;
            }
            "--allowedTools" | "--allowed-tools" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --allowedTools".to_string())?;
                allowed_tool_values.push(value.clone());
                index += 2;
            }
            flag if flag.starts_with("--allowedTools=") => {
                allowed_tool_values.push(flag[15..].to_string());
                index += 1;
            }
            flag if flag.starts_with("--allowed-tools=") => {
                allowed_tool_values.push(flag[16..].to_string());
                index += 1;
            }
            other if rest.is_empty() && other.starts_with('-') => {
                return Err(format_unknown_option(other))
            }
            other => {
                rest.push(other.to_string());
                index += 1;
            }
        }
    }

    if wants_help {
        return Ok(CliAction::Help { output_format });
    }

    if wants_version {
        return Ok(CliAction::Version { output_format });
    }

    let allowed_tools = normalize_allowed_tools(&allowed_tool_values)?;
    let file_paths = file_paths.clone();

    if repl_mode {
        let permission_mode = permission_mode_override.unwrap_or_else(default_permission_mode);
        let resume_target = match rest.as_slice() {
            // No explicit --resume: auto-resume the latest workspace session for
            // conversational continuity, unless --new was passed. `Some(latest)`
            // resolution falls back to a fresh session when none exists yet.
            [] if force_new_session => None,
            [] => Some(PathBuf::from(LATEST_SESSION_REFERENCE)),
            [flag] if flag == "--resume" => Some(PathBuf::from(LATEST_SESSION_REFERENCE)),
            [flag, target] if flag == "--resume" => Some(PathBuf::from(target)),
            [flag, ..] if flag == "--resume" => {
                return Err(
                    "--repl --resume accepts only an optional session reference".to_string()
                );
            }
            _ => return Err("--repl accepts only global flags and optional --resume".to_string()),
        };
        return Ok(CliAction::Repl {
            model,
            allowed_tools,
            permission_mode,
            allow_broad_cwd,
            resume_target,
        });
    }

    if rest.is_empty() {
        let permission_mode = permission_mode_override.unwrap_or_else(default_permission_mode);
        // When stdin is not a terminal (pipe/redirect) and no prompt is given on the
        // command line, read stdin as the prompt and dispatch as a one-shot Prompt
        // rather than starting the interactive REPL (which would consume the pipe and
        // print the startup banner, then exit without sending anything to the API).
        if !std::io::stdin().is_terminal() {
            let mut buf = String::new();
            let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf);
            let piped = buf.trim().to_string();
            if !piped.is_empty() {
                return Ok(CliAction::Prompt {
                    model,
                    prompt: piped,
                    allowed_tools,
                    permission_mode,
                    output_format,
                    compact: false,
                    base_commit,
                    reasoning_effort,
                    allow_broad_cwd,
                    file_paths: file_paths.clone(),
                });
            }
        }
        return Ok(CliAction::Repl {
            model,
            allowed_tools,
            permission_mode,
            allow_broad_cwd,
            // Bare interactive launch: auto-resume the latest workspace session
            // for continuity (falls back to fresh when none / when --new given).
            resume_target: if force_new_session {
                None
            } else {
                Some(PathBuf::from(LATEST_SESSION_REFERENCE))
            },
        });
    }
    if rest.first().map(String::as_str) == Some("--resume") {
        if let Some((session_path, prompt)) = parse_resume_prompt_args(&rest[1..])? {
            return Ok(CliAction::ResumePrompt {
                session_path,
                prompt,
                model,
                output_format,
                allowed_tools,
                permission_mode: permission_mode_override.unwrap_or_else(default_permission_mode),
                compact,
                base_commit,
                reasoning_effort: reasoning_effort.clone(),
                allow_broad_cwd,
                file_paths: file_paths.clone(),
            });
        }
        return parse_resume_args(&rest[1..], output_format);
    }
    if let Some(action) = parse_local_help_action(&rest) {
        return action;
    }
    if let Some(action) =
        parse_single_word_command_alias(&rest, &model, permission_mode_override, output_format)
    {
        return action;
    }

    let permission_mode = permission_mode_override.unwrap_or_else(default_permission_mode);

    match rest[0].as_str() {
        "dump-manifests" => Ok(CliAction::DumpManifests { output_format }),
        "bootstrap-plan" => Ok(CliAction::BootstrapPlan { output_format }),
        "maturity-matrix" | "maturity" => Ok(CliAction::MaturityMatrix { output_format }),
        "benchmark" => Ok(CliAction::Benchmark {
            command: parse_benchmark_cli_command(&rest[1..])?,
            output_format,
            model: model.clone(),
        }),
        "plan" => {
            let prompt = rest[1..].join(" ");
            if prompt.trim().is_empty() {
                return Err("plan command requires a task description".to_string());
            }
            Ok(CliAction::Plan {
                prompt,
                output_format,
                permission_mode,
            })
        }
        "agents" => Ok(CliAction::Agents {
            args: join_optional_args(&rest[1..]),
            output_format,
        }),
        "mcp" => Ok(CliAction::Mcp {
            args: join_optional_args(&rest[1..]),
            output_format,
        }),
        "skills" => {
            let args = join_optional_args(&rest[1..]);
            match classify_skills_slash_command(args.as_deref()) {
                SkillSlashDispatch::Invoke(prompt) => Ok(CliAction::Prompt {
                    prompt,
                    model,
                    output_format,
                    allowed_tools,
                    permission_mode,
                    compact,
                    base_commit,
                    reasoning_effort: reasoning_effort.clone(),
                    allow_broad_cwd,
                    file_paths: file_paths.clone(),
                }),
                SkillSlashDispatch::Local => Ok(CliAction::Skills {
                    args,
                    output_format,
                }),
            }
        }
        "tasks" => Ok(CliAction::Tasks {
            command: parse_task_cli_command(&rest[1..])?,
            output_format,
            model,
            allowed_tools,
            permission_mode,
            compact,
            reasoning_effort: reasoning_effort.clone(),
        }),
        "cron" => Ok(CliAction::Cron {
            command: parse_cron_cli_command(&rest[1..])?,
            output_format,
            permission_mode: permission_mode_override.unwrap_or(PermissionMode::ReadOnly),
        }),
        "routes" | "route" => Ok(CliAction::Routes {
            command: parse_route_cli_command(&rest[1..])?,
            output_format,
            model,
        }),
        "policy" | "policies" => Ok(CliAction::Policy {
            command: parse_policy_cli_command(&rest[1..])?,
            output_format,
            permission_mode,
        }),
        "test" | "lint" | "build" | "review" | "diagnostics" | "workspace" | "cwd" => {
            Ok(CliAction::Local {
                command: parse_local_cli_command(&rest)?,
                output_format,
            })
        }
        "workers" | "worker" => Ok(CliAction::Workers {
            command: parse_worker_cli_command(&rest[1..])?,
            output_format,
        }),
        "system-prompt" => parse_system_prompt_args(&rest[1..], output_format),
        "login" => Ok(CliAction::Login { output_format }),
        "logout" => Ok(CliAction::Logout { output_format }),
        "init" => Ok(CliAction::Init { output_format }),
        "export" => parse_export_args(&rest[1..], output_format),
        "prompt" => {
            let prompt = rest[1..].join(" ");
            if prompt.trim().is_empty() {
                return Err("prompt subcommand requires a prompt string".to_string());
            }
            Ok(CliAction::Prompt {
                prompt,
                model,
                output_format,
                allowed_tools,
                permission_mode,
                compact,
                base_commit: base_commit.clone(),
                reasoning_effort: reasoning_effort.clone(),
                allow_broad_cwd,
                file_paths: file_paths.clone(),
            })
        }
        other if other.starts_with('/') => parse_direct_slash_cli_action(
            &rest,
            model,
            output_format,
            allowed_tools,
            permission_mode,
            compact,
            base_commit,
            reasoning_effort,
            &file_paths,
            allow_broad_cwd,
        ),
        _other => Ok(CliAction::Prompt {
            prompt: rest.join(" "),
            model,
            output_format,
            allowed_tools,
            permission_mode,
            compact,
            base_commit,
            reasoning_effort: reasoning_effort.clone(),
            allow_broad_cwd,
            file_paths: file_paths.clone(),
        }),
    }
}

fn parse_local_cli_command(args: &[String]) -> Result<LocalCliCommand, String> {
    let Some((command, rest)) = args.split_first() else {
        return Err("local command is missing".to_string());
    };
    parse_local_cli_command_from_parts(command, join_optional_args(rest).as_deref())
}

fn parse_local_cli_command_from_parts(
    command: &str,
    args: Option<&str>,
) -> Result<LocalCliCommand, String> {
    let value = args.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    });
    match command {
        "test" => Ok(LocalCliCommand::Test { filter: value }),
        "lint" => Ok(LocalCliCommand::Lint { filter: value }),
        "build" => Ok(LocalCliCommand::Build { target: value }),
        "review" => Ok(LocalCliCommand::Review { scope: value }),
        "diagnostics" => Ok(LocalCliCommand::Diagnostics { path: value }),
        "workspace" | "cwd" => Ok(LocalCliCommand::Workspace {
            path: value.map(PathBuf::from),
        }),
        other => Err(format!("unknown local command: {other}")),
    }
}

fn parse_benchmark_cli_command(args: &[String]) -> Result<BenchmarkCliCommand, String> {
    match args
        .split_first()
        .map(|(command, rest)| (command.as_str(), rest))
    {
        None | Some(("list", [])) => Ok(BenchmarkCliCommand::List),
        Some(("show", [task_id])) => Ok(BenchmarkCliCommand::Show {
            task_id: task_id.clone(),
        }),
        Some(("run", rest)) => parse_benchmark_run_args(rest),
        Some(("autonomous", rest)) => parse_benchmark_autonomous_args(rest),
        Some(("list" | "show", _)) => Err(
            "Usage: Himalaya benchmark [list|show <task-id>|run [--record] [--max-parallelism N]|autonomous [--record] [--limit N] [--max-ticks N]]"
                .to_string(),
        ),
        Some((other, _)) => Err(format!(
            "unknown benchmark command: {other}\nUsage: Himalaya benchmark [list|show <task-id>|run [--record] [--max-parallelism N]|autonomous [--record] [--limit N] [--max-ticks N]]"
        )),
    }
}

fn parse_benchmark_run_args(args: &[String]) -> Result<BenchmarkCliCommand, String> {
    let mut record = false;
    let mut max_parallelism = 4_usize;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--record" => {
                record = true;
                index += 1;
            }
            "--max-parallelism" => {
                let value = args.get(index + 1).ok_or_else(|| {
                    "benchmark run --max-parallelism requires a value".to_string()
                })?;
                max_parallelism = parse_benchmark_parallelism(value)?;
                index += 2;
            }
            value if value.starts_with("--max-parallelism=") => {
                max_parallelism = parse_benchmark_parallelism(&value[18..])?;
                index += 1;
            }
            other => {
                return Err(format!(
                    "unknown benchmark run argument: {other}\nUsage: Himalaya benchmark run [--record] [--max-parallelism N]"
                ));
            }
        }
    }
    Ok(BenchmarkCliCommand::Run {
        record,
        max_parallelism,
    })
}

fn parse_benchmark_autonomous_args(args: &[String]) -> Result<BenchmarkCliCommand, String> {
    let mut record = false;
    let mut limit = 20_usize;
    let mut max_ticks = 1_usize;
    let mut optimize_routes = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--record" => {
                record = true;
                index += 1;
            }
            "--limit" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "benchmark autonomous --limit requires a value".to_string())?;
                limit = parse_positive_usize("--limit", value)?;
                index += 2;
            }
            value if value.starts_with("--limit=") => {
                limit = parse_positive_usize("--limit", &value[8..])?;
                index += 1;
            }
            "--max-ticks" => {
                let value = args.get(index + 1).ok_or_else(|| {
                    "benchmark autonomous --max-ticks requires a value".to_string()
                })?;
                max_ticks = parse_positive_usize("--max-ticks", value)?;
                index += 2;
            }
            value if value.starts_with("--max-ticks=") => {
                max_ticks = parse_positive_usize("--max-ticks", &value[12..])?;
                index += 1;
            }
            "--once" => {
                max_ticks = 1;
                index += 1;
            }
            "--optimize-routes" => {
                optimize_routes = true;
                index += 1;
            }
            other => {
                return Err(format!(
                    "unknown benchmark autonomous argument: {other}\nUsage: Himalaya benchmark autonomous [--record] [--limit N] [--once|--max-ticks N] [--optimize-routes]"
                ));
            }
        }
    }
    Ok(BenchmarkCliCommand::Autonomous {
        record,
        limit,
        max_ticks,
        optimize_routes,
    })
}

fn parse_benchmark_parallelism(value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("invalid --max-parallelism value: {value}"))?;
    if parsed == 0 {
        return Err("--max-parallelism must be greater than 0".to_string());
    }
    Ok(parsed)
}

fn parse_route_cli_command(args: &[String]) -> Result<RouteCliCommand, String> {
    match args
        .split_first()
        .map(|(command, rest)| (command.as_str(), rest))
    {
        None | Some(("feedback" | "feedback-summary" | "summary", [])) => {
            Ok(RouteCliCommand::FeedbackSummary)
        }
        Some(("feedback", rest)) => match rest {
            [subcommand] if matches!(subcommand.as_str(), "summary" | "summaries") => {
                Ok(RouteCliCommand::FeedbackSummary)
            }
            _ => Err(route_usage().to_string()),
        },
        Some(("list", [])) => Ok(RouteCliCommand::List),
        Some(("optimize", rest)) => {
            parse_route_optimizer_args(rest, "optimize").map(|(min_samples, threshold_percent)| {
                RouteCliCommand::Optimize {
                    min_samples,
                    threshold_percent,
                }
            })
        }
        Some(("replay", rest)) => {
            parse_route_optimizer_args(rest, "replay").map(|(min_samples, threshold_percent)| {
                RouteCliCommand::Replay {
                    min_samples,
                    threshold_percent,
                }
            })
        }
        Some(("propose", rest)) => {
            parse_route_optimizer_args(rest, "propose").map(|(min_samples, threshold_percent)| {
                RouteCliCommand::Propose {
                    min_samples,
                    threshold_percent,
                }
            })
        }
        Some(("apply", rest)) => parse_route_apply_args(rest),
        Some(("rollback", rest)) => parse_route_rollback_args(rest),
        Some((other, _)) => Err(format!(
            "unknown routes command: {other}\n{}",
            route_usage()
        )),
    }
}

fn route_usage() -> &'static str {
    "Usage: Himalaya routes [feedback summary|feedback-summary|summary|list|optimize [--min-samples N] [--threshold-percent N]|replay [--min-samples N] [--threshold-percent N]|propose [--min-samples N] [--threshold-percent N]|apply <proposal-id> [--dry-run]|rollback <proposal-id>]"
}

fn parse_route_optimizer_args(args: &[String], command: &str) -> Result<(usize, u8), String> {
    let mut min_samples = 2_usize;
    let mut threshold_percent = 50_u8;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--min-samples" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("routes {command} --min-samples requires a value"))?;
                min_samples = parse_positive_usize("--min-samples", value)?;
                index += 2;
            }
            value if value.starts_with("--min-samples=") => {
                min_samples = parse_positive_usize("--min-samples", &value[14..])?;
                index += 1;
            }
            "--threshold-percent" => {
                let value = args.get(index + 1).ok_or_else(|| {
                    format!("routes {command} --threshold-percent requires a value")
                })?;
                threshold_percent = parse_percent_u8("--threshold-percent", value)?;
                index += 2;
            }
            value if value.starts_with("--threshold-percent=") => {
                threshold_percent = parse_percent_u8("--threshold-percent", &value[20..])?;
                index += 1;
            }
            other => {
                return Err(format!(
                    "unknown routes {command} argument: {other}\nUsage: Himalaya routes {command} [--min-samples N] [--threshold-percent N]"
                ));
            }
        }
    }
    Ok((min_samples, threshold_percent))
}

fn parse_route_apply_args(args: &[String]) -> Result<RouteCliCommand, String> {
    let mut proposal_id = None;
    let mut dry_run = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            value if value.starts_with("--") => {
                return Err(format!(
                    "unknown routes apply argument: {value}\nUsage: Himalaya routes apply <proposal-id> [--dry-run]"
                ));
            }
            value => {
                if proposal_id.is_some() {
                    return Err(
                        "routes apply accepts one proposal id\nUsage: Himalaya routes apply <proposal-id> [--dry-run]"
                            .to_string(),
                    );
                }
                proposal_id = Some(value.to_string());
                index += 1;
            }
        }
    }
    let proposal_id =
        proposal_id.ok_or_else(|| "routes apply requires a proposal id".to_string())?;
    Ok(RouteCliCommand::Apply {
        proposal_id,
        dry_run,
    })
}

fn parse_route_rollback_args(args: &[String]) -> Result<RouteCliCommand, String> {
    match args {
        [proposal_id] => Ok(RouteCliCommand::Rollback {
            proposal_id: proposal_id.clone(),
        }),
        [] => Err("routes rollback requires a proposal id".to_string()),
        _ => Err(
            "routes rollback accepts one proposal id\nUsage: Himalaya routes rollback <proposal-id>"
                .to_string(),
        ),
    }
}

fn parse_percent_u8(name: &str, value: &str) -> Result<u8, String> {
    let parsed = value
        .parse::<u8>()
        .map_err(|_| format!("invalid {name} value: {value}"))?;
    if parsed > 100 {
        return Err(format!("{name} must be between 0 and 100"));
    }
    Ok(parsed)
}

fn parse_policy_cli_command(args: &[String]) -> Result<PolicyCliCommand, String> {
    match args.split_first().map(|(command, rest)| (command.as_str(), rest)) {
        None | Some(("review", [])) => Ok(PolicyCliCommand::Review {
            limit: 20,
            max_ticks: 1,
            record: true,
        }),
        Some(("review", rest)) => parse_policy_review_args(rest),
        Some(("ledger" | "log", rest)) => parse_policy_ledger_args(rest),
        Some(("replay", rest)) => parse_policy_replay_args(rest),
        Some(("plan", rest)) => parse_policy_plan_args(rest),
        Some(("apply", rest)) => parse_policy_apply_args(rest),
        Some(("rollback", rest)) => parse_policy_rollback_args(rest),
        Some((other, _)) => Err(format!(
            "unknown policy command: {other}\nUsage: Himalaya policy [review [--limit N] [--max-ticks N] [--no-record]|ledger [--limit N]|replay [--limit N]|plan [--limit N] [--max-ticks N]|apply [--dry-run] [--domain routing|scheduler|memory|recovery] [--proposal-id ID] [--limit N] [--max-ticks N]|rollback [--domain routing] [--proposal-id ID] [--limit N] [--max-ticks N]]"
        )),
    }
}

fn parse_policy_review_args(args: &[String]) -> Result<PolicyCliCommand, String> {
    let mut limit = 20_usize;
    let mut max_ticks = 1_usize;
    let mut record = true;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--limit" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "policy review --limit requires a value".to_string())?;
                limit = parse_positive_usize("--limit", value)?;
                index += 2;
            }
            value if value.starts_with("--limit=") => {
                limit = parse_positive_usize("--limit", &value[8..])?;
                index += 1;
            }
            "--max-ticks" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "policy review --max-ticks requires a value".to_string())?;
                max_ticks = parse_positive_usize("--max-ticks", value)?;
                index += 2;
            }
            value if value.starts_with("--max-ticks=") => {
                max_ticks = parse_positive_usize("--max-ticks", &value[12..])?;
                index += 1;
            }
            "--no-record" => {
                record = false;
                index += 1;
            }
            other => {
                return Err(format!(
                    "unknown policy review argument: {other}\nUsage: Himalaya policy review [--limit N] [--max-ticks N] [--no-record]"
                ));
            }
        }
    }
    Ok(PolicyCliCommand::Review {
        limit,
        max_ticks,
        record,
    })
}

fn parse_policy_ledger_args(args: &[String]) -> Result<PolicyCliCommand, String> {
    let mut limit = 20_usize;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--limit" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "policy ledger --limit requires a value".to_string())?;
                limit = parse_positive_usize("--limit", value)?;
                index += 2;
            }
            value if value.starts_with("--limit=") => {
                limit = parse_positive_usize("--limit", &value[8..])?;
                index += 1;
            }
            other => {
                return Err(format!(
                    "unknown policy ledger argument: {other}\nUsage: Himalaya policy ledger [--limit N]"
                ));
            }
        }
    }
    Ok(PolicyCliCommand::Ledger { limit })
}

fn parse_policy_replay_args(args: &[String]) -> Result<PolicyCliCommand, String> {
    let mut limit = 100_usize;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--limit" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "policy replay --limit requires a value".to_string())?;
                limit = parse_positive_usize("--limit", value)?;
                index += 2;
            }
            value if value.starts_with("--limit=") => {
                limit = parse_positive_usize("--limit", &value[8..])?;
                index += 1;
            }
            other => {
                return Err(format!(
                    "unknown policy replay argument: {other}\nUsage: Himalaya policy replay [--limit N]"
                ));
            }
        }
    }
    Ok(PolicyCliCommand::Replay { limit })
}

fn parse_policy_plan_args(args: &[String]) -> Result<PolicyCliCommand, String> {
    let (limit, max_ticks) = parse_policy_limit_max_ticks(args, "policy plan")?;
    Ok(PolicyCliCommand::Plan { limit, max_ticks })
}

fn parse_policy_apply_args(args: &[String]) -> Result<PolicyCliCommand, String> {
    let mut limit = 20_usize;
    let mut max_ticks = 1_usize;
    let mut domain = None;
    let mut proposal_id = None;
    let mut dry_run = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--limit" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "policy apply --limit requires a value".to_string())?;
                limit = parse_positive_usize("--limit", value)?;
                index += 2;
            }
            value if value.starts_with("--limit=") => {
                limit = parse_positive_usize("--limit", &value[8..])?;
                index += 1;
            }
            "--max-ticks" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "policy apply --max-ticks requires a value".to_string())?;
                max_ticks = parse_positive_usize("--max-ticks", value)?;
                index += 2;
            }
            value if value.starts_with("--max-ticks=") => {
                max_ticks = parse_positive_usize("--max-ticks", &value[12..])?;
                index += 1;
            }
            "--domain" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "policy apply --domain requires a value".to_string())?;
                domain = Some(parse_policy_domain(value)?);
                index += 2;
            }
            value if value.starts_with("--domain=") => {
                domain = Some(parse_policy_domain(&value[9..])?);
                index += 1;
            }
            "--proposal-id" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "policy apply --proposal-id requires a value".to_string())?;
                proposal_id = Some(value.clone());
                index += 2;
            }
            value if value.starts_with("--proposal-id=") => {
                proposal_id = Some(value[14..].to_string());
                index += 1;
            }
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            other => {
                return Err(format!(
                    "unknown policy apply argument: {other}\nUsage: Himalaya policy apply [--dry-run] [--domain routing|scheduler|memory|recovery] [--proposal-id ID] [--limit N] [--max-ticks N]"
                ));
            }
        }
    }
    Ok(PolicyCliCommand::Apply {
        limit,
        max_ticks,
        domain,
        proposal_id,
        dry_run,
    })
}

fn parse_policy_rollback_args(args: &[String]) -> Result<PolicyCliCommand, String> {
    let mut limit = 20_usize;
    let mut max_ticks = 1_usize;
    let mut domain = None;
    let mut proposal_id = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--limit" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "policy rollback --limit requires a value".to_string())?;
                limit = parse_positive_usize("--limit", value)?;
                index += 2;
            }
            value if value.starts_with("--limit=") => {
                limit = parse_positive_usize("--limit", &value[8..])?;
                index += 1;
            }
            "--max-ticks" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "policy rollback --max-ticks requires a value".to_string())?;
                max_ticks = parse_positive_usize("--max-ticks", value)?;
                index += 2;
            }
            value if value.starts_with("--max-ticks=") => {
                max_ticks = parse_positive_usize("--max-ticks", &value[12..])?;
                index += 1;
            }
            "--domain" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "policy rollback --domain requires a value".to_string())?;
                domain = Some(parse_policy_domain(value)?);
                index += 2;
            }
            value if value.starts_with("--domain=") => {
                domain = Some(parse_policy_domain(&value[9..])?);
                index += 1;
            }
            "--proposal-id" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "policy rollback --proposal-id requires a value".to_string())?;
                proposal_id = Some(value.clone());
                index += 2;
            }
            value if value.starts_with("--proposal-id=") => {
                proposal_id = Some(value[14..].to_string());
                index += 1;
            }
            other => {
                return Err(format!(
                    "unknown policy rollback argument: {other}\nUsage: Himalaya policy rollback [--domain routing] [--proposal-id ID] [--limit N] [--max-ticks N]"
                ));
            }
        }
    }
    Ok(PolicyCliCommand::Rollback {
        limit,
        max_ticks,
        domain,
        proposal_id,
    })
}

fn parse_policy_limit_max_ticks(
    args: &[String],
    usage_command: &str,
) -> Result<(usize, usize), String> {
    let mut limit = 20_usize;
    let mut max_ticks = 1_usize;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--limit" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("{usage_command} --limit requires a value"))?;
                limit = parse_positive_usize("--limit", value)?;
                index += 2;
            }
            value if value.starts_with("--limit=") => {
                limit = parse_positive_usize("--limit", &value[8..])?;
                index += 1;
            }
            "--max-ticks" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("{usage_command} --max-ticks requires a value"))?;
                max_ticks = parse_positive_usize("--max-ticks", value)?;
                index += 2;
            }
            value if value.starts_with("--max-ticks=") => {
                max_ticks = parse_positive_usize("--max-ticks", &value[12..])?;
                index += 1;
            }
            other => {
                return Err(format!(
                    "unknown {usage_command} argument: {other}\nUsage: Himalaya {usage_command} [--limit N] [--max-ticks N]"
                ));
            }
        }
    }
    Ok((limit, max_ticks))
}

fn parse_policy_domain(value: &str) -> Result<runtime::PolicyDomain, String> {
    match value {
        "routing" | "route" | "routes" => Ok(runtime::PolicyDomain::Routing),
        "autonomous_run" | "autonomous-run" | "autonomous" => {
            Ok(runtime::PolicyDomain::AutonomousRun)
        }
        "scheduler" => Ok(runtime::PolicyDomain::Scheduler),
        "memory" => Ok(runtime::PolicyDomain::Memory),
        "recovery" => Ok(runtime::PolicyDomain::Recovery),
        other => Err(format!("unknown policy domain: {other}")),
    }
}

fn parse_task_status(value: &str) -> Result<runtime::TaskStatus, String> {
    match value {
        "created" => Ok(runtime::TaskStatus::Created),
        "planning" => Ok(runtime::TaskStatus::Planning),
        "running" => Ok(runtime::TaskStatus::Running),
        "waiting_for_permission" => Ok(runtime::TaskStatus::WaitingForPermission),
        "waiting_for_verification" => Ok(runtime::TaskStatus::WaitingForVerification),
        "recovering" => Ok(runtime::TaskStatus::Recovering),
        "blocked" => Ok(runtime::TaskStatus::Blocked),
        "completed" => Ok(runtime::TaskStatus::Completed),
        "failed" => Ok(runtime::TaskStatus::Failed),
        "stopped" => Ok(runtime::TaskStatus::Stopped),
        "cancelled" => Ok(runtime::TaskStatus::Cancelled),
        other => Err(format!("unknown task status: {other}")),
    }
}

fn parse_task_cli_command(args: &[String]) -> Result<TaskCliCommand, String> {
    match args.split_first().map(|(command, rest)| (command.as_str(), rest)) {
        None | Some(("list", [])) => Ok(TaskCliCommand::List { status: None }),
        Some(("list", [flag, status])) if flag == "--status" => Ok(TaskCliCommand::List {
            status: Some(parse_task_status(status)?),
        }),
        Some(("show", [task_id])) => Ok(TaskCliCommand::Show {
            task_id: task_id.clone(),
        }),
        Some(("status", [task_id])) => Ok(TaskCliCommand::Status {
            task_id: task_id.clone(),
        }),
        Some(("report", [task_id])) => Ok(TaskCliCommand::Report {
            task_id: task_id.clone(),
        }),
        Some(("review", [task_id])) => Ok(TaskCliCommand::Review {
            task_id: task_id.clone(),
        }),
        Some(("packet", rest)) => Ok(TaskCliCommand::Packet {
            command: parse_task_packet_cli_command(rest)?,
        }),
        Some(("scheduler", rest)) => Ok(TaskCliCommand::Scheduler {
            command: parse_task_scheduler_cli_command(rest)?,
        }),
        Some(("daemon", rest)) => Ok(TaskCliCommand::Daemon {
            command: parse_task_daemon_cli_command(rest)?,
        }),
        Some(("resume", [task_id])) => Ok(TaskCliCommand::Resume {
            task_id: task_id.clone(),
            from_node: None,
            prompt: None,
        }),
        Some(("resume", [task_id, flag, node_id, prompt @ ..])) if flag == "--from-node" => {
            Ok(TaskCliCommand::Resume {
                task_id: task_id.clone(),
                from_node: Some(node_id.clone()),
                prompt: if prompt.is_empty() {
                    None
                } else {
                    Some(prompt.join(" "))
                },
            })
        }
        Some(("resume", [task_id, prompt @ ..])) => Ok(TaskCliCommand::Resume {
            task_id: task_id.clone(),
            from_node: None,
            prompt: Some(prompt.join(" ")),
        }),
        Some(("execute", [task_id])) => Ok(TaskCliCommand::Execute {
            task_id: task_id.clone(),
            from_node: None,
        }),
        Some(("execute", [task_id, flag, node_id])) if flag == "--from-node" => {
            Ok(TaskCliCommand::Execute {
                task_id: task_id.clone(),
                from_node: Some(node_id.clone()),
            })
        }
        Some(("recover", [task_id])) => Ok(TaskCliCommand::Recover {
            task_id: task_id.clone(),
        }),
        Some(("retry", [task_id, flag, node_id])) if flag == "--node" => Ok(TaskCliCommand::Retry {
            task_id: task_id.clone(),
            node_id: node_id.clone(),
        }),
        Some(("verify", [task_id])) => Ok(TaskCliCommand::VerifyTask {
            task_id: task_id.clone(),
        }),
        Some(("verify", [task_id, flag, node_id, command @ ..]))
            if flag == "--node" && !command.is_empty() =>
        {
            Ok(TaskCliCommand::Verify {
                task_id: task_id.clone(),
                node_id: node_id.clone(),
                command: command.join(" "),
            })
        }
        Some(("compact", [task_id])) => Ok(TaskCliCommand::Compact {
            task_id: task_id.clone(),
            keep_last: 200,
        }),
        Some(("compact", [task_id, flag, keep_last])) if flag == "--keep-last" => {
            Ok(TaskCliCommand::Compact {
                task_id: task_id.clone(),
                keep_last: keep_last
                    .parse::<usize>()
                    .map_err(|_| format!("invalid --keep-last value: {keep_last}"))?,
            })
        }
        Some(("cancel", [task_id])) => Ok(TaskCliCommand::Cancel {
            task_id: task_id.clone(),
        }),
        Some((other, _)) => Err(format!(
            "unknown tasks command: {other}\nUsage: Himalaya tasks [list|show <task-id>|status <task-id>|report <task-id>|review <task-id>|packet create <packet.json>|packet run <packet.json>|packet status <task-id>|scheduler tick|scheduler queue|scheduler explain <task-id>|scheduler run [--once|--max-ticks N]|scheduler status|daemon start [--once|--max-ticks N]|daemon status|daemon stop|daemon logs [--limit N]|daemon report [--limit N] [--max-ticks N]|daemon evaluate [--limit N] [--max-ticks N]|daemon replay [--limit N] [--max-ticks N]|resume <task-id> [--from-node <node-id>] [prompt]|execute <task-id> [--from-node <node-id>]|retry <task-id> --node <node-id>|verify <task-id> [--node <node-id> <command>]|recover <task-id>|compact <task-id> [--keep-last N]|cancel <task-id>]"
        )),
    }
}

fn parse_task_packet_cli_command(args: &[String]) -> Result<TaskPacketCliCommand, String> {
    match args.split_first().map(|(command, rest)| (command.as_str(), rest)) {
        Some(("create", [path])) => Ok(TaskPacketCliCommand::Create {
            path: PathBuf::from(path),
        }),
        Some(("run", [path])) => Ok(TaskPacketCliCommand::Run {
            path: PathBuf::from(path),
        }),
        Some(("status", [task_id])) => Ok(TaskPacketCliCommand::Status {
            task_id: task_id.clone(),
        }),
        None => Err("tasks packet requires create, run, or status".to_string()),
        Some((other, _)) => Err(format!(
            "unknown tasks packet command: {other}\nUsage: Himalaya tasks packet [create <packet.json>|run <packet.json>|status <task-id>]"
        )),
    }
}

fn parse_task_scheduler_cli_command(args: &[String]) -> Result<TaskSchedulerCliCommand, String> {
    match args
        .split_first()
        .map(|(command, rest)| (command.as_str(), rest))
    {
        None | Some(("tick", [])) => Ok(TaskSchedulerCliCommand::Tick),
        Some(("queue", [])) => Ok(TaskSchedulerCliCommand::Queue),
        Some(("explain", [task_id])) => Ok(TaskSchedulerCliCommand::Explain {
            task_id: task_id.clone(),
        }),
        Some(("status", [])) => Ok(TaskSchedulerCliCommand::Status),
        Some(("run", rest)) => parse_task_scheduler_run_args(rest),
        Some((other, _)) => Err(format!(
            "unknown tasks scheduler command: {other}\nUsage: Himalaya tasks scheduler [tick|queue|explain <task-id>|run [--once|--max-ticks N]|status]"
        )),
    }
}

fn parse_task_scheduler_run_args(args: &[String]) -> Result<TaskSchedulerCliCommand, String> {
    let mut max_ticks = 1_usize;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--once" => {
                max_ticks = 1;
                index += 1;
            }
            "--max-ticks" => {
                let value = args.get(index + 1).ok_or_else(|| {
                    "tasks scheduler run --max-ticks requires a value".to_string()
                })?;
                max_ticks = parse_positive_usize("--max-ticks", value)?;
                index += 2;
            }
            value if value.starts_with("--max-ticks=") => {
                max_ticks = parse_positive_usize("--max-ticks", &value[12..])?;
                index += 1;
            }
            other => {
                return Err(format!(
                    "unknown tasks scheduler run argument: {other}\nUsage: Himalaya tasks scheduler run [--once|--max-ticks N]"
                ));
            }
        }
    }
    Ok(TaskSchedulerCliCommand::Run { max_ticks })
}

fn parse_task_daemon_cli_command(args: &[String]) -> Result<TaskDaemonCliCommand, String> {
    match args
        .split_first()
        .map(|(command, rest)| (command.as_str(), rest))
    {
        None | Some(("status", [])) => Ok(TaskDaemonCliCommand::Status),
        Some(("start", rest)) => parse_task_daemon_start_args(rest),
        Some(("stop", [])) => Ok(TaskDaemonCliCommand::Stop),
        Some(("logs", rest)) => parse_task_daemon_logs_args(rest),
        Some(("report", rest)) => parse_task_daemon_report_args(rest),
        Some(("evaluate" | "eval", rest)) => parse_task_daemon_evaluate_args(rest),
        Some(("replay", rest)) => parse_task_daemon_replay_args(rest),
        Some((other, _)) => Err(format!(
            "unknown tasks daemon command: {other}\nUsage: Himalaya tasks daemon [start [--once|--max-ticks N]|status|stop|logs [--limit N]|report [--limit N] [--max-ticks N]|evaluate [--limit N] [--max-ticks N]|replay [--limit N] [--max-ticks N]]"
        )),
    }
}

fn parse_task_daemon_start_args(args: &[String]) -> Result<TaskDaemonCliCommand, String> {
    let scheduler_command = parse_task_scheduler_run_args(args)?;
    let TaskSchedulerCliCommand::Run { max_ticks } = scheduler_command else {
        unreachable!("scheduler run parser only returns run command");
    };
    Ok(TaskDaemonCliCommand::Start { max_ticks })
}

fn parse_task_daemon_logs_args(args: &[String]) -> Result<TaskDaemonCliCommand, String> {
    let mut limit = 20_usize;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--limit" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "tasks daemon logs --limit requires a value".to_string())?;
                limit = parse_positive_usize("--limit", value)?;
                index += 2;
            }
            value if value.starts_with("--limit=") => {
                limit = parse_positive_usize("--limit", &value[8..])?;
                index += 1;
            }
            other => {
                return Err(format!(
                    "unknown tasks daemon logs argument: {other}\nUsage: Himalaya tasks daemon logs [--limit N]"
                ));
            }
        }
    }
    Ok(TaskDaemonCliCommand::Logs { limit })
}

fn parse_task_daemon_report_args(args: &[String]) -> Result<TaskDaemonCliCommand, String> {
    let mut limit = 20_usize;
    let mut max_ticks = 1_usize;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--limit" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "tasks daemon report --limit requires a value".to_string())?;
                limit = parse_positive_usize("--limit", value)?;
                index += 2;
            }
            value if value.starts_with("--limit=") => {
                limit = parse_positive_usize("--limit", &value[8..])?;
                index += 1;
            }
            "--max-ticks" => {
                let value = args.get(index + 1).ok_or_else(|| {
                    "tasks daemon report --max-ticks requires a value".to_string()
                })?;
                max_ticks = parse_positive_usize("--max-ticks", value)?;
                index += 2;
            }
            value if value.starts_with("--max-ticks=") => {
                max_ticks = parse_positive_usize("--max-ticks", &value[12..])?;
                index += 1;
            }
            "--once" => {
                max_ticks = 1;
                index += 1;
            }
            other => {
                return Err(format!(
                    "unknown tasks daemon report argument: {other}\nUsage: Himalaya tasks daemon report [--limit N] [--once|--max-ticks N]"
                ));
            }
        }
    }
    Ok(TaskDaemonCliCommand::Report { limit, max_ticks })
}

fn parse_task_daemon_evaluate_args(args: &[String]) -> Result<TaskDaemonCliCommand, String> {
    let (limit, max_ticks) = parse_task_daemon_review_window_args(args, "evaluate")?;
    Ok(TaskDaemonCliCommand::Evaluate { limit, max_ticks })
}

fn parse_task_daemon_replay_args(args: &[String]) -> Result<TaskDaemonCliCommand, String> {
    let (limit, max_ticks) = parse_task_daemon_review_window_args(args, "replay")?;
    Ok(TaskDaemonCliCommand::Replay { limit, max_ticks })
}

fn parse_task_daemon_review_window_args(
    args: &[String],
    command: &str,
) -> Result<(usize, usize), String> {
    let mut limit = 20_usize;
    let mut max_ticks = 1_usize;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--limit" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("tasks daemon {command} --limit requires a value"))?;
                limit = parse_positive_usize("--limit", value)?;
                index += 2;
            }
            value if value.starts_with("--limit=") => {
                limit = parse_positive_usize("--limit", &value[8..])?;
                index += 1;
            }
            "--max-ticks" => {
                let value = args.get(index + 1).ok_or_else(|| {
                    format!("tasks daemon {command} --max-ticks requires a value")
                })?;
                max_ticks = parse_positive_usize("--max-ticks", value)?;
                index += 2;
            }
            value if value.starts_with("--max-ticks=") => {
                max_ticks = parse_positive_usize("--max-ticks", &value[12..])?;
                index += 1;
            }
            "--once" => {
                max_ticks = 1;
                index += 1;
            }
            other => {
                return Err(format!(
                    "unknown tasks daemon {command} argument: {other}\nUsage: Himalaya tasks daemon {command} [--limit N] [--once|--max-ticks N]"
                ));
            }
        }
    }
    Ok((limit, max_ticks))
}

fn parse_positive_usize(name: &str, value: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("invalid {name} value: {value}"))?;
    if parsed == 0 {
        return Err(format!("{name} must be greater than 0"));
    }
    Ok(parsed)
}

fn parse_worker_cli_command(args: &[String]) -> Result<WorkerCliCommand, String> {
    match args
        .split_first()
        .map(|(command, rest)| (command.as_str(), rest))
    {
        None | Some(("list", [])) => Ok(WorkerCliCommand::List),
        Some(("create", rest)) => {
            let (cwd, trusted_roots) = parse_worker_cwd_and_trust_args(rest, "create")?;
            Ok(WorkerCliCommand::Create { cwd, trusted_roots })
        }
        Some(("spawn", rest)) => {
            let (cwd, trusted_roots, isolate_worktree, worktree_root, command) =
                parse_worker_spawn_args(rest)?;
            Ok(WorkerCliCommand::Spawn {
                cwd,
                trusted_roots,
                isolate_worktree,
                worktree_root,
                command,
            })
        }
        Some(("probe", [worker_id])) => Ok(WorkerCliCommand::Probe {
            worker_id: worker_id.clone(),
        }),
        Some(("observe", [worker_id, screen @ ..])) if !screen.is_empty() => {
            Ok(WorkerCliCommand::Observe {
                worker_id: worker_id.clone(),
                screen_text: screen.join(" "),
            })
        }
        Some(("complete" | "finish", [_, _, _, _, ..])) => Err(
            "workers complete accepts at most: <worker-id> [finish-reason] [tokens-output]"
                .to_string(),
        ),
        Some(("ready", [worker_id])) => Ok(WorkerCliCommand::Ready {
            worker_id: worker_id.clone(),
        }),
        Some(("resolve-trust" | "trust", [worker_id])) => Ok(WorkerCliCommand::ResolveTrust {
            worker_id: worker_id.clone(),
        }),
        Some(("prompt", [worker_id, prompt @ ..])) => Ok(WorkerCliCommand::Prompt {
            worker_id: worker_id.clone(),
            prompt: (!prompt.is_empty()).then(|| prompt.join(" ")),
        }),
        Some(("complete" | "finish", [worker_id])) => Ok(WorkerCliCommand::Complete {
            worker_id: worker_id.clone(),
            finish_reason: "stop".to_string(),
            tokens_output: 1,
        }),
        Some(("complete" | "finish", [worker_id, finish_reason])) => Ok(WorkerCliCommand::Complete {
            worker_id: worker_id.clone(),
            finish_reason: finish_reason.clone(),
            tokens_output: 1,
        }),
        Some(("complete" | "finish", [worker_id, finish_reason, tokens_output])) => {
            Ok(WorkerCliCommand::Complete {
                worker_id: worker_id.clone(),
                finish_reason: finish_reason.clone(),
                tokens_output: tokens_output.parse::<u64>().map_err(|_| {
                    format!("workers complete tokens-output must be an integer: {tokens_output}")
                })?,
            })
        }
        Some(("restart", [worker_id])) => Ok(WorkerCliCommand::Restart {
            worker_id: worker_id.clone(),
        }),
        Some(("terminate" | "stop", [worker_id])) => Ok(WorkerCliCommand::Terminate {
            worker_id: worker_id.clone(),
        }),
        Some(("cleanup", [])) => Ok(WorkerCliCommand::Cleanup {
            include_stale: false,
        }),
        Some(("cleanup", [flag])) if matches!(flag.as_str(), "--stale" | "--include-stale") => {
            Ok(WorkerCliCommand::Cleanup {
                include_stale: true,
            })
        }
        Some(("supervise" | "tick", [])) => Ok(WorkerCliCommand::Supervise),
        Some((other, _)) => Err(format!(
            "unknown workers command: {other}\nUsage: Himalaya workers [list|create|spawn [--cwd PATH] [--trusted-root PATH] [--isolate-worktree] [--worktree-root PATH] -- COMMAND...|probe <worker-id>|observe <worker-id> <screen>|ready <worker-id>|resolve-trust <worker-id>|prompt <worker-id> [prompt]|complete <worker-id> [finish-reason] [tokens-output]|restart <worker-id>|terminate <worker-id>|cleanup [--stale]|supervise]"
        )),
    }
}

fn parse_worker_cwd_and_trust_args(
    args: &[String],
    command_name: &str,
) -> Result<(Option<PathBuf>, Vec<String>), String> {
    let mut cwd = None;
    let mut trusted_roots = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--cwd" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("workers {command_name} --cwd requires a path"))?;
                cwd = Some(PathBuf::from(value));
                index += 2;
            }
            value if value.starts_with("--cwd=") => {
                cwd = Some(PathBuf::from(&value[6..]));
                index += 1;
            }
            "--trusted-root" => {
                let value = args.get(index + 1).ok_or_else(|| {
                    format!("workers {command_name} --trusted-root requires a path")
                })?;
                trusted_roots.push(value.clone());
                index += 2;
            }
            value if value.starts_with("--trusted-root=") => {
                trusted_roots.push(value[15..].to_string());
                index += 1;
            }
            value if !value.starts_with('-') && cwd.is_none() => {
                cwd = Some(PathBuf::from(value));
                index += 1;
            }
            other => {
                return Err(format!(
                    "unknown workers {command_name} argument: {other}\nUsage: Himalaya workers {command_name} [--cwd PATH] [--trusted-root PATH]"
                ));
            }
        }
    }
    Ok((cwd, trusted_roots))
}

type WorkerSpawnArgs = (
    Option<PathBuf>,
    Vec<String>,
    bool,
    Option<PathBuf>,
    Vec<String>,
);

fn parse_worker_spawn_args(args: &[String]) -> Result<WorkerSpawnArgs, String> {
    let mut cwd = None;
    let mut trusted_roots = Vec::new();
    let mut isolate_worktree = false;
    let mut worktree_root = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--" => {
                let command = args[index + 1..].to_vec();
                if command.is_empty() {
                    return Err("workers spawn requires a command after --".to_string());
                }
                return Ok((cwd, trusted_roots, isolate_worktree, worktree_root, command));
            }
            "--cwd" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "workers spawn --cwd requires a path".to_string())?;
                cwd = Some(PathBuf::from(value));
                index += 2;
            }
            value if value.starts_with("--cwd=") => {
                cwd = Some(PathBuf::from(&value[6..]));
                index += 1;
            }
            "--trusted-root" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "workers spawn --trusted-root requires a path".to_string())?;
                trusted_roots.push(value.clone());
                index += 2;
            }
            value if value.starts_with("--trusted-root=") => {
                trusted_roots.push(value[15..].to_string());
                index += 1;
            }
            "--isolate-worktree" => {
                isolate_worktree = true;
                index += 1;
            }
            "--worktree-root" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "workers spawn --worktree-root requires a path".to_string())?;
                worktree_root = Some(PathBuf::from(value));
                isolate_worktree = true;
                index += 2;
            }
            value if value.starts_with("--worktree-root=") => {
                worktree_root = Some(PathBuf::from(&value[16..]));
                isolate_worktree = true;
                index += 1;
            }
            value if !value.starts_with('-') => {
                let command = args[index..].to_vec();
                if command.is_empty() {
                    return Err("workers spawn requires a command".to_string());
                }
                return Ok((cwd, trusted_roots, isolate_worktree, worktree_root, command));
            }
            other => {
                return Err(format!(
                    "unknown workers spawn argument: {other}\nUsage: Himalaya workers spawn [--cwd PATH] [--trusted-root PATH] [--isolate-worktree] [--worktree-root PATH] -- COMMAND..."
                ));
            }
        }
    }
    Err("workers spawn requires a command".to_string())
}

fn parse_cron_cli_command(args: &[String]) -> Result<CronCliCommand, String> {
    match args.split_first().map(|(command, rest)| (command.as_str(), rest)) {
        None | Some(("list", [])) => Ok(CronCliCommand::List),
        Some(("add" | "create", rest)) => {
            let (schedule, prompt_parts) = parse_cron_schedule_and_prompt(rest)?;
            Ok(CronCliCommand::Add {
                schedule,
                prompt: prompt_parts.join(" "),
                description: None,
            })
        }
        Some(("remove" | "delete" | "rm", [cron_id])) => Ok(CronCliCommand::Remove {
            cron_id: cron_id.clone(),
        }),
        Some(("run", rest)) => {
            // Optional runaway guards: `--max-fires N` bounds due cron entries
            // and `--max-ticks N` bounds durable scheduler ticks after enqueue.
            let mut max_fires = 16usize;
            let mut max_ticks = 1usize;
            let mut index = 0;
            while index < rest.len() {
                match rest[index].as_str() {
                    "--max-fires" => {
                        let value = rest
                            .get(index + 1)
                            .ok_or_else(|| "--max-fires requires a value".to_string())?;
                        max_fires = parse_positive_usize("--max-fires", value)?;
                        index += 2;
                    }
                    value if value.starts_with("--max-fires=") => {
                        max_fires = parse_positive_usize("--max-fires", &value[12..])?;
                        index += 1;
                    }
                    "--max-ticks" => {
                        let value = rest
                            .get(index + 1)
                            .ok_or_else(|| "--max-ticks requires a value".to_string())?;
                        max_ticks = parse_positive_usize("--max-ticks", value)?;
                        index += 2;
                    }
                    value if value.starts_with("--max-ticks=") => {
                        max_ticks = parse_positive_usize("--max-ticks", &value[12..])?;
                        index += 1;
                    }
                    "--once" => {
                        max_ticks = 1;
                        index += 1;
                    }
                    other => return Err(format!("unknown cron run argument: {other}")),
                }
            }
            Ok(CronCliCommand::Run {
                max_fires,
                max_ticks,
            })
        }
        Some((other, _)) => Err(format!(
            "unknown cron command: {other}\nUsage: Himalaya cron [list|add <5-field-cron> <prompt>|remove <cron-id>|run [--max-fires N] [--max-ticks N]]"
        )),
    }
}

fn parse_cron_schedule_and_prompt(args: &[String]) -> Result<(String, &[String]), String> {
    if args.is_empty() {
        return Err("cron add requires a schedule and prompt".to_string());
    }
    if args[0].split_whitespace().count() == 5 {
        let prompt_parts = &args[1..];
        if prompt_parts.is_empty() {
            return Err("cron add requires a prompt".to_string());
        }
        return Ok((args[0].clone(), prompt_parts));
    }
    if args.len() < 6 {
        return Err("cron add requires a 5-field schedule followed by a prompt".to_string());
    }
    let schedule = args[..5].join(" ");
    if schedule.split_whitespace().count() != 5 {
        return Err(format!("invalid cron schedule: {schedule}"));
    }
    Ok((schedule, &args[5..]))
}
fn parse_local_help_action(rest: &[String]) -> Option<Result<CliAction, String>> {
    if rest.len() != 2 || !is_help_flag(&rest[1]) {
        return None;
    }

    let topic = match rest[0].as_str() {
        "status" => LocalHelpTopic::Status,
        "sandbox" => LocalHelpTopic::Sandbox,
        "doctor" => LocalHelpTopic::Doctor,
        _ => return None,
    };
    Some(Ok(CliAction::HelpTopic(topic)))
}

fn is_help_flag(value: &str) -> bool {
    matches!(value, "--help" | "-h")
}

fn parse_single_word_command_alias(
    rest: &[String],
    model: &str,
    permission_mode_override: Option<PermissionMode>,
    output_format: CliOutputFormat,
) -> Option<Result<CliAction, String>> {
    if rest.len() != 1 {
        return None;
    }

    match rest[0].as_str() {
        "help" => Some(Ok(CliAction::Help { output_format })),
        "version" => Some(Ok(CliAction::Version { output_format })),
        "status" => Some(Ok(CliAction::Status {
            model: model.to_string(),
            permission_mode: permission_mode_override.unwrap_or_else(default_permission_mode),
            output_format,
        })),
        "sandbox" => Some(Ok(CliAction::Sandbox { output_format })),
        "doctor" => Some(Ok(CliAction::Doctor { output_format })),
        "state" => Some(Ok(CliAction::State { output_format })),
        "test" | "lint" | "build" | "review" | "diagnostics" | "workspace" | "cwd" => {
            Some(Ok(CliAction::Local {
                command: parse_local_cli_command(rest)
                    .expect("single-word local command should parse"),
                output_format,
            }))
        }
        other => bare_slash_command_guidance(other).map(Err),
    }
}

fn bare_slash_command_guidance(command_name: &str) -> Option<String> {
    if matches!(
        command_name,
        "dump-manifests"
            | "bootstrap-plan"
            | "agents"
            | "mcp"
            | "skills"
            | "tasks"
            | "cron"
            | "workers"
            | "worker"
            | "plan"
            | "benchmark"
            | "system-prompt"
            | "login"
            | "logout"
            | "init"
            | "prompt"
            | "export"
    ) {
        return None;
    }
    let slash_command = slash_command_specs()
        .iter()
        .find(|spec| spec.name == command_name)?;
    let guidance = if slash_command.resume_supported {
        format!(
            "`Himalaya {command_name}` is a slash command. Use `Himalaya --resume SESSION.jsonl /{command_name}` or start `Himalaya` and run `/{command_name}`."
        )
    } else {
        format!(
            "`Himalaya {command_name}` is a slash command. Start `Himalaya` and run `/{command_name}` inside the REPL."
        )
    };
    Some(guidance)
}

fn join_optional_args(args: &[String]) -> Option<String> {
    let joined = args.join(" ");
    let trimmed = joined.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn split_slash_remainder(args: Option<&str>) -> Vec<String> {
    args.unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn parse_direct_slash_cli_action(
    rest: &[String],
    model: String,
    output_format: CliOutputFormat,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    compact: bool,
    base_commit: Option<String>,
    reasoning_effort: Option<String>,
    file_paths: &[PathBuf],
    allow_broad_cwd: bool,
) -> Result<CliAction, String> {
    let raw = rest.join(" ");
    match SlashCommand::parse(&raw) {
        Ok(Some(SlashCommand::Help)) => Ok(CliAction::Help { output_format }),
        Ok(Some(SlashCommand::Agents { args })) => Ok(CliAction::Agents {
            args,
            output_format,
        }),
        Ok(Some(SlashCommand::Mcp { action, target })) => Ok(CliAction::Mcp {
            args: match (action, target) {
                (None, None) => None,
                (Some(action), None) => Some(action),
                (Some(action), Some(target)) => Some(format!("{action} {target}")),
                (None, Some(target)) => Some(target),
            },
            output_format,
        }),
        Ok(Some(SlashCommand::Skills { args })) => {
            match classify_skills_slash_command(args.as_deref()) {
                SkillSlashDispatch::Invoke(prompt) => Ok(CliAction::Prompt {
                    prompt,
                    model,
                    output_format,
                    allowed_tools,
                    permission_mode,
                    compact,
                    base_commit,
                    reasoning_effort: reasoning_effort.clone(),
                    allow_broad_cwd,
                    file_paths: file_paths.to_vec(),
                }),
                SkillSlashDispatch::Local => Ok(CliAction::Skills {
                    args,
                    output_format,
                }),
            }
        }
        Ok(Some(SlashCommand::Plan { mode })) => {
            let prompt = mode.unwrap_or_default();
            if prompt.trim().is_empty() {
                return Err("/plan requires a task description".to_string());
            }
            Ok(CliAction::Plan {
                prompt,
                output_format,
                permission_mode,
            })
        }
        Ok(Some(SlashCommand::Tasks { args })) => Ok(CliAction::Tasks {
            command: parse_task_cli_command(&split_slash_remainder(args.as_deref()))?,
            output_format,
            model,
            allowed_tools,
            permission_mode,
            compact,
            reasoning_effort: reasoning_effort.clone(),
        }),
        Ok(Some(SlashCommand::Cron { args })) => Ok(CliAction::Cron {
            command: parse_cron_cli_command(&split_slash_remainder(args.as_deref()))?,
            output_format,
            permission_mode: PermissionMode::ReadOnly,
        }),
        Ok(Some(SlashCommand::Benchmark { args })) => Ok(CliAction::Benchmark {
            command: parse_benchmark_cli_command(&split_slash_remainder(args.as_deref()))?,
            output_format,
            model: model.clone(),
        }),
        Ok(Some(SlashCommand::LocalCommand { name, args })) => Ok(CliAction::Local {
            command: parse_local_cli_command_from_parts(&name, args.as_deref())?,
            output_format,
        }),
        Ok(Some(SlashCommand::Review { scope })) => Ok(CliAction::Local {
            command: LocalCliCommand::Review { scope },
            output_format,
        }),
        Ok(Some(SlashCommand::Workspace { path })) => Ok(CliAction::Local {
            command: LocalCliCommand::Workspace {
                path: path.map(PathBuf::from),
            },
            output_format,
        }),
        Ok(Some(SlashCommand::Diagnostics { path })) => Ok(CliAction::Local {
            command: LocalCliCommand::Diagnostics { path },
            output_format,
        }),
        Ok(Some(SlashCommand::Unknown(name))) => Err(format_unknown_direct_slash_command(&name)),
        Ok(Some(command)) => Err({
            let _ = command;
            format!(
                "slash command {command_name} is interactive-only. Start `Himalaya` and run it there, or use `Himalaya --resume SESSION.jsonl {command_name}` / `Himalaya --resume {latest} {command_name}` when the command is marked [resume] in /help.",
                command_name = rest[0],
                latest = LATEST_SESSION_REFERENCE,
            )
        }),
        Ok(None) => Err(format!("unknown subcommand: {}", rest[0])),
        Err(error) => Err(error.to_string()),
    }
}

fn format_unknown_option(option: &str) -> String {
    let mut message = format!("unknown option: {option}");
    if let Some(suggestion) = suggest_closest_term(option, CLI_OPTION_SUGGESTIONS) {
        message.push_str("\nDid you mean ");
        message.push_str(suggestion);
        message.push('?');
    }
    message.push_str("\nRun `Himalaya --help` for usage.");
    message
}

fn format_unknown_direct_slash_command(name: &str) -> String {
    let mut message = format!("unknown slash command outside the REPL: /{name}");
    if let Some(suggestions) = render_suggestion_line("Did you mean", &suggest_slash_commands(name))
    {
        message.push('\n');
        message.push_str(&suggestions);
    }
    if let Some(note) = omc_compatibility_note_for_unknown_slash_command(name) {
        message.push('\n');
        message.push_str(note);
    }
    message.push_str("\nRun `Himalaya --help` for CLI usage, or start `Himalaya` and use /help.");
    message
}

fn format_unknown_slash_command(name: &str) -> String {
    let mut message = format!("Unknown slash command: /{name}");
    if let Some(suggestions) = render_suggestion_line("Did you mean", &suggest_slash_commands(name))
    {
        message.push('\n');
        message.push_str(&suggestions);
    }
    if let Some(note) = omc_compatibility_note_for_unknown_slash_command(name) {
        message.push('\n');
        message.push_str(note);
    }
    message.push_str("\n  Help             /help lists available slash commands");
    message
}

fn omc_compatibility_note_for_unknown_slash_command(name: &str) -> Option<&'static str> {
    name.starts_with("oh-my-Himalayacode:")
        .then_some(
            "Compatibility note: `/oh-my-Himalayacode:*` is a Himalaya Code/OMC plugin command. `Himalaya` does not yet load plugin slash commands, Himalaya statusline stdin, or OMC session hooks.",
        )
}

fn render_suggestion_line(label: &str, suggestions: &[String]) -> Option<String> {
    (!suggestions.is_empty()).then(|| format!("  {label:<16} {}", suggestions.join(", "),))
}

fn suggest_slash_commands(input: &str) -> Vec<String> {
    let mut candidates = slash_command_specs()
        .iter()
        .flat_map(|spec| {
            std::iter::once(spec.name)
                .chain(spec.aliases.iter().copied())
                .map(|name| format!("/{name}"))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    candidates.sort();
    candidates.dedup();
    let candidate_refs = candidates.iter().map(String::as_str).collect::<Vec<_>>();
    ranked_suggestions(input.trim_start_matches('/'), &candidate_refs)
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn suggest_closest_term<'a>(input: &str, candidates: &'a [&'a str]) -> Option<&'a str> {
    ranked_suggestions(input, candidates).into_iter().next()
}

fn ranked_suggestions<'a>(input: &str, candidates: &'a [&'a str]) -> Vec<&'a str> {
    let normalized_input = input.trim_start_matches('/').to_ascii_lowercase();
    let mut ranked = candidates
        .iter()
        .filter_map(|candidate| {
            let normalized_candidate = candidate.trim_start_matches('/').to_ascii_lowercase();
            let distance = levenshtein_distance(&normalized_input, &normalized_candidate);
            let prefix_bonus = usize::from(
                !(normalized_candidate.starts_with(&normalized_input)
                    || normalized_input.starts_with(&normalized_candidate)),
            );
            let score = distance + prefix_bonus;
            (score <= 4).then_some((score, *candidate))
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| left.cmp(right).then_with(|| left.1.cmp(right.1)));
    ranked
        .into_iter()
        .map(|(_, candidate)| candidate)
        .take(3)
        .collect()
}

fn levenshtein_distance(left: &str, right: &str) -> usize {
    if left.is_empty() {
        return right.chars().count();
    }
    if right.is_empty() {
        return left.chars().count();
    }

    let right_chars = right.chars().collect::<Vec<_>>();
    let mut previous = (0..=right_chars.len()).collect::<Vec<_>>();
    let mut current = vec![0; right_chars.len() + 1];

    for (left_index, left_char) in left.chars().enumerate() {
        current[0] = left_index + 1;
        for (right_index, right_char) in right_chars.iter().enumerate() {
            let substitution_cost = usize::from(left_char != *right_char);
            current[right_index + 1] = (previous[right_index + 1] + 1)
                .min(current[right_index] + 1)
                .min(previous[right_index] + substitution_cost);
        }
        previous.clone_from(&current);
    }

    previous[right_chars.len()]
}

/// Built-in convenience aliases. These are checked AFTER any user-defined
/// aliases in settings.json (`{"aliases": {"my-shortcut": "full-model-id"}}`),
/// so a user alias always takes precedence over a built-in of the same name.
fn resolve_model_alias(model: &str) -> &str {
    match model {
        "opus" => "Himalaya-opus-4-6",
        "sonnet" => "Himalaya-sonnet-4-6",
        "haiku" => "Himalaya-haiku-4-5-20251213",
        _ => model,
    }
}

/// Resolve a model name through user-defined config aliases first, then fall
/// back to the built-in alias table. This is the entry point used wherever a
/// user-supplied model string is about to be dispatched to a provider.
fn resolve_model_alias_with_config(model: &str) -> String {
    let trimmed = model.trim();
    if let Some(resolved) = config_alias_for_current_dir(trimmed) {
        return resolve_model_alias(&resolved).to_string();
    }
    resolve_model_alias(trimmed).to_string()
}

fn config_alias_for_current_dir(alias: &str) -> Option<String> {
    if alias.is_empty() {
        return None;
    }
    let cwd = env::current_dir().ok()?;
    let loader = ConfigLoader::default_for(&cwd);
    let config = loader.load().ok()?;
    config.aliases().get(alias).cloned()
}

fn normalize_allowed_tools(values: &[String]) -> Result<Option<AllowedToolSet>, String> {
    if values.is_empty() {
        return Ok(None);
    }
    current_tool_registry()?.normalize_allowed_tools(values)
}

fn current_tool_registry() -> Result<GlobalToolRegistry, String> {
    let cwd = env::current_dir().map_err(|error| error.to_string())?;
    let loader = ConfigLoader::default_for(&cwd);
    let runtime_config = loader.load().map_err(|error| error.to_string())?;
    let state = build_runtime_plugin_state_with_loader(&cwd, &loader, &runtime_config)
        .map_err(|error| error.to_string())?;
    let registry = state.tool_registry.clone();
    if let Some(mcp_state) = state.mcp_state {
        mcp_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .shutdown()
            .map_err(|error| error.to_string())?;
    }
    Ok(registry)
}

fn parse_permission_mode_arg(value: &str) -> Result<PermissionMode, String> {
    PermissionMode::parse_public(value).ok_or_else(|| {
        format!(
            "unsupported permission mode '{value}'. Use {}.",
            PermissionMode::public_labels().join(", ")
        )
    })
}

fn permission_mode_from_label(mode: &str) -> PermissionMode {
    PermissionMode::parse_public(mode)
        .unwrap_or_else(|| panic!("unsupported permission mode label: {mode}"))
}

fn default_permission_mode() -> PermissionMode {
    env::var("RUSTY_Himalaya_PERMISSION_MODE")
        .ok()
        .as_deref()
        .and_then(PermissionMode::parse_public)
        .or_else(config_permission_mode_for_current_dir)
        .unwrap_or(PermissionMode::Prompt)
}

fn config_permission_mode_for_current_dir() -> Option<PermissionMode> {
    let cwd = env::current_dir().ok()?;
    let loader = ConfigLoader::default_for(&cwd);
    loader
        .load()
        .ok()?
        .permission_mode()
        .map(PermissionMode::from)
}

fn config_model_for_current_dir() -> Option<String> {
    let cwd = env::current_dir().ok()?;
    let loader = ConfigLoader::default_for(&cwd);
    loader.load().ok()?.model().map(ToOwned::to_owned)
}

fn resolve_repl_model(cli_model: String) -> String {
    if cli_model != DEFAULT_MODEL {
        return cli_model;
    }
    if let Some(env_model) = env::var("ANTHROPIC_MODEL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return resolve_model_alias_with_config(&env_model);
    }
    if let Some(config_model) = config_model_for_current_dir() {
        return resolve_model_alias_with_config(&config_model);
    }
    // Check for a previously persisted wizard choice (P0-1: so the user does
    // not have to re-run the wizard on every launch).
    if let Ok(cwd) = env::current_dir() {
        if let Some(saved) = provider_config::load_wizard_selection(&cwd) {
            if let Some(url) = &saved.base_url {
                env::set_var("OPENAI_BASE_URL", url);
            }
            if let Some(key) = &saved.api_key {
                env::set_var("OPENAI_API_KEY", key);
            }
            return saved.model;
        }
    }
    // No model configured — run the interactive wizard when in a terminal.
    if let Some(selection) = model_selector::run_wizard() {
        if let Some(url) = &selection.base_url {
            env::set_var("OPENAI_BASE_URL", url);
        }
        if let Some(key) = &selection.api_key {
            env::set_var("OPENAI_API_KEY", key);
        }
        // Persist the selection so the next launch skips the wizard.
        if let Ok(cwd) = env::current_dir() {
            let _ = provider_config::persist_wizard_selection(&selection, &cwd);
        }
        return selection.model;
    }
    cli_model
}

fn provider_label(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Anthropic => "anthropic",
        ProviderKind::Xai => "xai",
        ProviderKind::OpenAi => "openai",
    }
}

fn format_connected_line(model: &str) -> String {
    let provider = provider_label(detect_provider_kind(model));
    format!("Connected: {model} via {provider}")
}

fn filter_tool_specs(
    tool_registry: &GlobalToolRegistry,
    allowed_tools: Option<&AllowedToolSet>,
) -> Vec<ToolDefinition> {
    tool_registry.definitions(allowed_tools)
}

fn parse_system_prompt_args(
    args: &[String],
    output_format: CliOutputFormat,
) -> Result<CliAction, String> {
    let mut cwd = env::current_dir().map_err(|error| error.to_string())?;
    let mut date = DEFAULT_DATE.to_string();
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--cwd" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --cwd".to_string())?;
                cwd = PathBuf::from(value);
                index += 2;
            }
            "--date" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --date".to_string())?;
                date.clone_from(value);
                index += 2;
            }
            other => return Err(format!("unknown system-prompt option: {other}")),
        }
    }

    Ok(CliAction::PrintSystemPrompt {
        cwd,
        date,
        output_format,
    })
}

fn parse_export_args(args: &[String], output_format: CliOutputFormat) -> Result<CliAction, String> {
    let mut session_reference = LATEST_SESSION_REFERENCE.to_string();
    let mut output_path: Option<PathBuf> = None;
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--session" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --session".to_string())?;
                session_reference = value.clone();
                index += 2;
            }
            flag if flag.starts_with("--session=") => {
                session_reference = flag[10..].to_string();
                index += 1;
            }
            "--output" | "-o" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("missing value for {}", args[index]))?;
                output_path = Some(PathBuf::from(value));
                index += 2;
            }
            flag if flag.starts_with("--output=") => {
                output_path = Some(PathBuf::from(&flag[9..]));
                index += 1;
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown export option: {other}"));
            }
            other if output_path.is_none() => {
                output_path = Some(PathBuf::from(other));
                index += 1;
            }
            other => {
                return Err(format!("unexpected export argument: {other}"));
            }
        }
    }

    Ok(CliAction::Export {
        session_reference,
        output_path,
        output_format,
    })
}

fn parse_resume_prompt_args(args: &[String]) -> Result<Option<(PathBuf, String)>, String> {
    let Some(first) = args.first() else {
        return Ok(None);
    };
    if looks_like_slash_command_token(first) {
        return Ok(None);
    }

    let (session_path, prompt_tokens) = if first == "prompt" {
        (PathBuf::from(LATEST_SESSION_REFERENCE), &args[1..])
    } else if args.get(1).map(String::as_str) == Some("prompt") {
        (PathBuf::from(first), &args[2..])
    } else {
        return Ok(None);
    };

    let prompt = prompt_tokens.join(" ");
    if prompt.trim().is_empty() {
        return Err("resumed prompt subcommand requires a prompt string".to_string());
    }

    Ok(Some((session_path, prompt)))
}

fn parse_resume_args(args: &[String], output_format: CliOutputFormat) -> Result<CliAction, String> {
    let (session_path, command_tokens): (PathBuf, &[String]) = match args.first() {
        None => (PathBuf::from(LATEST_SESSION_REFERENCE), &[]),
        Some(first) if looks_like_slash_command_token(first) => {
            (PathBuf::from(LATEST_SESSION_REFERENCE), args)
        }
        Some(first) => (PathBuf::from(first), &args[1..]),
    };
    let mut commands = Vec::new();
    let mut current_command = String::new();

    for token in command_tokens {
        if token.trim_start().starts_with('/') {
            if resume_command_can_absorb_token(&current_command, token) {
                current_command.push(' ');
                current_command.push_str(token);
                continue;
            }
            if !current_command.is_empty() {
                commands.push(current_command);
            }
            current_command = String::from(token.as_str());
            continue;
        }

        if current_command.is_empty() {
            return Err("--resume trailing arguments must be slash commands".to_string());
        }

        current_command.push(' ');
        current_command.push_str(token);
    }

    if !current_command.is_empty() {
        commands.push(current_command);
    }

    Ok(CliAction::ResumeSession {
        session_path,
        commands,
        output_format,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagnosticLevel {
    Ok,
    Warn,
    Fail,
}

impl DiagnosticLevel {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }

    fn is_failure(self) -> bool {
        matches!(self, Self::Fail)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DiagnosticCheck {
    name: &'static str,
    level: DiagnosticLevel,
    summary: String,
    details: Vec<String>,
    data: Map<String, Value>,
}

impl DiagnosticCheck {
    fn new(name: &'static str, level: DiagnosticLevel, summary: impl Into<String>) -> Self {
        Self {
            name,
            level,
            summary: summary.into(),
            details: Vec::new(),
            data: Map::new(),
        }
    }

    fn with_details(mut self, details: Vec<String>) -> Self {
        self.details = details;
        self
    }

    fn with_data(mut self, data: Map<String, Value>) -> Self {
        self.data = data;
        self
    }

    fn json_value(&self) -> Value {
        let mut value = Map::from_iter([
            (
                "name".to_string(),
                Value::String(self.name.to_ascii_lowercase()),
            ),
            (
                "status".to_string(),
                Value::String(self.level.label().to_string()),
            ),
            ("summary".to_string(), Value::String(self.summary.clone())),
            (
                "details".to_string(),
                Value::Array(
                    self.details
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect::<Vec<_>>(),
                ),
            ),
        ]);
        value.extend(self.data.clone());
        Value::Object(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DoctorReport {
    checks: Vec<DiagnosticCheck>,
}

impl DoctorReport {
    fn counts(&self) -> (usize, usize, usize) {
        (
            self.checks
                .iter()
                .filter(|check| check.level == DiagnosticLevel::Ok)
                .count(),
            self.checks
                .iter()
                .filter(|check| check.level == DiagnosticLevel::Warn)
                .count(),
            self.checks
                .iter()
                .filter(|check| check.level == DiagnosticLevel::Fail)
                .count(),
        )
    }

    fn has_failures(&self) -> bool {
        self.checks.iter().any(|check| check.level.is_failure())
    }

    fn render(&self) -> String {
        let (ok_count, warn_count, fail_count) = self.counts();
        let mut lines = vec![
            "Doctor".to_string(),
            format!(
                "Summary\n  OK               {ok_count}\n  Warnings         {warn_count}\n  Failures         {fail_count}"
            ),
        ];
        lines.extend(self.checks.iter().map(render_diagnostic_check));
        lines.join("\n\n")
    }

    fn json_value(&self) -> Value {
        let report = self.render();
        let (ok_count, warn_count, fail_count) = self.counts();
        json!({
            "kind": "doctor",
            "message": report,
            "report": report,
            "has_failures": self.has_failures(),
            "summary": {
                "total": self.checks.len(),
                "ok": ok_count,
                "warnings": warn_count,
                "failures": fail_count,
            },
            "checks": self
                .checks
                .iter()
                .map(DiagnosticCheck::json_value)
                .collect::<Vec<_>>(),
        })
    }
}

fn render_diagnostic_check(check: &DiagnosticCheck) -> String {
    let mut lines = vec![format!(
        "{}\n  Status           {}\n  Summary          {}",
        check.name,
        check.level.label(),
        check.summary
    )];
    if !check.details.is_empty() {
        lines.push("  Details".to_string());
        lines.extend(check.details.iter().map(|detail| format!("    - {detail}")));
    }
    lines.join("\n")
}

fn render_doctor_report() -> Result<DoctorReport, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let config_loader = ConfigLoader::default_for(&cwd);
    let config = config_loader.load();
    let discovered_config = config_loader.discover();
    let project_context = ProjectContext::discover_with_git(&cwd, DEFAULT_DATE)?;
    let (project_root, git_branch) =
        parse_git_status_metadata(project_context.git_status.as_deref());
    let git_summary = parse_git_workspace_summary(project_context.git_status.as_deref());
    let empty_config = runtime::RuntimeConfig::empty();
    let sandbox_config = config.as_ref().ok().unwrap_or(&empty_config);
    let context = StatusContext {
        cwd: cwd.clone(),
        session_path: None,
        loaded_config_files: config
            .as_ref()
            .ok()
            .map_or(0, |runtime_config| runtime_config.loaded_entries().len()),
        discovered_config_files: discovered_config.len(),
        memory_file_count: project_context.instruction_files.len(),
        project_root,
        git_branch,
        git_summary,
        sandbox_status: resolve_sandbox_status(sandbox_config.sandbox(), &cwd),
    };
    Ok(DoctorReport {
        checks: vec![
            check_auth_health(),
            check_config_health(&config_loader, config.as_ref()),
            check_workspace_health(&context),
            check_sandbox_health(&context.sandbox_status),
            check_system_health(&cwd, config.as_ref().ok()),
        ],
    })
}

fn run_doctor(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    let report = render_doctor_report()?;
    let message = report.render();
    match output_format {
        CliOutputFormat::Text => println!("{message}"),
        CliOutputFormat::Json | CliOutputFormat::StreamJson => {
            println!("{}", serde_json::to_string_pretty(&report.json_value())?);
        }
    }
    if report.has_failures() {
        return Err("doctor found failing checks".into());
    }
    Ok(())
}

/// Starts a minimal Model Context Protocol server that exposes Himalaya's
/// built-in tools over stdio.
///
/// Tool descriptors come from [`tools::mvp_tool_specs`] and calls are
/// dispatched through [`tools::execute_tool`], so this server exposes exactly
/// Read `.Himalaya/worker-state.json` from the current working directory and print it.
/// This is the file-based worker observability surface: `push_event()` in `worker_boot.rs`
/// atomically writes state transitions here so external observers (Himalayahip, orchestrators)
/// can poll current `WorkerStatus` without needing an HTTP route on the opencode binary.
fn run_worker_state(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let state_path = cwd.join(".Himalaya").join("worker-state.json");
    if !state_path.exists() {
        // Emit a structured error, then return Err so the process exits 1.
        // Callers (scripts, CI) need a non-zero exit to detect "no state" without
        // parsing prose output.
        // Let the error propagate to main() which will format it correctly
        // (prose for text mode, JSON envelope for --output-format json).
        return Err(format!(
            "no worker state file found at {} — run a worker first",
            state_path.display()
        )
        .into());
    }
    let raw = std::fs::read_to_string(&state_path)?;
    match output_format {
        CliOutputFormat::Text => println!("{raw}"),
        CliOutputFormat::Json | CliOutputFormat::StreamJson => {
            // Validate it parses as JSON before re-emitting
            let _: serde_json::Value = serde_json::from_str(&raw)?;
            println!("{raw}");
        }
    }
    Ok(())
}

/// the same surface the in-process agent loop uses.
fn run_mcp_serve() -> Result<(), Box<dyn std::error::Error>> {
    let tools = mvp_tool_specs()
        .into_iter()
        .map(|spec| McpTool {
            name: spec.name.to_string(),
            description: Some(spec.description.to_string()),
            input_schema: Some(spec.input_schema),
            annotations: None,
            meta: None,
        })
        .collect();

    let spec = McpServerSpec {
        server_name: "Himalaya".to_string(),
        server_version: VERSION.to_string(),
        tools,
        tool_handler: Box::new(execute_tool),
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let mut server = McpServer::new(spec);
        server.run().await
    })?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn check_auth_health() -> DiagnosticCheck {
    let api_key_present = env::var("ANTHROPIC_API_KEY")
        .ok()
        .is_some_and(|value| !value.trim().is_empty());
    let auth_token_present = env::var("ANTHROPIC_AUTH_TOKEN")
        .ok()
        .is_some_and(|value| !value.trim().is_empty());

    match load_oauth_credentials() {
        Ok(Some(token_set)) => {
            let expired = oauth_token_is_expired(&api::OAuthTokenSet {
                access_token: token_set.access_token.clone(),
                refresh_token: token_set.refresh_token.clone(),
                expires_at: token_set.expires_at,
                scopes: token_set.scopes.clone(),
            });
            let mut details = vec![
                format!(
                    "Environment       api_key={} auth_token={}",
                    if api_key_present { "present" } else { "absent" },
                    if auth_token_present {
                        "present"
                    } else {
                        "absent"
                    }
                ),
                format!(
                    "Saved OAuth       expires_at={} refresh_token={} scopes={}",
                    token_set
                        .expires_at
                        .map_or_else(|| "<none>".to_string(), |value| value.to_string()),
                    if token_set.refresh_token.is_some() {
                        "present"
                    } else {
                        "absent"
                    },
                    if token_set.scopes.is_empty() {
                        "<none>".to_string()
                    } else {
                        token_set.scopes.join(",")
                    }
                ),
            ];
            if expired {
                details.push(
                    "Suggested action  Himalaya login to refresh local OAuth credentials"
                        .to_string(),
                );
            }
            DiagnosticCheck::new(
                "Auth",
                if expired {
                    DiagnosticLevel::Warn
                } else {
                    DiagnosticLevel::Ok
                },
                if expired {
                    "saved OAuth credentials are present but expired"
                } else if api_key_present || auth_token_present {
                    "environment and saved credentials are available"
                } else {
                    "saved OAuth credentials are available"
                },
            )
            .with_details(details)
            .with_data(Map::from_iter([
                ("api_key_present".to_string(), json!(api_key_present)),
                ("auth_token_present".to_string(), json!(auth_token_present)),
                ("saved_oauth_present".to_string(), json!(true)),
                ("saved_oauth_expired".to_string(), json!(expired)),
                (
                    "saved_oauth_expires_at".to_string(),
                    json!(token_set.expires_at),
                ),
                (
                    "refresh_token_present".to_string(),
                    json!(token_set.refresh_token.is_some()),
                ),
                ("scopes".to_string(), json!(token_set.scopes)),
            ]))
        }
        Ok(None) => DiagnosticCheck::new(
            "Auth",
            if api_key_present || auth_token_present {
                DiagnosticLevel::Ok
            } else {
                DiagnosticLevel::Warn
            },
            if api_key_present || auth_token_present {
                "environment credentials are configured"
            } else {
                "no API key or saved OAuth credentials were found"
            },
        )
        .with_details(vec![format!(
            "Environment       api_key={} auth_token={}",
            if api_key_present { "present" } else { "absent" },
            if auth_token_present {
                "present"
            } else {
                "absent"
            }
        )])
        .with_data(Map::from_iter([
            ("api_key_present".to_string(), json!(api_key_present)),
            ("auth_token_present".to_string(), json!(auth_token_present)),
            ("saved_oauth_present".to_string(), json!(false)),
            ("saved_oauth_expired".to_string(), json!(false)),
            ("saved_oauth_expires_at".to_string(), Value::Null),
            ("refresh_token_present".to_string(), json!(false)),
            ("scopes".to_string(), json!(Vec::<String>::new())),
        ])),
        Err(error) => DiagnosticCheck::new(
            "Auth",
            DiagnosticLevel::Fail,
            format!("failed to inspect saved credentials: {error}"),
        )
        .with_data(Map::from_iter([
            ("api_key_present".to_string(), json!(api_key_present)),
            ("auth_token_present".to_string(), json!(auth_token_present)),
            ("saved_oauth_present".to_string(), Value::Null),
            ("saved_oauth_expired".to_string(), Value::Null),
            ("saved_oauth_expires_at".to_string(), Value::Null),
            ("refresh_token_present".to_string(), Value::Null),
            ("scopes".to_string(), Value::Null),
            ("saved_oauth_error".to_string(), json!(error.to_string())),
        ])),
    }
}

fn check_config_health(
    config_loader: &ConfigLoader,
    config: Result<&runtime::RuntimeConfig, &runtime::ConfigError>,
) -> DiagnosticCheck {
    let discovered = config_loader.discover();
    let discovered_count = discovered.len();
    // Separate candidate paths that actually exist from those that don't.
    // Showing non-existent paths as "Discovered file" implies they loaded
    // but something went wrong, which is confusing. We only surface paths
    // that exist on disk as discovered; non-existent ones are silently
    // omitted from the display (they are just the standard search locations).
    let present_paths: Vec<String> = discovered
        .iter()
        .filter(|e| e.path.exists())
        .map(|e| e.path.display().to_string())
        .collect();
    let discovered_paths = discovered
        .iter()
        .map(|entry| entry.path.display().to_string())
        .collect::<Vec<_>>();
    match config {
        Ok(runtime_config) => {
            let loaded_entries = runtime_config.loaded_entries();
            let loaded_count = loaded_entries.len();
            let present_count = present_paths.len();
            let mut details = vec![format!(
                "Config files      loaded {}/{}",
                loaded_count, present_count
            )];
            if let Some(model) = runtime_config.model() {
                details.push(format!("Resolved model    {model}"));
            }
            details.push(format!(
                "MCP servers       {}",
                runtime_config.mcp().servers().len()
            ));
            if present_paths.is_empty() {
                details.push("Discovered files  <none> (defaults active)".to_string());
            } else {
                details.extend(
                    present_paths
                        .iter()
                        .map(|path| format!("Discovered file   {path}")),
                );
            }
            DiagnosticCheck::new(
                "Config",
                DiagnosticLevel::Ok,
                if present_count == 0 {
                    "no config files present; defaults are active"
                } else {
                    "runtime config loaded successfully"
                },
            )
            .with_details(details)
            .with_data(Map::from_iter([
                ("discovered_files".to_string(), json!(present_paths)),
                ("discovered_files_count".to_string(), json!(present_count)),
                ("loaded_config_files".to_string(), json!(loaded_count)),
                ("resolved_model".to_string(), json!(runtime_config.model())),
                (
                    "mcp_servers".to_string(),
                    json!(runtime_config.mcp().servers().len()),
                ),
            ]))
        }
        Err(error) => DiagnosticCheck::new(
            "Config",
            DiagnosticLevel::Fail,
            format!("runtime config failed to load: {error}"),
        )
        .with_details(if discovered_paths.is_empty() {
            vec!["Discovered files  <none>".to_string()]
        } else {
            discovered_paths
                .iter()
                .map(|path| format!("Discovered file   {path}"))
                .collect()
        })
        .with_data(Map::from_iter([
            ("discovered_files".to_string(), json!(discovered_paths)),
            (
                "discovered_files_count".to_string(),
                json!(discovered_count),
            ),
            ("loaded_config_files".to_string(), json!(0)),
            ("resolved_model".to_string(), Value::Null),
            ("mcp_servers".to_string(), Value::Null),
            ("load_error".to_string(), json!(error.to_string())),
        ])),
    }
}

fn check_workspace_health(context: &StatusContext) -> DiagnosticCheck {
    let in_repo = context.project_root.is_some();
    DiagnosticCheck::new(
        "Workspace",
        if in_repo {
            DiagnosticLevel::Ok
        } else {
            DiagnosticLevel::Warn
        },
        if in_repo {
            format!(
                "project root detected on branch {}",
                context.git_branch.as_deref().unwrap_or("unknown")
            )
        } else {
            "current directory is not inside a git project".to_string()
        },
    )
    .with_details(vec![
        format!("Cwd              {}", context.cwd.display()),
        format!(
            "Project root     {}",
            context
                .project_root
                .as_ref()
                .map_or_else(|| "<none>".to_string(), |path| path.display().to_string())
        ),
        format!(
            "Git branch       {}",
            context.git_branch.as_deref().unwrap_or("unknown")
        ),
        format!("Git state        {}", context.git_summary.headline()),
        format!("Changed files    {}", context.git_summary.changed_files),
        format!(
            "Memory files     {} · config files loaded {}/{}",
            context.memory_file_count, context.loaded_config_files, context.discovered_config_files
        ),
    ])
    .with_data(Map::from_iter([
        ("cwd".to_string(), json!(context.cwd.display().to_string())),
        (
            "project_root".to_string(),
            json!(context
                .project_root
                .as_ref()
                .map(|path| path.display().to_string())),
        ),
        ("in_git_repo".to_string(), json!(in_repo)),
        ("git_branch".to_string(), json!(context.git_branch)),
        (
            "git_state".to_string(),
            json!(context.git_summary.headline()),
        ),
        (
            "changed_files".to_string(),
            json!(context.git_summary.changed_files),
        ),
        (
            "memory_file_count".to_string(),
            json!(context.memory_file_count),
        ),
        (
            "loaded_config_files".to_string(),
            json!(context.loaded_config_files),
        ),
        (
            "discovered_config_files".to_string(),
            json!(context.discovered_config_files),
        ),
    ]))
}

fn check_sandbox_health(status: &runtime::SandboxStatus) -> DiagnosticCheck {
    let degraded = status.enabled && !status.active;
    let mut details = vec![
        format!("Enabled          {}", status.enabled),
        format!("Active           {}", status.active),
        format!("Supported        {}", status.supported),
        format!("Filesystem mode  {}", status.filesystem_mode.as_str()),
        format!("Filesystem live  {}", status.filesystem_active),
    ];
    if let Some(reason) = &status.fallback_reason {
        details.push(format!("Fallback reason  {reason}"));
    }
    DiagnosticCheck::new(
        "Sandbox",
        if degraded {
            DiagnosticLevel::Warn
        } else {
            DiagnosticLevel::Ok
        },
        if degraded {
            "sandbox was requested but is not currently active"
        } else if status.active {
            "sandbox protections are active"
        } else {
            "sandbox is not active for this session"
        },
    )
    .with_details(details)
    .with_data(Map::from_iter([
        ("enabled".to_string(), json!(status.enabled)),
        ("active".to_string(), json!(status.active)),
        ("supported".to_string(), json!(status.supported)),
        (
            "namespace_supported".to_string(),
            json!(status.namespace_supported),
        ),
        (
            "namespace_active".to_string(),
            json!(status.namespace_active),
        ),
        (
            "network_supported".to_string(),
            json!(status.network_supported),
        ),
        ("network_active".to_string(), json!(status.network_active)),
        (
            "filesystem_mode".to_string(),
            json!(status.filesystem_mode.as_str()),
        ),
        (
            "filesystem_active".to_string(),
            json!(status.filesystem_active),
        ),
        ("allowed_mounts".to_string(), json!(status.allowed_mounts)),
        ("in_container".to_string(), json!(status.in_container)),
        (
            "container_markers".to_string(),
            json!(status.container_markers),
        ),
        ("fallback_reason".to_string(), json!(status.fallback_reason)),
    ]))
}

fn check_system_health(cwd: &Path, config: Option<&runtime::RuntimeConfig>) -> DiagnosticCheck {
    let default_model = config.and_then(runtime::RuntimeConfig::model);
    let mut details = vec![
        format!("OS               {} {}", env::consts::OS, env::consts::ARCH),
        format!("Working dir      {}", cwd.display()),
        format!("Version          {}", VERSION),
        format!("Build target     {}", BUILD_TARGET.unwrap_or("<unknown>")),
        format!("Git SHA          {}", GIT_SHA.unwrap_or("<unknown>")),
    ];
    if let Some(model) = default_model {
        details.push(format!("Default model    {model}"));
    }
    DiagnosticCheck::new(
        "System",
        DiagnosticLevel::Ok,
        "captured local runtime metadata",
    )
    .with_details(details)
    .with_data(Map::from_iter([
        ("os".to_string(), json!(env::consts::OS)),
        ("arch".to_string(), json!(env::consts::ARCH)),
        ("working_dir".to_string(), json!(cwd.display().to_string())),
        ("version".to_string(), json!(VERSION)),
        ("build_target".to_string(), json!(BUILD_TARGET)),
        ("git_sha".to_string(), json!(GIT_SHA)),
        ("default_model".to_string(), json!(default_model)),
    ]))
}

fn resume_command_can_absorb_token(current_command: &str, token: &str) -> bool {
    matches!(
        SlashCommand::parse(current_command),
        Ok(Some(SlashCommand::Export { path: None }))
    ) && !looks_like_slash_command_token(token)
}

fn looks_like_slash_command_token(token: &str) -> bool {
    let trimmed = token.trim_start();
    let Some(name) = trimmed.strip_prefix('/').and_then(|value| {
        value
            .split_whitespace()
            .next()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }) else {
        return false;
    };

    slash_command_specs()
        .iter()
        .any(|spec| spec.name == name || spec.aliases.contains(&name))
}

fn dump_manifests(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    let workspace_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    // Surface the resolved path in the error so users can diagnose missing
    // manifest files without guessing what path the binary expected.
    // ROADMAP #45: this path is only correct when running from the build tree;
    // a proper fix would ship manifests alongside the binary.
    let resolved = workspace_dir
        .canonicalize()
        .unwrap_or_else(|_| workspace_dir.clone());
    let paths = UpstreamPaths::from_workspace_dir(&workspace_dir);
    match extract_manifest(&paths) {
        Ok(manifest) => {
            match output_format {
                CliOutputFormat::Text => {
                    println!("commands: {}", manifest.commands.entries().len());
                    println!("tools: {}", manifest.tools.entries().len());
                    println!("bootstrap phases: {}", manifest.bootstrap.phases().len());
                }
                CliOutputFormat::Json | CliOutputFormat::StreamJson => println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "kind": "dump-manifests",
                        "commands": manifest.commands.entries().len(),
                        "tools": manifest.tools.entries().len(),
                        "bootstrap_phases": manifest.bootstrap.phases().len(),
                    }))?
                ),
            }
            Ok(())
        }
        Err(error) => Err(format!(
            "failed to extract manifests: {error}\n  looked in: {}",
            resolved.display()
        )
        .into()),
    }
}

fn print_bootstrap_plan(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    let phases = runtime::BootstrapPlan::Himalaya_code_default()
        .phases()
        .iter()
        .map(|phase| format!("{phase:?}"))
        .collect::<Vec<_>>();
    match output_format {
        CliOutputFormat::Text => {
            for phase in &phases {
                println!("- {phase}");
            }
        }
        CliOutputFormat::Json | CliOutputFormat::StreamJson => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "kind": "bootstrap-plan",
                "phases": phases,
            }))?
        ),
    }
    Ok(())
}

fn output_structured_report(
    value: Value,
    output_format: CliOutputFormat,
    render_text: impl FnOnce(&Value) -> String,
) -> Result<(), Box<dyn std::error::Error>> {
    match output_format {
        CliOutputFormat::Text => println!("{}", render_text(&value)),
        CliOutputFormat::Json => println!("{}", serde_json::to_string_pretty(&value)?),
        CliOutputFormat::StreamJson => print_stream_json_event(value),
    }
    Ok(())
}

fn infer_task_capabilities(prompt: &str) -> Vec<String> {
    let normalized = prompt.to_ascii_lowercase();
    let mut capabilities = BTreeSet::new();
    for (needles, capability) in [
        (
            &["read", "inspect", "analyze", "summarize", "review"][..],
            "read",
        ),
        (&["write", "create", "add", "generate"], "write"),
        (&["edit", "modify", "fix", "refactor", "update"], "edit"),
        (&["search", "find", "grep", "glob", "locate"], "search"),
        (&["test", "verify", "validate", "benchmark"], "test"),
        (&["bash", "shell", "command", "run", "build"], "shell"),
        (&["git", "commit", "branch", "diff", "pr"], "git"),
        (&["mcp", "server", "resource"], "mcp"),
        (&["agent", "worker", "task", "delegate"], "agent"),
        (&["model", "moe", "route", "reasoning"], "model"),
    ] {
        if needles.iter().any(|needle| normalized.contains(needle)) {
            capabilities.insert(capability.to_string());
        }
    }
    if capabilities.is_empty() {
        capabilities.insert("read".to_string());
        capabilities.insert("search".to_string());
    }
    capabilities.into_iter().collect()
}

fn estimate_task_complexity(prompt: &str, capabilities: &[String]) -> u8 {
    let word_count = prompt.split_whitespace().count();
    let mut complexity = 1_u8;
    if word_count > 12 {
        complexity += 1;
    }
    if word_count > 32 {
        complexity += 1;
    }
    if capabilities.len() > 2 {
        complexity += 1;
    }
    if prompt.contains("and") || prompt.contains('，') || prompt.contains(',') {
        complexity += 1;
    }
    complexity.clamp(1, 5)
}

fn build_plan_output(
    prompt: &str,
    permission_mode: PermissionMode,
) -> Result<Value, Box<dyn std::error::Error>> {
    let capabilities = infer_task_capabilities(prompt);
    let task = runtime::Task::new(
        "plan-local",
        prompt.trim().to_string(),
        estimate_task_complexity(prompt, &capabilities),
        capabilities,
        vec![format!("permission-mode:{}", permission_mode.as_str())],
    );
    let available_tools = mvp_tool_specs()
        .into_iter()
        .map(|spec| {
            runtime::tool_from_profile(spec.name, Some(spec.description), Some(&spec.input_schema))
        })
        .collect::<Vec<_>>();
    let reasoning_context = runtime::ReasoningContext {
        workspace_root: env::current_dir().ok(),
        active_constraints: task.constraints.clone(),
        max_parallelism: 4,
        ..runtime::ReasoningContext::default()
    };
    let engine = runtime::DecisioningEngine::new(
        runtime::ToolSelector::new(available_tools, reasoning_context),
        runtime::TaskPlanner::new(4),
        runtime::SafetyPolicy::default(),
    );
    let snapshot = engine.analyze(&task);
    Ok(json!({
        "type": "plan",
        "task": snapshot.task,
        "selected_tools": snapshot.selected_tools.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>(),
        "plan": snapshot.plan,
        "risk": snapshot.risk,
        "events": snapshot.events,
    }))
}

fn render_plan_text(value: &Value) -> String {
    let task = value["task"]["description"].as_str().unwrap_or("<unknown>");
    let mode = value["plan"]["execution_mode"].as_object().map_or_else(
        || {
            value["plan"]["execution_mode"]
                .as_str()
                .unwrap_or("serial")
                .to_string()
        },
        |object| {
            object
                .keys()
                .next()
                .cloned()
                .unwrap_or_else(|| "parallel".to_string())
        },
    );
    let confidence = value["plan"]["confidence"].as_f64().unwrap_or_default();
    let selected_tools = value["selected_tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|tools| !tools.is_empty())
        .unwrap_or_else(|| "<none>".to_string());
    let risk_score = value["risk"]["score"].as_f64().unwrap_or_default();
    let risk_outcome = value["risk"]["outcome"].as_str().unwrap_or("Review");

    let mut lines = vec![
        "Plan".to_string(),
        format!("Task: {task}"),
        format!("Execution: {mode}"),
        format!("Confidence: {:.0}%", confidence * 100.0),
        format!("Selected tools: {selected_tools}"),
        format!("Risk: {risk_outcome} ({risk_score:.2})"),
        "Steps:".to_string(),
    ];
    if let Some(steps) = value["plan"]["steps"].as_array() {
        for step in steps {
            let id = step["id"].as_str().unwrap_or("step");
            let title = step["title"].as_str().unwrap_or("Untitled step");
            let tools = step["candidate_tools"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|items| !items.is_empty())
                .unwrap_or_else(|| "<none>".to_string());
            lines.push(format!("  - {id}: {title} [{tools}]"));
        }
    }
    if let Some(reasons) = value["risk"]["reasons"].as_array() {
        if !reasons.is_empty() {
            lines.push("Risk notes:".to_string());
            lines.extend(
                reasons
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|reason| format!("  - {reason}")),
            );
        }
    }
    lines.join("\n")
}

fn run_plan_command(
    prompt: &str,
    output_format: CliOutputFormat,
    permission_mode: PermissionMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let value = build_plan_output(prompt, permission_mode)?;
    output_structured_report(value, output_format, render_plan_text)
}

fn render_benchmark_list_text(value: &Value) -> String {
    let suite_id = value["suite_id"].as_str().unwrap_or("benchmark-suite");
    let version = value["version"].as_str().unwrap_or("unknown");
    let mut lines = vec![format!("Benchmark suite: {suite_id} ({version})")];
    if let Some(tasks) = value["tasks"].as_array() {
        for task in tasks {
            let id = task["id"].as_str().unwrap_or("task");
            let title = task["title"].as_str().unwrap_or("Untitled task");
            let category = task["category"].as_str().unwrap_or("uncategorized");
            let complexity = task["complexity"].as_u64().unwrap_or_default();
            lines.push(format!("  - {id}: {title} [{category}, c{complexity}]"));
        }
    }
    lines.join("\n")
}

fn render_benchmark_task_text(value: &Value) -> String {
    let task = &value["task"];
    let id = task["id"].as_str().unwrap_or("task");
    let title = task["title"].as_str().unwrap_or("Untitled task");
    let objective = task["objective"].as_str().unwrap_or_default();
    let mut lines = vec![
        format!("Benchmark task: {id}"),
        format!("Title: {title}"),
        format!("Objective: {objective}"),
    ];
    if let Some(capabilities) = task["expected_capabilities"].as_array() {
        let capabilities = capabilities
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("Capabilities: {capabilities}"));
    }
    if let Some(tests) = task["acceptance_tests"].as_array() {
        lines.push("Acceptance tests:".to_string());
        for test in tests.iter().filter_map(Value::as_str) {
            lines.push(format!("  - {test}"));
        }
    }
    lines.join("\n")
}

fn render_benchmark_run_text(value: &Value) -> String {
    let summary = &value["run"]["summary"];
    let total_tasks = summary["total_tasks"].as_u64().unwrap_or_default();
    let average = summary["average_total_score"].as_f64().unwrap_or_default() * 100.0;
    let coverage = summary["average_capability_coverage"]
        .as_f64()
        .unwrap_or_default()
        * 100.0;
    let adaptive_routing = summary["average_adaptive_routing_quality_score"]
        .as_f64()
        .unwrap_or_default()
        * 100.0;
    let parallel = summary["parallel_plans"].as_u64().unwrap_or_default();
    let review = summary["review_or_deny_tasks"].as_u64().unwrap_or_default();
    let mut lines = vec![
        format!("Benchmark run: {total_tasks} task(s)"),
        format!("Average score: {average:.0}%"),
        format!("Capability coverage: {coverage:.0}%"),
        format!("Adaptive routing quality: {adaptive_routing:.0}%"),
        format!("Parallel plans: {parallel}"),
        format!("Review/deny tasks: {review}"),
    ];
    if let Some(record_path) = value["record_path"].as_str() {
        lines.push(format!("Record: {record_path}"));
    }
    if let Some(results) = value["run"]["results"].as_array() {
        lines.push("Results:".to_string());
        for result in results {
            let id = result["task_id"].as_str().unwrap_or("task");
            let score = result["score"]["total"].as_f64().unwrap_or_default() * 100.0;
            let mode = result["execution_mode"].as_str().unwrap_or("serial");
            let steps = result["plan_steps"].as_u64().unwrap_or_default();
            lines.push(format!("  - {id}: {score:.0}% ({mode}, {steps} steps)"));
        }
    }
    lines.join("\n")
}

fn benchmark_record_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    Ok(cwd.join(".Himalaya").join("benchmarks"))
}

fn record_benchmark_run(
    run: &runtime::BenchmarkRun,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let dir = benchmark_record_dir()?;
    fs::create_dir_all(&dir)?;
    let path = dir.join("runs.jsonl");
    let recorded_at = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let line = serde_json::to_string(&json!({
        "recorded_at": recorded_at,
        "run": run,
    }))?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(file, "{line}")?;
    Ok(path)
}

fn record_autonomous_benchmark_run(
    run: &runtime::AutonomousBenchmarkRun,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let dir = benchmark_record_dir()?;
    fs::create_dir_all(&dir)?;
    let path = dir.join("runs.jsonl");
    let recorded_at = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let line = serde_json::to_string(&json!({
        "recorded_at": recorded_at,
        "kind": "autonomous_benchmark",
        "run": run,
    }))?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(file, "{line}")?;
    Ok(path)
}

fn build_autonomous_evaluation_input(
    limit: usize,
    max_ticks: usize,
    permission_mode: PermissionMode,
) -> Result<runtime::AutonomousEvaluationInput, Box<dyn std::error::Error>> {
    let registry = load_task_registry()?;
    let tasks = registry.list(None);
    let loaded_memory =
        load_task_memory_store().unwrap_or_else(|_| runtime::TaskMemoryStore::new());
    let task_memory = if loaded_memory.entries().is_empty() && !tasks.is_empty() {
        runtime::TaskMemoryStore::from_tasks(&tasks)
    } else {
        loaded_memory
    };
    let route_feedback = load_route_feedback_store()?;
    let autonomous_runs =
        runtime::load_autonomous_run_reports_with_diagnostics(&scheduler_state_dir()?, limit)?;
    let policy_replay = runtime::replay_policy_lifecycle(&policy_governance_dir()?, limit).ok();
    Ok(runtime::AutonomousEvaluationInput {
        tasks,
        task_memory,
        route_feedback,
        autonomous_runs,
        permission_mode,
        requested_max_ticks: max_ticks,
        policy_replay,
    })
}

fn build_autonomous_integration_input(
    limit: usize,
    max_ticks: usize,
    permission_mode: PermissionMode,
) -> Result<runtime::AutonomousIntegrationInput, Box<dyn std::error::Error>> {
    let registry = load_task_registry()?;
    let worker_registry = load_worker_registry()?;
    let tasks = registry.list(None);
    let loaded_memory =
        load_task_memory_store().unwrap_or_else(|_| runtime::TaskMemoryStore::new());
    let task_memory = if loaded_memory.entries().is_empty() && !tasks.is_empty() {
        runtime::TaskMemoryStore::from_tasks(&tasks)
    } else {
        loaded_memory
    };
    let route_feedback = load_route_feedback_store()?;
    let routing_proposals = runtime::load_routing_policy_proposals(&route_feedback_dir()?)
        .map(|snapshot| snapshot.proposals)
        .unwrap_or_default();
    let scheduler = runtime::DurableTaskScheduler::with_workers(
        registry.clone(),
        runtime::VerificationRunner::new(Some(env::current_dir()?)),
        worker_registry.clone(),
    )
    .with_permission_mode(permission_mode);
    let daemon = runtime::SchedulerDaemon::new(scheduler.clone(), scheduler_state_dir()?);
    let scheduler_state = daemon.load_state().ok();
    let scheduler_events = daemon.load_events().unwrap_or_default();
    let scheduler_queue = scheduler.queue();
    let policy_dir = policy_governance_dir()?;
    let policy_ledger = runtime::PolicyGovernanceLedger::new(&policy_dir)
        .load(limit)
        .ok();
    let policy_replay = runtime::replay_policy_lifecycle(&policy_dir, limit).ok();
    let autonomous_runs =
        runtime::load_autonomous_run_reports_with_diagnostics(&scheduler_state_dir()?, limit)?;
    let evaluation = Some(runtime::evaluate_autonomous_loop(
        runtime::AutonomousEvaluationInput {
            tasks: tasks.clone(),
            task_memory: task_memory.clone(),
            route_feedback: route_feedback.clone(),
            autonomous_runs: autonomous_runs.clone(),
            permission_mode,
            requested_max_ticks: max_ticks,
            policy_replay: policy_replay.clone(),
        },
    ));
    Ok(runtime::AutonomousIntegrationInput {
        tasks,
        task_ledger: registry.ledger(),
        task_events: registry.event_log(),
        scheduler_state,
        scheduler_events,
        scheduler_queue,
        workers: worker_registry.list(),
        task_memory,
        route_feedback,
        routing_proposals,
        policy_ledger,
        policy_replay,
        autonomous_runs,
        evaluation,
    })
}

#[derive(Debug, Clone)]
struct AutonomousIntegrationReview {
    integration: runtime::AutonomousIntegrationReport,
    health: runtime::AutonomousHealthView,
}

fn build_autonomous_integration_review(
    limit: usize,
    max_ticks: usize,
    permission_mode: PermissionMode,
) -> Result<AutonomousIntegrationReview, Box<dyn std::error::Error>> {
    let integration = runtime::review_autonomous_integration(build_autonomous_integration_input(
        limit,
        max_ticks,
        permission_mode,
    )?);
    let health = runtime::autonomous_health_view(&integration);
    Ok(AutonomousIntegrationReview {
        integration,
        health,
    })
}

fn autonomous_preflight_blocked_value(
    operation: &str,
    review: &AutonomousIntegrationReview,
) -> Value {
    json!({
        "type": "autonomous_preflight_blocked",
        "operation": operation,
        "status": review.health.status,
        "next_action": review.health.next_action,
        "safe_to_iterate": review.health.safe_to_iterate,
        "safe_to_apply_policy": review.health.safe_to_apply_policy,
        "health": review.health,
        "integration_summary": review.integration.summary,
        "recommendations": review.integration.recommendations,
    })
}

fn render_benchmark_value_text(value: &Value) -> String {
    match value["type"].as_str() {
        Some("benchmark_suite") => render_benchmark_list_text(value),
        Some("benchmark_task") => render_benchmark_task_text(value),
        Some("benchmark_run") => render_benchmark_run_text(value),
        Some("benchmark_autonomous") => render_autonomous_benchmark_text(value),
        _ => serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
    }
}

fn benchmark_command_value(
    command: BenchmarkCliCommand,
    model: &str,
) -> Result<Value, Box<dyn std::error::Error>> {
    match command {
        BenchmarkCliCommand::List => {
            let suite = runtime::complex_coding_benchmark_suite();
            Ok(json!({
                "type": "benchmark_suite",
                "suite_id": suite.suite_id,
                "version": suite.version,
                "description": suite.description,
                "tasks": suite.tasks,
            }))
        }
        BenchmarkCliCommand::Show { task_id } => {
            let suite = runtime::complex_coding_benchmark_suite();
            let task = suite
                .tasks
                .into_iter()
                .find(|task| task.id == task_id)
                .ok_or_else(|| format!("benchmark task not found: {task_id}"))?;
            Ok(json!({"type":"benchmark_task","task":task}))
        }
        BenchmarkCliCommand::Run {
            record,
            max_parallelism,
        } => {
            let run = runtime::run_complex_coding_benchmark(model, max_parallelism);
            let record_path = if record {
                Some(record_benchmark_run(&run)?.to_string_lossy().to_string())
            } else {
                None
            };
            Ok(json!({
                "type": "benchmark_run",
                "run": run,
                "record_path": record_path,
            }))
        }
        BenchmarkCliCommand::Autonomous {
            record,
            limit,
            max_ticks,
            optimize_routes,
        } => {
            let input =
                build_autonomous_evaluation_input(limit, max_ticks, PermissionMode::ReadOnly)?;
            let run = runtime::run_autonomous_benchmark(input);
            let route_optimizer = if optimize_routes {
                let store = load_route_feedback_store()?;
                Some(json!({
                    "report": runtime::evaluate_routing_feedback(&store, 2, 0.5),
                    "replay": runtime::replay_routing_optimizer(
                        runtime::MoERoutingPolicy::balanced(model),
                        &store,
                        2,
                        0.5,
                    ),
                }))
            } else {
                None
            };
            let record_path = if record {
                Some(
                    record_autonomous_benchmark_run(&run)?
                        .to_string_lossy()
                        .to_string(),
                )
            } else {
                None
            };
            Ok(json!({
                "type": "benchmark_autonomous",
                "run": run,
                "route_optimizer": route_optimizer,
                "record_path": record_path,
                "runs_path": scheduler_state_dir()?.join("runs.jsonl"),
            }))
        }
    }
}

fn run_benchmark_command(
    command: BenchmarkCliCommand,
    output_format: CliOutputFormat,
    model: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let value = benchmark_command_value(command, model)?;
    output_structured_report(value, output_format, render_benchmark_value_text)
}

fn maturity_level_for_tool(name: &str) -> &'static str {
    match name {
        "bash" | "read_file" | "write_file" | "edit_file" | "glob_search" | "grep_search"
        | "TaskCreate" | "TaskGet" | "TaskList" | "TaskStop" | "TaskUpdate" | "TaskOutput"
        | "CronCreate" | "CronDelete" | "CronList" | "LSP" | "MCP" | "ListMcpResources"
        | "ReadMcpResource" | "ToolSearch" | "Sleep" | "StructuredOutput" => "good",
        "WebFetch" | "WebSearch" | "TodoWrite" | "Skill" | "Agent" | "NotebookEdit" | "Config"
        | "REPL" | "PowerShell" => "moderate",
        "AskUserQuestion" | "McpAuth" | "RemoteTrigger" => "partial",
        _ => "implemented",
    }
}

fn maturity_matrix_value() -> Value {
    let tools = mvp_tool_specs()
        .into_iter()
        .map(|spec| {
            json!({
                "name": spec.name,
                "required_permission": spec.required_permission.as_str(),
                "maturity": maturity_level_for_tool(spec.name),
                "description": spec.description,
            })
        })
        .collect::<Vec<_>>();
    let slash_commands = slash_command_specs()
        .iter()
        .map(|spec| {
            let status = slash_command_status(spec.name);
            let implemented = status == SlashCommandStatus::Implemented;
            json!({
                "name": spec.name,
                "aliases": spec.aliases,
                "argument_hint": spec.argument_hint,
                "resume_supported": spec.resume_supported && implemented,
                "status": match status {
                    SlashCommandStatus::Implemented => "implemented",
                    SlashCommandStatus::Stub => "stub",
                },
                "implemented": implemented,
                "maturity": if implemented { "implemented" } else { "stub" },
                "summary": spec.summary,
            })
        })
        .collect::<Vec<_>>();
    let implemented_commands = slash_command_specs()
        .iter()
        .filter(|spec| slash_command_status(spec.name) == SlashCommandStatus::Implemented)
        .count();
    json!({
        "type": "maturity_matrix",
        "tool_count": tools.len(),
        "slash_command_count": slash_commands.len(),
        "implemented_slash_command_count": implemented_commands,
        "stub_slash_command_count": slash_commands.len().saturating_sub(implemented_commands),
        "release_readiness": release_readiness_value(),
        "tools": tools,
        "slash_commands": slash_commands,
    })
}

fn release_readiness_value() -> Value {
    json!({
        "status": "converging",
        "goal": "stabilize runtime correctness, normal user interaction, and clear autonomous diagnostics before adding new autonomous loops",
        "stable_capabilities": [
            "durable task registry and scheduler queue",
            "worker lifecycle supervision and recovery diagnostics",
            "task memory and route feedback stores",
            "policy governance ledger, replay, plan, dry-run apply, and rollback",
            "autonomous evaluation, trace replay, integration report, and health view",
            "daemon status/logs health checkpoints",
            "mutating autonomous operation preflight gates",
            "JSON and stream-json command contracts for daemon status/logs/report/evaluate"
        ],
        "experimental_capabilities": [
            "fully unattended long-horizon daemon execution",
            "automatic governed policy apply without human review",
            "cross-domain optimizer decisions beyond dry-run evidence",
            "large-scale concurrent worker pools"
        ],
        "operator_workflow": [
            "inspect `Himalaya tasks daemon status` for a lightweight health checkpoint",
            "run report/evaluate/replay before starting a bounded daemon loop",
            "use `Himalaya policy apply --dry-run` before any persistent governed apply",
            "treat `autonomous_preflight_blocked` as a recovery instruction, not a command failure"
        ],
        "recommended_smoke_tests": [
            "cargo fmt -- --check",
            "cargo check -p runtime",
            "cargo check -p rusty-Himalaya-cli",
            "cargo test -p runtime autonomous_integration --no-fail-fast",
            "cargo test -p rusty-Himalaya-cli --test output_format_contract --no-fail-fast",
            "cargo test -p rusty-Himalaya-cli --test stream_json_contract --no-fail-fast",
            "Himalaya --output-format json tasks daemon status",
            "Himalaya --output-format json tasks daemon logs --limit 1",
            "Himalaya --output-format json tasks daemon report --limit 20 --max-ticks 3",
            "Himalaya --output-format json tasks daemon evaluate --limit 20 --max-ticks 3",
            "Himalaya --output-format json tasks daemon replay --limit 20 --max-ticks 3"
        ],
        "release_gates": [
            "no failed integration health blockers",
            "daemon status/logs/report/evaluate text gives next action before detailed counters",
            "tasks daemon start is preflight-blocked unless health allows iteration",
            "policy apply is dry-run or preflight-blocked unless health is healthy",
            "stream-json schema remains backward compatible"
        ]
    })
}

fn print_maturity_matrix(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    output_structured_report(
        maturity_matrix_value(),
        output_format,
        render_maturity_matrix_text,
    )
}

fn render_maturity_matrix_text(value: &Value) -> String {
    let tool_count = value["tool_count"].as_u64().unwrap_or_default();
    let command_count = value["slash_command_count"].as_u64().unwrap_or_default();
    let implemented = value["implemented_slash_command_count"]
        .as_u64()
        .unwrap_or_default();
    let stubs = value["stub_slash_command_count"]
        .as_u64()
        .unwrap_or_default();
    let mut tool_levels = BTreeMap::<String, usize>::new();
    if let Some(tools) = value["tools"].as_array() {
        for tool in tools {
            if let Some(level) = tool["maturity"].as_str() {
                *tool_levels.entry(level.to_string()).or_default() += 1;
            }
        }
    }
    let mut lines = vec![
        "Maturity matrix".to_string(),
        format!("Tools: {tool_count}"),
        format!("Slash commands: {implemented}/{command_count} implemented, {stubs} stub"),
        "Tool maturity:".to_string(),
    ];
    lines.extend(
        tool_levels
            .into_iter()
            .map(|(level, count)| format!("  - {level}: {count}")),
    );
    if let Some(readiness) = value["release_readiness"].as_object() {
        lines.push("Release readiness:".to_string());
        if let Some(status) = readiness.get("status").and_then(Value::as_str) {
            lines.push(format!("  Status {status}"));
        }
        if let Some(gates) = readiness.get("release_gates").and_then(Value::as_array) {
            lines.push("  Gates".to_string());
            lines.extend(
                gates
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|gate| format!("    - {gate}")),
            );
        }
        if let Some(workflow) = readiness.get("operator_workflow").and_then(Value::as_array) {
            lines.push("  Operator workflow".to_string());
            lines.extend(
                workflow
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|step| format!("    - {step}")),
            );
        }
    }
    lines.push("Implemented slash commands:".to_string());
    if let Some(commands) = value["slash_commands"].as_array() {
        lines.extend(
            commands
                .iter()
                .filter(|command| command["implemented"].as_bool().unwrap_or(false))
                .filter_map(|command| command["name"].as_str())
                .map(|name| format!("  - /{name}")),
        );
    }
    lines.join("\n")
}

fn default_oauth_config() -> OAuthConfig {
    OAuthConfig {
        client_id: String::from("9d1c250a-e61b-44d9-88ed-5944d1962f5e"),
        authorize_url: String::from("https://platform.Himalaya.com/oauth/authorize"),
        token_url: String::from("https://platform.Himalaya.com/v1/oauth/token"),
        callback_port: None,
        manual_redirect_url: None,
        scopes: vec![
            String::from("user:profile"),
            String::from("user:inference"),
            String::from("user:sessions:Himalaya_code"),
        ],
    }
}

fn run_login(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let config = ConfigLoader::default_for(&cwd).load()?;
    let default_oauth = default_oauth_config();
    let oauth = config.oauth().unwrap_or(&default_oauth);
    let callback_port = oauth.callback_port.unwrap_or(DEFAULT_OAUTH_CALLBACK_PORT);
    let redirect_uri = runtime::loopback_redirect_uri(callback_port);
    let pkce = generate_pkce_pair()?;
    let state = generate_state()?;
    let authorize_url =
        OAuthAuthorizationRequest::from_config(oauth, redirect_uri.clone(), state.clone(), &pkce)
            .build_url();

    if output_format == CliOutputFormat::Text {
        println!("Starting Himalaya OAuth login...");
        println!("Listening for callback on {redirect_uri}");
    }
    if let Err(error) = open_browser(&authorize_url) {
        emit_login_browser_open_failure(
            output_format,
            &authorize_url,
            &error,
            &mut io::stdout(),
            &mut io::stderr(),
        )?;
    }

    let callback = wait_for_oauth_callback(callback_port)?;
    if let Some(error) = callback.error {
        let description = callback
            .error_description
            .unwrap_or_else(|| "authorization failed".to_string());
        return Err(io::Error::other(format!("{error}: {description}")).into());
    }
    let code = callback.code.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "callback did not include code")
    })?;
    let returned_state = callback.state.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "callback did not include state")
    })?;
    if returned_state != state {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "oauth state mismatch").into());
    }

    let client = AnthropicClient::from_auth(AuthSource::None).with_base_url(api::read_base_url());
    let exchange_request = OAuthTokenExchangeRequest::from_config(
        oauth,
        code,
        state,
        pkce.verifier,
        redirect_uri.clone(),
    );
    let runtime = tokio::runtime::Runtime::new()?;
    let token_set = runtime.block_on(client.exchange_oauth_code(oauth, &exchange_request))?;
    save_oauth_credentials(&runtime::OAuthTokenSet {
        access_token: token_set.access_token,
        refresh_token: token_set.refresh_token,
        expires_at: token_set.expires_at,
        scopes: token_set.scopes,
    })?;
    match output_format {
        CliOutputFormat::Text => println!("Himalaya OAuth login complete."),
        CliOutputFormat::Json | CliOutputFormat::StreamJson => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "kind": "login",
                "callback_port": callback_port,
                "redirect_uri": redirect_uri,
                "message": "Himalaya OAuth login complete.",
            }))?
        ),
    }
    Ok(())
}

fn emit_login_browser_open_failure(
    output_format: CliOutputFormat,
    authorize_url: &str,
    error: &io::Error,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> io::Result<()> {
    writeln!(
        stderr,
        "warning: failed to open browser automatically: {error}"
    )?;
    match output_format {
        CliOutputFormat::Text => writeln!(stdout, "Open this URL manually:\n{authorize_url}"),
        CliOutputFormat::Json | CliOutputFormat::StreamJson => {
            writeln!(stderr, "Open this URL manually:\n{authorize_url}")
        }
    }
}

fn run_logout(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    clear_oauth_credentials()?;
    match output_format {
        CliOutputFormat::Text => println!("Himalaya OAuth credentials cleared."),
        CliOutputFormat::Json | CliOutputFormat::StreamJson => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "kind": "logout",
                "message": "Himalaya OAuth credentials cleared.",
            }))?
        ),
    }
    Ok(())
}

fn open_browser(url: &str) -> io::Result<()> {
    let commands = if cfg!(target_os = "macos") {
        vec![("open", vec![url])]
    } else if cfg!(target_os = "windows") {
        vec![("cmd", vec!["/C", "start", "", url])]
    } else {
        vec![("xdg-open", vec![url])]
    };
    for (program, args) in commands {
        match Command::new(program).args(args).spawn() {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no supported browser opener command found",
    ))
}

fn wait_for_oauth_callback(
    port: u16,
) -> Result<runtime::OAuthCallbackParams, Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let (mut stream, _) = listener.accept()?;
    let mut buffer = [0_u8; 4096];
    let bytes_read = stream.read(&mut buffer)?;
    let request = String::from_utf8_lossy(&buffer[..bytes_read]);
    let request_line = request.lines().next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing callback request line")
    })?;
    let target = request_line.split_whitespace().nth(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "missing callback request target",
        )
    })?;
    let callback = parse_oauth_callback_request_target(target)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let body = if callback.error.is_some() {
        "Himalaya OAuth login failed. You can close this window."
    } else {
        "Himalaya OAuth login succeeded. You can close this window."
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes())?;
    Ok(callback)
}

fn print_system_prompt(
    cwd: PathBuf,
    date: String,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let sections = load_system_prompt(cwd, date, env::consts::OS, "unknown")?;
    let message = sections.join(
        "

",
    );
    match output_format {
        CliOutputFormat::Text => println!("{message}"),
        CliOutputFormat::Json | CliOutputFormat::StreamJson => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "kind": "system-prompt",
                "message": message,
                "sections": sections,
            }))?
        ),
    }
    Ok(())
}

fn print_version(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    match output_format {
        CliOutputFormat::Text => println!("{}", render_version_report()),
        CliOutputFormat::Json | CliOutputFormat::StreamJson => {
            println!("{}", serde_json::to_string_pretty(&version_json_value())?);
        }
    }
    Ok(())
}

fn version_json_value() -> serde_json::Value {
    json!({
        "kind": "version",
        "message": render_version_report(),
        "version": VERSION,
        "git_sha": GIT_SHA,
        "target": BUILD_TARGET,
    })
}

fn resume_session(session_path: &Path, commands: &[String], output_format: CliOutputFormat) {
    let resolved_path = if session_path.exists() {
        session_path.to_path_buf()
    } else {
        match resolve_session_reference(&session_path.display().to_string()) {
            Ok(handle) => handle.path,
            Err(error) => {
                if output_format == CliOutputFormat::Json {
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "type": "error",
                            "error": format!("failed to restore session: {error}"),
                        })
                    );
                } else {
                    eprintln!("failed to restore session: {error}");
                }
                std::process::exit(1);
            }
        }
    };

    let session = match Session::load_from_path(&resolved_path) {
        Ok(session) => session,
        Err(error) => {
            if output_format == CliOutputFormat::Json {
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "type": "error",
                        "error": format!("failed to restore session: {error}"),
                    })
                );
            } else {
                eprintln!("failed to restore session: {error}");
            }
            std::process::exit(1);
        }
    };

    if commands.is_empty() {
        if output_format == CliOutputFormat::Json {
            println!(
                "{}",
                serde_json::json!({
                    "kind": "restored",
                    "session_id": session.session_id,
                    "path": resolved_path.display().to_string(),
                    "message_count": session.messages.len(),
                })
            );
        } else {
            println!(
                "Restored session from {} ({} messages).",
                resolved_path.display(),
                session.messages.len()
            );
        }
        return;
    }

    let mut session = session;
    for raw_command in commands {
        // The commands crate owns the implemented/stub truth source. Intercept
        // stubs before calling SlashCommand::parse so parse-less spec entries do
        // not produce circular "Did you mean /X?" errors.
        {
            let cmd_root = raw_command
                .trim_start_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or("");
            if is_stub_slash_command(cmd_root) {
                if output_format == CliOutputFormat::Json {
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "type": "error",
                            "error": format!("/{cmd_root} is not yet implemented in this build"),
                            "command": raw_command,
                        })
                    );
                } else {
                    eprintln!("/{cmd_root} is not yet implemented in this build");
                }
                std::process::exit(2);
            }
        }
        let command = match SlashCommand::parse(raw_command) {
            Ok(Some(command)) => command,
            Ok(None) => {
                if output_format == CliOutputFormat::Json {
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "type": "error",
                            "error": format!("unsupported resumed command: {raw_command}"),
                            "command": raw_command,
                        })
                    );
                } else {
                    eprintln!("unsupported resumed command: {raw_command}");
                }
                std::process::exit(2);
            }
            Err(error) => {
                if output_format == CliOutputFormat::Json {
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "type": "error",
                            "error": error.to_string(),
                            "command": raw_command,
                        })
                    );
                } else {
                    eprintln!("{error}");
                }
                std::process::exit(2);
            }
        };
        match run_resume_command(&resolved_path, &session, &command) {
            Ok(ResumeCommandOutcome {
                session: next_session,
                message,
                json,
            }) => {
                session = next_session;
                if output_format == CliOutputFormat::Json {
                    if let Some(value) = json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&value)
                                .expect("resume command json output")
                        );
                    } else if let Some(message) = message {
                        println!("{message}");
                    }
                } else if let Some(message) = message {
                    println!("{message}");
                }
            }
            Err(error) => {
                if output_format == CliOutputFormat::Json {
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "type": "error",
                            "error": error.to_string(),
                            "command": raw_command,
                        })
                    );
                } else {
                    eprintln!("{error}");
                }
                std::process::exit(2);
            }
        }
    }
}

#[derive(Debug, Clone)]
struct ResumeCommandOutcome {
    session: Session,
    message: Option<String>,
    json: Option<serde_json::Value>,
}

#[derive(Debug, Clone)]
struct StatusContext {
    cwd: PathBuf,
    session_path: Option<PathBuf>,
    loaded_config_files: usize,
    discovered_config_files: usize,
    memory_file_count: usize,
    project_root: Option<PathBuf>,
    git_branch: Option<String>,
    git_summary: GitWorkspaceSummary,
    sandbox_status: runtime::SandboxStatus,
}

#[derive(Debug, Clone, Copy)]
struct StatusUsage {
    message_count: usize,
    turns: u32,
    latest: TokenUsage,
    cumulative: TokenUsage,
    estimated_tokens: usize,
}

#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct GitWorkspaceSummary {
    changed_files: usize,
    staged_files: usize,
    unstaged_files: usize,
    untracked_files: usize,
    conflicted_files: usize,
}

impl GitWorkspaceSummary {
    fn is_clean(self) -> bool {
        self.changed_files == 0
    }

    fn headline(self) -> String {
        if self.is_clean() {
            "clean".to_string()
        } else {
            let mut details = Vec::new();
            if self.staged_files > 0 {
                details.push(format!("{} staged", self.staged_files));
            }
            if self.unstaged_files > 0 {
                details.push(format!("{} unstaged", self.unstaged_files));
            }
            if self.untracked_files > 0 {
                details.push(format!("{} untracked", self.untracked_files));
            }
            if self.conflicted_files > 0 {
                details.push(format!("{} conflicted", self.conflicted_files));
            }
            format!(
                "dirty · {} files · {}",
                self.changed_files,
                details.join(", ")
            )
        }
    }
}

#[cfg(test)]
fn format_unknown_slash_command_message(name: &str) -> String {
    let suggestions = suggest_slash_commands(name);
    let mut message = format!("unknown slash command: /{name}.");
    if !suggestions.is_empty() {
        message.push_str(" Did you mean ");
        message.push_str(&suggestions.join(", "));
        message.push('?');
    }
    if let Some(note) = omc_compatibility_note_for_unknown_slash_command(name) {
        message.push(' ');
        message.push_str(note);
    }
    message.push_str(" Use /help to list available commands.");
    message
}

fn format_model_report(model: &str, message_count: usize, turns: u32) -> String {
    let mut report = format!(
        "Model
  Current model    {model}
  Session messages {message_count}
  Session turns    {turns}",
    );
    // Saved profiles from provider.json (when present in the cwd).
    if let Ok(cwd) = env::current_dir() {
        let profiles = provider_config::list_profiles(&cwd);
        if !profiles.is_empty() {
            report.push_str("\n\n  Saved profiles (from provider.json):");
            for (name, profile) in &profiles {
                if let Some(ref url) = profile.base_url {
                    report.push_str(&format!(
                        "\n    /model use {name:<14} → {} ({url})",
                        profile.model
                    ));
                } else {
                    report.push_str(&format!("\n    /model use {name:<14} → {}", profile.model));
                }
            }
        }
    }
    report.push_str("\n\n  Built-in aliases:");
    for (alias, resolved) in BUILTIN_ALIASES {
        report.push_str(&format!("\n    /model {alias:<16} → {resolved}"));
    }
    report.push_str(
        "\n\n  Add custom aliases in settings.json: {\"aliases\": {\"my-shortcut\": \"full-model-id\"}}
  They override the built-in aliases above.\n\nUsage\n  Inspect current model with /model\n  Switch models with /model <name>\n  Re-run wizard with /model wizard",
    );
    report
}

/// Built-in model aliases displayed in the report. User aliases in settings.json
/// override these (see resolve_model_alias_with_config).
const BUILTIN_ALIASES: &[(&str, &str)] = &[
    ("opus", "Himalaya-opus-4-6"),
    ("sonnet", "Himalaya-sonnet-4-6"),
    ("haiku", "Himalaya-haiku-4-5-20251213"),
];

fn format_model_switch_report(previous: &str, next: &str, message_count: usize) -> String {
    format!(
        "Model updated
  Previous         {previous}
  Current          {next}
  Preserved msgs   {message_count}"
    )
}

fn public_permission_labels_for_sentence() -> String {
    let labels = PermissionMode::public_labels();
    match labels.as_slice() {
        [] => String::new(),
        [only] => (*only).to_string(),
        [head @ .., last] => format!("{}, or {last}", head.join(", ")),
    }
}

fn format_permissions_report(mode: &str) -> String {
    let modes = PermissionMode::public_modes()
        .iter()
        .map(|candidate| {
            let name = candidate.as_str();
            let description = match candidate {
                PermissionMode::Prompt => "Claude default: read/search tools run automatically; writes, shell, MCP, and elevated actions ask first",
                PermissionMode::ReadOnly => "Plan mode: read/search tools only",
                PermissionMode::WorkspaceWrite => "Accept edits / auto: edit files inside the workspace",
                PermissionMode::DangerFullAccess => "Bypass permissions: unrestricted tool access",
                PermissionMode::Allow => {
                    unreachable!("public modes exclude internal aliases")
                }
            };
            let marker = if name == mode {
                "● current"
            } else {
                "○ available"
            };
            format!("  {name:<18} {marker:<11} {description}")
        })
        .collect::<Vec<_>>()
        .join(
            "
",
        );

    format!(
        "Permissions
  Active mode      {mode}
  Mode status      live session default

Modes
{modes}

Usage
  Inspect current mode with /permissions
  Switch modes with /permissions <mode>"
    )
}

fn format_permissions_switch_report(previous: &str, next: &str) -> String {
    format!(
        "Permissions updated
  Result           mode switched
  Previous mode    {previous}
  Active mode      {next}
  Applies to       subsequent tool calls
  Usage            /permissions to inspect current mode"
    )
}

fn format_cost_report(usage: TokenUsage) -> String {
    format!(
        "Cost
  Input tokens     {}
  Output tokens    {}
  Cache create     {}
  Cache read       {}
  Total tokens     {}",
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_creation_input_tokens,
        usage.cache_read_input_tokens,
        usage.total_tokens(),
    )
}

fn format_resume_report(session_path: &str, message_count: usize, turns: u32) -> String {
    format!(
        "Session resumed
  Session file     {session_path}
  Messages         {message_count}
  Turns            {turns}"
    )
}

fn render_resume_usage() -> String {
    format!(
        "Resume
  Usage            /resume <session-path|session-id|{LATEST_SESSION_REFERENCE}>
  Auto-save        .Himalaya/sessions/<session-id>.{PRIMARY_SESSION_EXTENSION}
  Tip              use /session list to inspect saved sessions"
    )
}

fn format_compact_report(removed: usize, resulting_messages: usize, skipped: bool) -> String {
    if skipped {
        format!(
            "Compact
  Result           skipped
  Reason           session below compaction threshold
  Messages kept    {resulting_messages}"
        )
    } else {
        format!(
            "Compact
  Result           compacted
  Messages removed {removed}
  Messages kept    {resulting_messages}"
        )
    }
}

fn format_auto_compaction_notice(removed: usize) -> String {
    format!("[auto-compacted: removed {removed} messages]")
}

fn parse_git_status_metadata(status: Option<&str>) -> (Option<PathBuf>, Option<String>) {
    parse_git_status_metadata_for(
        &env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        status,
    )
}

fn parse_git_status_branch(status: Option<&str>) -> Option<String> {
    let status = status?;
    let first_line = status.lines().next()?;
    let line = first_line.strip_prefix("## ")?;
    if line.starts_with("HEAD") {
        return Some("detached HEAD".to_string());
    }
    let branch = line.split(['.', ' ']).next().unwrap_or_default().trim();
    if branch.is_empty() {
        None
    } else {
        Some(branch.to_string())
    }
}

fn parse_git_workspace_summary(status: Option<&str>) -> GitWorkspaceSummary {
    let mut summary = GitWorkspaceSummary::default();
    let Some(status) = status else {
        return summary;
    };

    for line in status.lines() {
        if line.starts_with("## ") || line.trim().is_empty() {
            continue;
        }

        summary.changed_files += 1;
        let mut chars = line.chars();
        let index_status = chars.next().unwrap_or(' ');
        let worktree_status = chars.next().unwrap_or(' ');

        if index_status == '?' && worktree_status == '?' {
            summary.untracked_files += 1;
            continue;
        }

        if index_status != ' ' {
            summary.staged_files += 1;
        }
        if worktree_status != ' ' {
            summary.unstaged_files += 1;
        }
        if (matches!(index_status, 'U' | 'A') && matches!(worktree_status, 'U' | 'A'))
            || index_status == 'U'
            || worktree_status == 'U'
        {
            summary.conflicted_files += 1;
        }
    }

    summary
}

fn resolve_git_branch_for(cwd: &Path) -> Option<String> {
    let branch = run_git_capture_in(cwd, &["branch", "--show-current"])?;
    let branch = branch.trim();
    if !branch.is_empty() {
        return Some(branch.to_string());
    }

    let fallback = run_git_capture_in(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let fallback = fallback.trim();
    if fallback.is_empty() {
        None
    } else if fallback == "HEAD" {
        Some("detached HEAD".to_string())
    } else {
        Some(fallback.to_string())
    }
}

fn run_git_capture_in(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn find_git_root_in(cwd: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()?;
    if !output.status.success() {
        return Err("not a git repository".into());
    }
    let path = String::from_utf8(output.stdout)?.trim().to_string();
    if path.is_empty() {
        return Err("empty git root".into());
    }
    Ok(PathBuf::from(path))
}

fn parse_git_status_metadata_for(
    cwd: &Path,
    status: Option<&str>,
) -> (Option<PathBuf>, Option<String>) {
    let branch = resolve_git_branch_for(cwd).or_else(|| parse_git_status_branch(status));
    let project_root = find_git_root_in(cwd).ok();
    (project_root, branch)
}

#[allow(clippy::too_many_lines)]
fn run_resume_command(
    session_path: &Path,
    session: &Session,
    command: &SlashCommand,
) -> Result<ResumeCommandOutcome, Box<dyn std::error::Error>> {
    match command {
        SlashCommand::Help => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(render_repl_help()),
            json: Some(serde_json::json!({ "kind": "help", "text": render_repl_help() })),
        }),
        SlashCommand::Compact => {
            let result = runtime::compact_session(
                session,
                CompactionConfig {
                    max_estimated_tokens: 0,
                    ..CompactionConfig::default()
                },
            );
            let removed = result.removed_message_count;
            let kept = result.compacted_session.messages.len();
            let skipped = removed == 0;
            result.compacted_session.save_to_path(session_path)?;
            Ok(ResumeCommandOutcome {
                session: result.compacted_session,
                message: Some(format_compact_report(removed, kept, skipped)),
                json: Some(serde_json::json!({
                    "kind": "compact",
                    "skipped": skipped,
                    "removed_messages": removed,
                    "kept_messages": kept,
                })),
            })
        }
        SlashCommand::Clear { confirm } => {
            if !confirm {
                return Ok(ResumeCommandOutcome {
                    session: session.clone(),
                    message: Some(
                        "clear: confirmation required; rerun with /clear --confirm".to_string(),
                    ),
                    json: Some(serde_json::json!({
                        "kind": "error",
                        "error": "confirmation required",
                        "hint": "rerun with /clear --confirm",
                    })),
                });
            }
            let backup_path = write_session_clear_backup(session, session_path)?;
            let previous_session_id = session.session_id.clone();
            let cleared = Session::new();
            let new_session_id = cleared.session_id.clone();
            cleared.save_to_path(session_path)?;
            Ok(ResumeCommandOutcome {
                session: cleared,
                message: Some(format!(
                    "Session cleared\n  Mode             resumed session reset\n  Previous session {previous_session_id}\n  Backup           {}\n  Resume previous  Himalaya --resume {}\n  New session      {new_session_id}\n  Session file     {}",
                    backup_path.display(),
                    backup_path.display(),
                    session_path.display()
                )),
                json: Some(serde_json::json!({
                    "kind": "clear",
                    "previous_session_id": previous_session_id,
                    "new_session_id": new_session_id,
                    "backup": backup_path.display().to_string(),
                    "session_file": session_path.display().to_string(),
                })),
            })
        }
        SlashCommand::Status => {
            let tracker = UsageTracker::from_session(session);
            let usage = tracker.cumulative_usage();
            let context = status_context(Some(session_path))?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format_status_report(
                    session.model.as_deref().unwrap_or("restored-session"),
                    StatusUsage {
                        message_count: session.messages.len(),
                        turns: tracker.turns(),
                        latest: tracker.current_turn_usage(),
                        cumulative: usage,
                        estimated_tokens: 0,
                    },
                    default_permission_mode().as_str(),
                    &context,
                )),
                json: Some(status_json_value(
                    session.model.as_deref(),
                    StatusUsage {
                        message_count: session.messages.len(),
                        turns: tracker.turns(),
                        latest: tracker.current_turn_usage(),
                        cumulative: usage,
                        estimated_tokens: 0,
                    },
                    default_permission_mode().as_str(),
                    &context,
                )),
            })
        }
        SlashCommand::Sandbox => {
            let cwd = env::current_dir()?;
            let loader = ConfigLoader::default_for(&cwd);
            let runtime_config = loader.load()?;
            let status = resolve_sandbox_status(runtime_config.sandbox(), &cwd);
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format_sandbox_report(&status)),
                json: Some(sandbox_json_value(&status)),
            })
        }
        SlashCommand::Cost => {
            let usage = UsageTracker::from_session(session).cumulative_usage();
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format_cost_report(usage)),
                json: Some(serde_json::json!({
                    "kind": "cost",
                    "input_tokens": usage.input_tokens,
                    "output_tokens": usage.output_tokens,
                    "cache_creation_input_tokens": usage.cache_creation_input_tokens,
                    "cache_read_input_tokens": usage.cache_read_input_tokens,
                    "total_tokens": usage.total_tokens(),
                })),
            })
        }
        SlashCommand::Config { section } => {
            let message = render_config_report(section.as_deref())?;
            let json = render_config_json(section.as_deref())?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(message),
                json: Some(json),
            })
        }
        SlashCommand::Mcp { action, target } => {
            let cwd = env::current_dir()?;
            let args = match (action.as_deref(), target.as_deref()) {
                (None, None) => None,
                (Some(action), None) => Some(action.to_string()),
                (Some(action), Some(target)) => Some(format!("{action} {target}")),
                (None, Some(target)) => Some(target.to_string()),
            };
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(handle_mcp_slash_command(args.as_deref(), &cwd)?),
                json: Some(handle_mcp_slash_command_json(args.as_deref(), &cwd)?),
            })
        }
        SlashCommand::Memory => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(render_memory_report()?),
            json: Some(render_memory_json()?),
        }),
        SlashCommand::Init => {
            let message = init_Himalaya_md()?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(message.clone()),
                json: Some(init_json_value(&message)),
            })
        }
        SlashCommand::Diff => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let message = render_diff_report_for(&cwd)?;
            let json = render_diff_json_for(&cwd)?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(message),
                json: Some(json),
            })
        }
        SlashCommand::Version => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(render_version_report()),
            json: Some(version_json_value()),
        }),
        SlashCommand::Export { path } => {
            let export_path = resolve_export_path(path.as_deref(), session)?;
            fs::write(&export_path, render_export_text(session))?;
            let msg_count = session.messages.len();
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format!(
                    "Export\n  Result           wrote transcript\n  File             {}\n  Messages         {}",
                    export_path.display(),
                    msg_count,
                )),
                json: Some(serde_json::json!({
                    "kind": "export",
                    "file": export_path.display().to_string(),
                    "message_count": msg_count,
                })),
            })
        }
        SlashCommand::Agents { args } => {
            let cwd = env::current_dir()?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(handle_agents_slash_command(args.as_deref(), &cwd)?),
                json: Some(serde_json::json!({
                    "kind": "agents",
                    "text": handle_agents_slash_command(args.as_deref(), &cwd)?,
                })),
            })
        }
        SlashCommand::Skills { args } => {
            if let SkillSlashDispatch::Invoke(_) = classify_skills_slash_command(args.as_deref()) {
                return Err(
                    "resumed /skills invocations are interactive-only; start `Himalaya` and run `/skills <skill>` in the REPL".into(),
                );
            }
            let cwd = env::current_dir()?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(handle_skills_slash_command(args.as_deref(), &cwd)?),
                json: Some(handle_skills_slash_command_json(args.as_deref(), &cwd)?),
            })
        }
        SlashCommand::Doctor => {
            let report = render_doctor_report()?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(report.render()),
                json: Some(report.json_value()),
            })
        }
        SlashCommand::Stats => {
            let usage = UsageTracker::from_session(session).cumulative_usage();
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format_cost_report(usage)),
                json: Some(serde_json::json!({
                    "kind": "stats",
                    "input_tokens": usage.input_tokens,
                    "output_tokens": usage.output_tokens,
                    "cache_creation_input_tokens": usage.cache_creation_input_tokens,
                    "cache_read_input_tokens": usage.cache_read_input_tokens,
                    "total_tokens": usage.total_tokens(),
                })),
            })
        }
        SlashCommand::History { count } => {
            let limit = parse_history_count(count.as_deref())
                .map_err(|error| -> Box<dyn std::error::Error> { error.into() })?;
            let entries = collect_session_prompt_history(session);
            let shown: Vec<_> = entries.iter().rev().take(limit).rev().collect();
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(render_prompt_history_report(&entries, limit)),
                json: Some(serde_json::json!({
                    "kind": "history",
                    "total": entries.len(),
                    "showing": shown.len(),
                    "entries": shown.iter().map(|e| serde_json::json!({
                        "timestamp_ms": e.timestamp_ms,
                        "text": e.text,
                    })).collect::<Vec<_>>(),
                })),
            })
        }
        SlashCommand::LocalCommand { name, args } => {
            let value =
                local_command_value(parse_local_cli_command_from_parts(name, args.as_deref())?)?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(render_local_command_text(&value)),
                json: Some(value),
            })
        }
        SlashCommand::Review { scope } => {
            let value = local_command_value(LocalCliCommand::Review {
                scope: scope.clone(),
            })?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(render_local_command_text(&value)),
                json: Some(value),
            })
        }
        SlashCommand::Workspace { path } => {
            let value = local_command_value(LocalCliCommand::Workspace {
                path: path.as_ref().map(PathBuf::from),
            })?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(render_local_command_text(&value)),
                json: Some(value),
            })
        }
        SlashCommand::Diagnostics { path } => {
            let value = local_command_value(LocalCliCommand::Diagnostics { path: path.clone() })?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(render_local_command_text(&value)),
                json: Some(value),
            })
        }
        SlashCommand::Plan { mode } => {
            let prompt = mode.clone().unwrap_or_default();
            if prompt.trim().is_empty() {
                return Err("resumed /plan requires a task description".into());
            }
            let value = build_plan_output(&prompt, PermissionMode::ReadOnly)?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(render_plan_text(&value)),
                json: Some(value),
            })
        }
        SlashCommand::Tasks { args } => {
            let task_command = parse_task_cli_command(&split_slash_remainder(args.as_deref()))?;
            match task_command {
                TaskCliCommand::List { status } => {
                    let registry = load_task_registry()?;
                    Ok(ResumeCommandOutcome {
                        session: session.clone(),
                        message: Some(render_task_list(status)?),
                        json: Some(serde_json::json!({
                            "type": "task_list",
                            "tasks": registry.list(status),
                        })),
                    })
                }
                TaskCliCommand::Show { task_id } => {
                    let registry = load_task_registry()?;
                    let task = registry
                        .get(&task_id)
                        .ok_or_else(|| format!("task not found: {task_id}"))?;
                    Ok(ResumeCommandOutcome {
                        session: session.clone(),
                        message: Some(render_task_show(&task_id)?),
                        json: Some(serde_json::json!({
                            "type": "task_show",
                            "task": task,
                            "ledger": registry.ledger_for_task(&task_id),
                            "event_log": registry.event_log_for_task(&task_id),
                        })),
                    })
                }
                TaskCliCommand::Status { task_id } => {
                    let registry = load_task_registry()?;
                    let task = registry
                        .get(&task_id)
                        .ok_or_else(|| format!("task not found: {task_id}"))?;
                    let value = task_status_value(&registry, task, "task_status");
                    Ok(ResumeCommandOutcome {
                        session: session.clone(),
                        message: Some(render_task_status_text(&value)),
                        json: Some(value),
                    })
                }
                TaskCliCommand::Report { task_id } => {
                    let registry = load_task_registry()?;
                    let task = registry
                        .get(&task_id)
                        .ok_or_else(|| format!("task not found: {task_id}"))?;
                    let value = task_report_value(&registry, task)?;
                    Ok(ResumeCommandOutcome {
                        session: session.clone(),
                        message: Some(render_task_report_text(&value)),
                        json: Some(value),
                    })
                }
                TaskCliCommand::Review { task_id } => {
                    let registry = load_task_registry()?;
                    let task = registry
                        .get(&task_id)
                        .ok_or_else(|| format!("task not found: {task_id}"))?;
                    let value = task_review_value(&registry, task)?;
                    Ok(ResumeCommandOutcome {
                        session: session.clone(),
                        message: Some(render_task_review_text(&value)),
                        json: Some(value),
                    })
                }
                TaskCliCommand::Packet {
                    command: TaskPacketCliCommand::Status { task_id },
                } => {
                    let registry = load_task_registry()?;
                    let task = registry
                        .get(&task_id)
                        .ok_or_else(|| format!("task not found: {task_id}"))?;
                    if task.task_packet.is_none() {
                        return Err(format!("task {task_id} was not created from a task packet").into());
                    }
                    Ok(ResumeCommandOutcome {
                        session: session.clone(),
                        message: Some(render_task_show(&task_id)?),
                        json: Some(task_packet_status_value(
                            &registry,
                            task,
                            "task_packet_status",
                        )),
                    })
                }
                TaskCliCommand::Scheduler {
                    command: TaskSchedulerCliCommand::Queue,
                } => {
                    let registry = load_task_registry()?;
                    let scheduler = runtime::DurableTaskScheduler::new(
                        registry,
                        runtime::VerificationRunner::new(Some(env::current_dir()?)),
                    );
                    Ok(ResumeCommandOutcome {
                        session: session.clone(),
                        message: Some("scheduler queue loaded".to_string()),
                        json: Some(serde_json::json!({
                            "type": "task_scheduler_queue",
                            "queue": scheduler.queue(),
                        })),
                    })
                }
                TaskCliCommand::Scheduler {
                    command: TaskSchedulerCliCommand::Tick,
                } => Err(
                    "resumed /tasks supports scheduler queue; use `Himalaya tasks scheduler tick` to run scheduler ticks"
                        .into(),
                ),
                _ => Err(
                    "resumed /tasks supports list, show, status, report, review, packet status, and scheduler queue; use `Himalaya tasks ...` for execution, recovery, verification, retry, compact, cancel, packet create/run, scheduler tick, or resume actions"
                        .into(),
                ),
            }
        }
        SlashCommand::Cron { args } => {
            let cron_command = parse_cron_cli_command(&split_slash_remainder(args.as_deref()))?;
            match cron_command {
                CronCliCommand::List => {
                    let registry = load_cron_registry()?;
                    Ok(ResumeCommandOutcome {
                        session: session.clone(),
                        message: Some(render_cron_list()?),
                        json: Some(serde_json::json!({
                            "type": "cron_list",
                            "crons": registry.list(false),
                        })),
                    })
                }
                _ => Err(
                    "resumed /cron supports list; use `Himalaya cron ...` for add or remove actions"
                        .into(),
                ),
            }
        }
        SlashCommand::Benchmark { args } => {
            let command = parse_benchmark_cli_command(&split_slash_remainder(args.as_deref()))?;
            let value = benchmark_command_value(
                command,
                &session
                    .model
                    .clone()
                    .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            )?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(render_benchmark_value_text(&value)),
                json: Some(value),
            })
        }
        SlashCommand::Unknown(name) => Err(format_unknown_slash_command(name).into()),
        // /session list can be served from the sessions directory without a live session.
        SlashCommand::Session {
            action: Some(ref act),
            ..
        } if act == "list" => {
            let sessions = list_managed_sessions().unwrap_or_default();
            let session_ids: Vec<String> = sessions.iter().map(|s| s.id.clone()).collect();
            let active_id = session.session_id.clone();
            let text = render_session_list(&active_id).unwrap_or_else(|e| format!("error: {e}"));
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(text),
                json: Some(serde_json::json!({
                    "kind": "session_list",
                    "sessions": session_ids,
                    "active": active_id,
                })),
            })
        }
        SlashCommand::Bughunter { .. }
        | SlashCommand::Commit { .. }
        | SlashCommand::Pr { .. }
        | SlashCommand::Issue { .. }
        | SlashCommand::Ultraplan { .. }
        | SlashCommand::Teleport { .. }
        | SlashCommand::DebugToolCall { .. }
        | SlashCommand::Resume { .. }
        | SlashCommand::Model { .. }
        | SlashCommand::Permissions { .. }
        | SlashCommand::Session { .. }
        | SlashCommand::Plugins { .. }
        | SlashCommand::Login
        | SlashCommand::Logout
        | SlashCommand::Vim
        | SlashCommand::Upgrade
        | SlashCommand::Share
        | SlashCommand::Feedback
        | SlashCommand::Files
        | SlashCommand::Fast
        | SlashCommand::Exit
        | SlashCommand::Summary
        | SlashCommand::Desktop
        | SlashCommand::Brief
        | SlashCommand::Advisor
        | SlashCommand::Stickers
        | SlashCommand::Insights
        | SlashCommand::Thinkback
        | SlashCommand::ReleaseNotes
        | SlashCommand::SecurityReview
        | SlashCommand::Keybindings
        | SlashCommand::PrivacySettings
        | SlashCommand::Theme { .. }
        | SlashCommand::Voice { .. }
        | SlashCommand::Usage { .. }
        | SlashCommand::Rename { .. }
        | SlashCommand::Copy { .. }
        | SlashCommand::Hooks { .. }
        | SlashCommand::Context { .. }
        | SlashCommand::Color { .. }
        | SlashCommand::Effort { .. }
        | SlashCommand::Branch { .. }
        | SlashCommand::Rewind { .. }
        | SlashCommand::Ide { .. }
        | SlashCommand::Tag { .. }
        | SlashCommand::OutputStyle { .. }
        | SlashCommand::AddDir { .. } => Err("unsupported resumed slash command".into()),
    }
}

/// Detect if the current working directory is "broad" (home directory or
/// filesystem root). Returns the cwd path if broad, None otherwise.
fn detect_broad_cwd() -> Option<PathBuf> {
    None
}

/// Enforce the broad-CWD policy: when running from home or root, either
/// require the --allow-broad-cwd flag, or prompt for confirmation (interactive),
/// or exit with an error (non-interactive).
fn enforce_broad_cwd_policy(
    allow_broad_cwd: bool,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    if allow_broad_cwd {
        return Ok(());
    }
    let Some(cwd) = detect_broad_cwd() else {
        return Ok(());
    };

    let is_interactive = io::stdin().is_terminal();

    if is_interactive {
        // Interactive mode: print warning and ask for confirmation
        eprintln!(
            "Warning: Himalaya is running from a very broad directory ({}).\n\
             The agent can read and search everything under this path.\n\
             Consider running from inside your project: cd /path/to/project && Himalaya",
            cwd.display()
        );
        eprint!("Continue anyway? [y/N]: ");
        io::stderr().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let trimmed = input.trim().to_lowercase();
        if trimmed != "y" && trimmed != "yes" {
            eprintln!("Aborted.");
            std::process::exit(0);
        }
        Ok(())
    } else {
        // Non-interactive mode: exit with error (JSON or text)
        let message = format!(
            "Himalaya is running from a very broad directory ({}). \
             The agent can read and search everything under this path. \
             Use --allow-broad-cwd to proceed anyway, \
             or run from inside your project: cd /path/to/project && Himalaya",
            cwd.display()
        );
        match output_format {
            CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "type": "error",
                        "error": message,
                    })
                );
            }
            CliOutputFormat::Text => {
                eprintln!("error: {message}");
            }
        }
        std::process::exit(1);
    }
}

fn run_stale_base_preflight(flag_value: Option<&str>) {
    let cwd = match env::current_dir() {
        Ok(cwd) => cwd,
        Err(_) => return,
    };
    let source = resolve_expected_base(flag_value, &cwd);
    let state = check_base_commit(&cwd, source.as_ref());
    if let Some(warning) = format_stale_base_warning(&state) {
        eprintln!("{warning}");
    }
}

/// Machine-friendly NDJSON REPL mode for persistent VS Code extension sessions.
///
/// Reads NDJSON commands from stdin (one JSON object per line), processes each
/// prompt through `run_turn`, and streams responses back as stream-json events.
/// The session stays in memory between prompts, avoiding per-request startup
/// overhead (config loading, system prompt discovery, session file I/O).
///
/// Protocol:
///   stdin:  {"type":"prompt","text":"...","files":["/path/to/file.pdf"]}
///           {"type":"exit"}
///   stdout: stream-json events (text_delta, tool_use, tool_result, done, ...)
fn run_repl_ndjson(
    model: String,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    allow_broad_cwd: bool,
    resume_target: Option<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    enforce_broad_cwd_policy(allow_broad_cwd, CliOutputFormat::StreamJson)?;
    let mut cli = match resume_target {
        Some(target) => match LiveCli::from_existing_session(
            target,
            model.clone(),
            true,
            allowed_tools.clone(),
            permission_mode,
        ) {
            Ok(cli) => cli,
            // No prior session to resume (e.g. fresh workspace) — start clean
            // instead of failing the whole REPL.
            Err(_) => LiveCli::new(model.clone(), true, allowed_tools, permission_mode)?,
        },
        None => LiveCli::new(model.clone(), true, allowed_tools, permission_mode)?,
    };

    // Signal readiness so the extension knows we're ready for prompts.
    eprintln!("[repl] ready");

    let mut line = String::new();
    loop {
        line.clear();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) => {
                // EOF — clean shutdown
                let _ = cli.persist_session();
                break;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("[repl] stdin read error: {e}");
                break;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let cmd: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[repl] invalid JSON: {e}");
                continue;
            }
        };
        match cmd.get("type").and_then(serde_json::Value::as_str) {
            Some("prompt") => {
                let text = cmd
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if text.is_empty() {
                    continue;
                }
                let file_paths = cmd
                    .get("files")
                    .and_then(serde_json::Value::as_array)
                    .map(|files| {
                        files
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .filter(|path| !path.trim().is_empty())
                            .map(PathBuf::from)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if !file_paths.is_empty() {
                    match load_files_as_content_blocks(&file_paths, &cli.model) {
                        Ok(blocks) => {
                            if let Err(error) = cli.inject_file_blocks(blocks) {
                                print_stream_json_event(
                                    json!({"type":"error","error":error.to_string()}),
                                );
                                print_stream_json_event(json!({"type":"done","iterations":0}));
                                continue;
                            }
                        }
                        Err(error) => {
                            print_stream_json_event(json!({"type":"error","error":error}));
                            print_stream_json_event(json!({"type":"done","iterations":0}));
                            continue;
                        }
                    }
                }
                match cli.run_prompt_stream_json(text) {
                    Ok(()) => {}
                    Err(e) => {
                        print_stream_json_event(json!({"type":"error","error":e.to_string()}));
                        print_stream_json_event(json!({"type":"done","iterations":0}));
                    }
                }
            }
            Some("exit") => {
                let _ = cli.persist_session();
                break;
            }
            _ => {
                eprintln!(
                    "[repl] unknown command type: {}",
                    trimmed.chars().take(80).collect::<String>()
                );
            }
        }
    }
    Ok(())
}

fn run_repl(
    model: String,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    base_commit: Option<String>,
    reasoning_effort: Option<String>,
    allow_broad_cwd: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    enforce_broad_cwd_policy(allow_broad_cwd, CliOutputFormat::Text)?;
    run_stale_base_preflight(base_commit.as_deref());
    let resolved_model = resolve_repl_model(model);
    let mut cli = LiveCli::new(resolved_model, true, allowed_tools, permission_mode)?;
    cli.set_reasoning_effort(reasoning_effort);
    let mut editor =
        input::LineEditor::new("> ", cli.repl_completion_candidates().unwrap_or_default());
    println!("{}", cli.startup_banner());
    println!("{}", format_connected_line(&cli.model));

    loop {
        editor.set_completions(cli.repl_completion_candidates().unwrap_or_default());
        match editor.read_line()? {
            input::ReadOutcome::Submit(input) => {
                let trimmed = input.trim().to_string();
                if trimmed.is_empty() {
                    continue;
                }
                if matches!(trimmed.as_str(), "/exit" | "/quit") {
                    cli.persist_session()?;
                    break;
                }
                match SlashCommand::parse(&trimmed) {
                    Ok(Some(command)) => {
                        if cli.handle_repl_command(command)? {
                            cli.persist_session()?;
                        }
                        continue;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        eprintln!("{error}");
                        continue;
                    }
                }
                // Bare-word skill dispatch: if the first token of the input
                // matches a known skill name, invoke it as `/skills <input>`
                // rather than forwarding raw text to the LLM (ROADMAP #36).
                let bare_first_token = trimmed.split_whitespace().next().unwrap_or_default();
                let looks_like_skill_name = !bare_first_token.is_empty()
                    && !bare_first_token.starts_with('/')
                    && bare_first_token
                        .chars()
                        .all(|c| c.is_alphanumeric() || c == '-' || c == '_');
                if looks_like_skill_name {
                    let cwd = std::env::current_dir().unwrap_or_default();
                    if let Ok(SkillSlashDispatch::Invoke(prompt)) =
                        resolve_skill_invocation(&cwd, Some(&trimmed))
                    {
                        editor.push_history(input);
                        cli.record_prompt_history(&trimmed);
                        cli.run_turn(&prompt)?;
                        continue;
                    }
                }
                editor.push_history(input);
                cli.record_prompt_history(&trimmed);
                // Expand file://, file:, 附件:, 文件: prefix lines to @path before
                // the main @file expansion pass (unified attachment syntax).
                let expanded = expand_file_prefix_lines(&trimmed);
                // Expand @file tokens before sending to the LLM.
                match expand_at_file_syntax(&expanded, &cli.model) {
                    Ok((processed, file_blocks)) => {
                        let summary = attachment_summary(&processed, &file_blocks);
                        if !summary.is_empty() {
                            eprintln!("{summary}");
                        }
                        if !file_blocks.is_empty() {
                            if let Err(e) = cli.inject_file_blocks(file_blocks) {
                                eprintln!("file injection error: {e}");
                                continue;
                            }
                        }
                        cli.run_turn(&processed)?;
                    }
                    Err(e) => {
                        eprintln!("{e}");
                        continue;
                    }
                }
            }
            input::ReadOutcome::Cancel => {}
            input::ReadOutcome::Exit => {
                cli.persist_session()?;
                break;
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct SessionHandle {
    id: String,
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct ManagedSessionSummary {
    id: String,
    path: PathBuf,
    modified_epoch_millis: u128,
    message_count: usize,
    parent_session_id: Option<String>,
    branch_name: Option<String>,
}

struct LiveCli {
    model: String,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    system_prompt: Vec<String>,
    runtime: BuiltRuntime,
    session: SessionHandle,
    prompt_history: Vec<PromptHistoryEntry>,
    pending_file_blocks: Vec<ContentBlock>,
    resume_task_id: Option<String>,
}

#[derive(Debug, Clone)]
struct PromptHistoryEntry {
    timestamp_ms: u64,
    text: String,
}

struct RuntimePluginState {
    feature_config: runtime::RuntimeFeatureConfig,
    tool_registry: GlobalToolRegistry,
    plugin_registry: PluginRegistry,
    mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
}

struct RuntimeMcpState {
    runtime: tokio::runtime::Runtime,
    manager: McpServerManager,
    pending_servers: Vec<String>,
    degraded_report: Option<runtime::McpDegradedReport>,
}

struct BuiltRuntime {
    runtime: Option<ConversationRuntime<AnthropicRuntimeClient, CliToolExecutor>>,
    plugin_registry: PluginRegistry,
    plugins_active: bool,
    mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    mcp_active: bool,
}

impl BuiltRuntime {
    fn new(
        runtime: ConversationRuntime<AnthropicRuntimeClient, CliToolExecutor>,
        plugin_registry: PluginRegistry,
        mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    ) -> Self {
        Self {
            runtime: Some(runtime),
            plugin_registry,
            plugins_active: true,
            mcp_state,
            mcp_active: true,
        }
    }

    fn with_hook_abort_signal(mut self, hook_abort_signal: runtime::HookAbortSignal) -> Self {
        let runtime = self
            .runtime
            .take()
            .expect("runtime should exist before installing hook abort signal");
        self.runtime = Some(runtime.with_hook_abort_signal(hook_abort_signal));
        self
    }

    fn with_resume_task_id(mut self, task_id: impl Into<String>) -> Self {
        let runtime = self
            .runtime
            .take()
            .expect("runtime should exist before installing resume task id");
        self.runtime = Some(runtime.with_resume_task_id(task_id));
        self
    }

    fn shutdown_plugins(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.plugins_active {
            self.plugin_registry.shutdown()?;
            self.plugins_active = false;
        }
        Ok(())
    }

    fn shutdown_mcp(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.mcp_active {
            if let Some(mcp_state) = &self.mcp_state {
                mcp_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .shutdown()?;
            }
            self.mcp_active = false;
        }
        Ok(())
    }
}

impl Deref for BuiltRuntime {
    type Target = ConversationRuntime<AnthropicRuntimeClient, CliToolExecutor>;

    fn deref(&self) -> &Self::Target {
        self.runtime
            .as_ref()
            .expect("runtime should exist while built runtime is alive")
    }
}

impl DerefMut for BuiltRuntime {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.runtime
            .as_mut()
            .expect("runtime should exist while built runtime is alive")
    }
}

impl Drop for BuiltRuntime {
    fn drop(&mut self) {
        let _ = self.shutdown_mcp();
        let _ = self.shutdown_plugins();
    }
}

#[derive(Debug, Deserialize)]
struct ToolSearchRequest {
    query: String,
    max_results: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct McpToolRequest {
    #[serde(rename = "qualifiedName")]
    qualified_name: Option<String>,
    tool: Option<String>,
    arguments: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ListMcpResourcesRequest {
    server: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReadMcpResourceRequest {
    server: String,
    uri: String,
}

impl RuntimeMcpState {
    fn new(
        runtime_config: &runtime::RuntimeConfig,
    ) -> Result<Option<(Self, runtime::McpToolDiscoveryReport)>, Box<dyn std::error::Error>> {
        let mut manager = McpServerManager::from_runtime_config(runtime_config);
        if manager.server_names().is_empty() && manager.unsupported_servers().is_empty() {
            return Ok(None);
        }

        let runtime = tokio::runtime::Runtime::new()?;
        let discovery = runtime.block_on(manager.discover_tools_best_effort());
        let pending_servers = discovery
            .failed_servers
            .iter()
            .map(|failure| failure.server_name.clone())
            .chain(
                discovery
                    .unsupported_servers
                    .iter()
                    .map(|server| server.server_name.clone()),
            )
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let available_tools = discovery
            .tools
            .iter()
            .map(|tool| tool.qualified_name.clone())
            .collect::<Vec<_>>();
        let failed_server_names = pending_servers.iter().cloned().collect::<BTreeSet<_>>();
        let working_servers = manager
            .server_names()
            .into_iter()
            .filter(|server_name| !failed_server_names.contains(server_name))
            .collect::<Vec<_>>();
        let failed_servers =
            discovery
                .failed_servers
                .iter()
                .map(|failure| runtime::McpFailedServer {
                    server_name: failure.server_name.clone(),
                    phase: runtime::McpLifecyclePhase::ToolDiscovery,
                    error: runtime::McpErrorSurface::new(
                        runtime::McpLifecyclePhase::ToolDiscovery,
                        Some(failure.server_name.clone()),
                        failure.error.clone(),
                        std::collections::BTreeMap::new(),
                        true,
                    ),
                })
                .chain(discovery.unsupported_servers.iter().map(|server| {
                    runtime::McpFailedServer {
                        server_name: server.server_name.clone(),
                        phase: runtime::McpLifecyclePhase::ServerRegistration,
                        error: runtime::McpErrorSurface::new(
                            runtime::McpLifecyclePhase::ServerRegistration,
                            Some(server.server_name.clone()),
                            server.reason.clone(),
                            std::collections::BTreeMap::from([(
                                "transport".to_string(),
                                format!("{:?}", server.transport).to_ascii_lowercase(),
                            )]),
                            false,
                        ),
                    }
                }))
                .collect::<Vec<_>>();
        let degraded_report = (!failed_servers.is_empty()).then(|| {
            runtime::McpDegradedReport::new(
                working_servers,
                failed_servers,
                available_tools.clone(),
                available_tools,
            )
        });

        Ok(Some((
            Self {
                runtime,
                manager,
                pending_servers,
                degraded_report,
            },
            discovery,
        )))
    }

    fn shutdown(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime.block_on(self.manager.shutdown())?;
        Ok(())
    }

    fn pending_servers(&self) -> Option<Vec<String>> {
        (!self.pending_servers.is_empty()).then(|| self.pending_servers.clone())
    }

    fn degraded_report(&self) -> Option<runtime::McpDegradedReport> {
        self.degraded_report.clone()
    }

    fn server_names(&self) -> Vec<String> {
        self.manager.server_names()
    }

    fn call_tool(
        &mut self,
        qualified_tool_name: &str,
        arguments: Option<serde_json::Value>,
    ) -> Result<String, ToolError> {
        let response = self
            .runtime
            .block_on(self.manager.call_tool(qualified_tool_name, arguments))
            .map_err(|error| ToolError::new(error.to_string()))?;
        if let Some(error) = response.error {
            return Err(ToolError::new(format!(
                "MCP tool `{qualified_tool_name}` returned JSON-RPC error: {} ({})",
                error.message, error.code
            )));
        }

        let result = response.result.ok_or_else(|| {
            ToolError::new(format!(
                "MCP tool `{qualified_tool_name}` returned no result payload"
            ))
        })?;
        serde_json::to_string_pretty(&result).map_err(|error| ToolError::new(error.to_string()))
    }

    fn list_resources_for_server(&mut self, server_name: &str) -> Result<String, ToolError> {
        let result = self
            .runtime
            .block_on(self.manager.list_resources(server_name))
            .map_err(|error| ToolError::new(error.to_string()))?;
        serde_json::to_string_pretty(&json!({
            "server": server_name,
            "resources": result.resources,
        }))
        .map_err(|error| ToolError::new(error.to_string()))
    }

    fn list_resources_for_all_servers(&mut self) -> Result<String, ToolError> {
        let mut resources = Vec::new();
        let mut failures = Vec::new();

        for server_name in self.server_names() {
            match self
                .runtime
                .block_on(self.manager.list_resources(&server_name))
            {
                Ok(result) => resources.push(json!({
                    "server": server_name,
                    "resources": result.resources,
                })),
                Err(error) => failures.push(json!({
                    "server": server_name,
                    "error": error.to_string(),
                })),
            }
        }

        if resources.is_empty() && !failures.is_empty() {
            let message = failures
                .iter()
                .filter_map(|failure| failure.get("error").and_then(serde_json::Value::as_str))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ToolError::new(message));
        }

        serde_json::to_string_pretty(&json!({
            "resources": resources,
            "failures": failures,
        }))
        .map_err(|error| ToolError::new(error.to_string()))
    }

    fn read_resource(&mut self, server_name: &str, uri: &str) -> Result<String, ToolError> {
        let result = self
            .runtime
            .block_on(self.manager.read_resource(server_name, uri))
            .map_err(|error| ToolError::new(error.to_string()))?;
        serde_json::to_string_pretty(&json!({
            "server": server_name,
            "contents": result.contents,
        }))
        .map_err(|error| ToolError::new(error.to_string()))
    }
}

fn build_runtime_mcp_state(
    runtime_config: &runtime::RuntimeConfig,
) -> Result<RuntimePluginStateBuildOutput, Box<dyn std::error::Error>> {
    let Some((mcp_state, discovery)) = RuntimeMcpState::new(runtime_config)? else {
        return Ok((None, Vec::new()));
    };

    let mut runtime_tools = discovery
        .tools
        .iter()
        .map(mcp_runtime_tool_definition)
        .collect::<Vec<_>>();
    if !mcp_state.server_names().is_empty() {
        runtime_tools.extend(mcp_wrapper_tool_definitions());
    }

    Ok((Some(Arc::new(Mutex::new(mcp_state))), runtime_tools))
}

fn mcp_runtime_tool_definition(tool: &runtime::ManagedMcpTool) -> RuntimeToolDefinition {
    RuntimeToolDefinition {
        name: tool.qualified_name.clone(),
        description: Some(
            tool.tool
                .description
                .clone()
                .unwrap_or_else(|| format!("Invoke MCP tool `{}`.", tool.qualified_name)),
        ),
        input_schema: tool
            .tool
            .input_schema
            .clone()
            .unwrap_or_else(|| json!({ "type": "object", "additionalProperties": true })),
        required_permission: permission_mode_for_mcp_tool(&tool.tool),
    }
}

fn mcp_wrapper_tool_definitions() -> Vec<RuntimeToolDefinition> {
    vec![
        RuntimeToolDefinition {
            name: "MCPTool".to_string(),
            description: Some(
                "Call a configured MCP tool by its qualified name and JSON arguments.".to_string(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "qualifiedName": { "type": "string" },
                    "arguments": {}
                },
                "required": ["qualifiedName"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::DangerFullAccess,
        },
        RuntimeToolDefinition {
            name: "ListMcpResourcesTool".to_string(),
            description: Some(
                "List MCP resources from one configured server or from every connected server."
                    .to_string(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" }
                },
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
        RuntimeToolDefinition {
            name: "ReadMcpResourceTool".to_string(),
            description: Some("Read a specific MCP resource from a configured server.".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": { "type": "string" },
                    "uri": { "type": "string" }
                },
                "required": ["server", "uri"],
                "additionalProperties": false
            }),
            required_permission: PermissionMode::ReadOnly,
        },
    ]
}

fn permission_mode_for_mcp_tool(tool: &McpTool) -> PermissionMode {
    let read_only = mcp_annotation_flag(tool, "readOnlyHint");
    let destructive = mcp_annotation_flag(tool, "destructiveHint");
    let open_world = mcp_annotation_flag(tool, "openWorldHint");

    if read_only && !destructive && !open_world {
        PermissionMode::ReadOnly
    } else if destructive || open_world {
        PermissionMode::DangerFullAccess
    } else {
        PermissionMode::WorkspaceWrite
    }
}

fn mcp_annotation_flag(tool: &McpTool, key: &str) -> bool {
    tool.annotations
        .as_ref()
        .and_then(|annotations| annotations.get(key))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

struct HookAbortMonitor {
    stop_tx: Option<Sender<()>>,
    join_handle: Option<JoinHandle<()>>,
}

impl HookAbortMonitor {
    fn spawn(abort_signal: runtime::HookAbortSignal) -> Self {
        Self::spawn_with_waiter(abort_signal, move |stop_rx, abort_signal| {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };

            runtime.block_on(async move {
                let wait_for_stop = tokio::task::spawn_blocking(move || {
                    let _ = stop_rx.recv();
                });

                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        if result.is_ok() {
                            abort_signal.abort();
                        }
                    }
                    _ = wait_for_stop => {}
                }
            });
        })
    }

    fn spawn_with_waiter<F>(abort_signal: runtime::HookAbortSignal, wait_for_interrupt: F) -> Self
    where
        F: FnOnce(Receiver<()>, runtime::HookAbortSignal) + Send + 'static,
    {
        let (stop_tx, stop_rx) = mpsc::channel();
        let join_handle = thread::spawn(move || wait_for_interrupt(stop_rx, abort_signal));

        Self {
            stop_tx: Some(stop_tx),
            join_handle: Some(join_handle),
        }
    }

    fn stop(mut self) {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

impl LiveCli {
    fn new(
        model: String,
        enable_tools: bool,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let system_prompt = build_system_prompt()?;
        let workspace_root = env::current_dir()?;
        let session_state = Session::new().with_workspace_root(workspace_root);
        let session = create_managed_session_handle(&session_state.session_id)?;
        let runtime = build_runtime(
            session_state.with_persistence_path(session.path.clone()),
            &session.id,
            model.clone(),
            system_prompt.clone(),
            enable_tools,
            true,
            false,
            allowed_tools.clone(),
            permission_mode,
            None,
        )?;
        let cli = Self {
            model,
            allowed_tools,
            permission_mode,
            system_prompt,
            runtime,
            session,
            prompt_history: Vec::new(),
            pending_file_blocks: Vec::new(),
            resume_task_id: None,
        };
        cli.persist_session()?;
        Ok(cli)
    }

    fn from_existing_session(
        session_path: PathBuf,
        model: String,
        enable_tools: bool,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let resolved_path = if session_path.exists() {
            session_path
        } else {
            resolve_session_reference(&session_path.display().to_string())?.path
        };
        let system_prompt = build_system_prompt()?;
        let mut session_state = Session::load_from_path(&resolved_path)?;
        if session_state.workspace_root().is_none() {
            session_state = session_state.with_workspace_root(env::current_dir()?);
        }
        let session_id = session_state.session_id.clone();
        let runtime = build_runtime(
            session_state,
            &session_id,
            model.clone(),
            system_prompt.clone(),
            enable_tools,
            true,
            false,
            allowed_tools.clone(),
            permission_mode,
            None,
        )?;
        Ok(Self {
            model,
            allowed_tools,
            permission_mode,
            system_prompt,
            runtime,
            session: SessionHandle {
                id: session_id,
                path: resolved_path,
            },
            prompt_history: Vec::new(),
            pending_file_blocks: Vec::new(),
            resume_task_id: None,
        })
    }

    fn set_reasoning_effort(&mut self, effort: Option<String>) {
        if let Some(rt) = self.runtime.runtime.as_mut() {
            rt.api_client_mut().set_reasoning_effort(effort);
        }
    }

    fn resume_task_once(&mut self, task_id: impl Into<String>) {
        self.resume_task_id = Some(task_id.into());
    }

    /// Inject file content blocks into the session before the next turn.
    fn inject_file_blocks(
        &mut self,
        blocks: Vec<ContentBlock>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.pending_file_blocks.extend(blocks);
        Ok(())
    }

    fn startup_banner(&self) -> String {
        let cwd = env::current_dir().map_or_else(
            |_| "<unknown>".to_string(),
            |path| path.display().to_string(),
        );
        let status = status_context(None).ok();
        let git_branch = status
            .as_ref()
            .and_then(|context| context.git_branch.as_deref())
            .unwrap_or("unknown");
        let workspace = status.as_ref().map_or_else(
            || "unknown".to_string(),
            |context| context.git_summary.headline(),
        );
        let session_path = self.session.path.strip_prefix(Path::new(&cwd)).map_or_else(
            |_| self.session.path.display().to_string(),
            |path| path.display().to_string(),
        );
        format!(
            "\x1b[38;5;196m\
 ██╗  ██╗██╗███╗   ███╗ █████╗ ██╗      █████╗ ██╗   ██╗ █████╗ \n\
 ██║  ██║██║████╗ ████║██╔══██╗██║     ██╔══██╗╚██╗ ██╔╝██╔══██╗\n\
 ███████║██║██╔████╔██║███████║██║     ███████║ ╚████╔╝ ███████║\n\
 ██╔══██║██║██║╚██╔╝██║██╔══██║██║     ██╔══██║  ╚██╔╝  ██╔══██║\n\
 ██║  ██║██║██║ ╚═╝ ██║██║  ██║███████╗██║  ██║   ██║   ██║  ██║\n\
 ╚═╝  ╚═╝╚═╝╚═╝     ╚═╝╚═╝  ╚═╝╚══════╝╚═╝  ╚═╝   ╚═╝   ╚═╝  ╚═╝\x1b[0m \x1b[38;5;208mCode\x1b[0m ✨\n\n\
  \x1b[2mModel\x1b[0m            {}\n\
  \x1b[2mPermissions\x1b[0m      {}\n\
  \x1b[2mBranch\x1b[0m           {}\n\
  \x1b[2mWorkspace\x1b[0m        {}\n\
  \x1b[2mDirectory\x1b[0m        {}\n\
  \x1b[2mSession\x1b[0m          {}\n\
  \x1b[2mAuto-save\x1b[0m        {}\n\n\
  Type \x1b[1m/help\x1b[0m for commands · \x1b[1m/status\x1b[0m for live context · \x1b[2m/resume latest\x1b[0m jumps back to the newest session · \x1b[1m/diff\x1b[0m then \x1b[1m/commit\x1b[0m to ship · \x1b[2mTab\x1b[0m for workflow completions · \x1b[2mShift+Enter\x1b[0m for newline",
            self.model,
            self.permission_mode.as_str(),
            git_branch,
            workspace,
            cwd,
            self.session.id,
            session_path,
        )
    }

    fn repl_completion_candidates(&self) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        Ok(slash_command_completion_candidates_with_sessions(
            &self.model,
            Some(&self.session.id),
            list_managed_sessions()?
                .into_iter()
                .map(|session| session.id)
                .collect(),
        ))
    }

    fn prepare_turn_runtime(
        &mut self,
        emit_output: bool,
        stream_json: bool,
    ) -> Result<(BuiltRuntime, HookAbortMonitor), Box<dyn std::error::Error>> {
        self.system_prompt = build_system_prompt()?;
        let mut session_state = self.runtime.session().clone();
        if session_state.workspace_root().is_none() {
            session_state = session_state.with_workspace_root(env::current_dir()?);
        }
        let hook_abort_signal = runtime::HookAbortSignal::new();
        let mut runtime = build_runtime(
            session_state,
            &self.session.id,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            emit_output,
            stream_json,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?
        .with_hook_abort_signal(hook_abort_signal.clone());
        if let Some(task_id) = self.resume_task_id.take() {
            runtime = runtime.with_resume_task_id(task_id);
        }
        let pending_file_blocks = std::mem::take(&mut self.pending_file_blocks);
        if !pending_file_blocks.is_empty() {
            runtime.inject_user_blocks(pending_file_blocks)?;
        }
        let hook_abort_monitor = HookAbortMonitor::spawn(hook_abort_signal);

        Ok((runtime, hook_abort_monitor))
    }

    fn replace_runtime(&mut self, runtime: BuiltRuntime) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime.shutdown_plugins()?;
        self.runtime = runtime;
        Ok(())
    }

    fn run_turn(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(true, false)?;
        let mut spinner = Spinner::new();
        let mut stdout = io::stdout();
        spinner.tick(
            "🧠 Thinking...",
            TerminalRenderer::new().color_theme(),
            &mut stdout,
        )?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let result = runtime.run_turn(input, Some(&mut permission_prompter));
        hook_abort_monitor.stop();
        match result {
            Ok(summary) => {
                self.replace_runtime(runtime)?;
                spinner.finish(
                    "✨ Done",
                    TerminalRenderer::new().color_theme(),
                    &mut stdout,
                )?;
                println!();
                if let Some(event) = summary.auto_compaction {
                    println!(
                        "{}",
                        format_auto_compaction_notice(event.removed_message_count)
                    );
                }
                self.persist_session()?;
                Ok(())
            }
            Err(error) => {
                let _ = save_task_registry(runtime.task_registry());
                runtime.shutdown_plugins()?;
                spinner.fail(
                    "❌ Request failed",
                    TerminalRenderer::new().color_theme(),
                    &mut stdout,
                )?;
                Err(Box::new(error))
            }
        }
    }

    fn run_turn_with_output(
        &mut self,
        input: &str,
        output_format: CliOutputFormat,
        compact: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match output_format {
            CliOutputFormat::Text if compact => self.run_prompt_compact(input),
            CliOutputFormat::Text => self.run_turn(input),
            CliOutputFormat::Json => self.run_prompt_json(input),
            CliOutputFormat::StreamJson => self.run_prompt_stream_json(input),
        }
    }

    fn run_prompt_compact(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(false, false)?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let result = runtime.run_turn(input, Some(&mut permission_prompter));
        hook_abort_monitor.stop();
        let summary = match result {
            Ok(summary) => summary,
            Err(error) => {
                let _ = save_task_registry(runtime.task_registry());
                return Err(Box::new(error));
            }
        };
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        let final_text = final_assistant_text(&summary);
        println!("{final_text}");
        Ok(())
    }

    fn run_prompt_json(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(false, false)?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let result = runtime.run_turn(input, Some(&mut permission_prompter));
        hook_abort_monitor.stop();
        let summary = match result {
            Ok(summary) => summary,
            Err(error) => {
                let _ = save_task_registry(runtime.task_registry());
                return Err(Box::new(error));
            }
        };
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        println!(
            "{}",
            json!({
                "message": final_assistant_text(&summary),
                "model": self.model,
                "iterations": summary.iterations,
                "auto_compaction": summary.auto_compaction.map(|event| json!({
                    "removed_messages": event.removed_message_count,
                    "notice": format_auto_compaction_notice(event.removed_message_count),
                })),
                "tool_uses": collect_tool_uses(&summary),
                "tool_results": collect_tool_results(&summary),
                "prompt_cache_events": collect_prompt_cache_events(&summary),
                "usage": {
                    "input_tokens": summary.usage.input_tokens,
                    "output_tokens": summary.usage.output_tokens,
                    "cache_creation_input_tokens": summary.usage.cache_creation_input_tokens,
                    "cache_read_input_tokens": summary.usage.cache_read_input_tokens,
                },
                "estimated_cost": format_usd(
                    summary.usage.estimate_cost_usd_with_pricing(
                        pricing_for_model(&self.model)
                            .unwrap_or_else(runtime::ModelPricing::default_sonnet_tier)
                    ).total_cost_usd()
                )
            })
        );
        Ok(())
    }

    fn run_prompt_stream_json(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        // Host-driven mode: AskUserQuestion exchanges a structured user_question
        // NDJSON event + a single-line answer (so a VS Code webview can render
        // options) instead of printing a human stdin prompt.
        std::env::set_var("HIMALAYAD_INTERACTIVE_PROTOCOL", "stream-json");
        let (mut runtime, hook_abort_monitor) = self.prepare_turn_runtime(false, true)?;
        let mut permission_prompter =
            CliPermissionPrompter::new_with_stream_json(self.permission_mode, true);
        print_stream_json_event(json!({
            "type": "session_meta",
            "session_id": self.session.id,
            "session_path": self.session.path,
            "model": self.model,
        }));
        print_stream_json_event(json!({"type":"message_start"}));
        let result = runtime.run_turn(input, Some(&mut permission_prompter));
        hook_abort_monitor.stop();
        let summary = match result {
            Ok(summary) => summary,
            Err(error) => {
                let _ = save_task_registry(runtime.task_registry());
                print_stream_json_event(json!({"type":"error","error":error.to_string()}));
                print_stream_json_event(json!({"type":"done","iterations":0}));
                return Ok(());
            }
        };
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        // Surface automatic context compaction so the VS Code webview can show
        // when long-horizon memory trimming happened during the turn.
        if let Some(event) = summary.auto_compaction {
            print_stream_json_event(json!({
                "type": "context_event",
                "kind": "context_compact",
                "removed_entries": event.removed_message_count,
                "notice": format_auto_compaction_notice(event.removed_message_count),
            }));
        }
        // Emit any denied tool results as structured permission_denial events so
        // the VS Code extension can surface them to the user.
        for msg in &summary.tool_results {
            if let Some(runtime::ContentBlock::ToolResult {
                tool_name,
                output,
                is_error,
                ..
            }) = msg.blocks.first()
            {
                if *is_error && is_permission_denial_output(output) {
                    print_stream_json_event(json!({
                        "type": "permission_denial",
                        "tool": tool_name,
                        "reason": output,
                    }));
                    print_stream_json_event(recovery_suggestion_event(
                        "permission_denial",
                        tool_name,
                        output,
                    ));
                }
            }
        }
        print_stream_json_event(json!({"type":"done","iterations":summary.iterations}));
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn handle_repl_command(
        &mut self,
        command: SlashCommand,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        Ok(match command {
            SlashCommand::Help => {
                println!("{}", render_repl_help());
                false
            }
            SlashCommand::Status => {
                self.print_status();
                false
            }
            SlashCommand::Bughunter { scope } => {
                self.run_bughunter(scope.as_deref())?;
                false
            }
            SlashCommand::Commit => {
                self.run_commit(None)?;
                false
            }
            SlashCommand::Pr { context } => {
                self.run_pr(context.as_deref())?;
                false
            }
            SlashCommand::Issue { context } => {
                self.run_issue(context.as_deref())?;
                false
            }
            SlashCommand::Ultraplan { task } => {
                self.run_ultraplan(task.as_deref())?;
                false
            }
            SlashCommand::Teleport { target } => {
                Self::run_teleport(target.as_deref())?;
                false
            }
            SlashCommand::DebugToolCall => {
                self.run_debug_tool_call(None)?;
                false
            }
            SlashCommand::Sandbox => {
                Self::print_sandbox_status();
                false
            }
            SlashCommand::Compact => {
                self.compact()?;
                false
            }
            SlashCommand::Model { model } => {
                if model.as_deref() == Some("wizard") {
                    self.run_model_wizard()?;
                    false
                } else {
                    self.set_model(model)?
                }
            }
            SlashCommand::Permissions { mode } => self.set_permissions(mode)?,
            SlashCommand::Clear { confirm } => self.clear_session(confirm)?,
            SlashCommand::Cost => {
                self.print_cost();
                false
            }
            SlashCommand::Resume { session_path } => self.resume_session(session_path)?,
            SlashCommand::Config { section } => {
                Self::print_config(section.as_deref())?;
                false
            }
            SlashCommand::Mcp { action, target } => {
                let args = match (action.as_deref(), target.as_deref()) {
                    (None, None) => None,
                    (Some(action), None) => Some(action.to_string()),
                    (Some(action), Some(target)) => Some(format!("{action} {target}")),
                    (None, Some(target)) => Some(target.to_string()),
                };
                Self::print_mcp(args.as_deref(), CliOutputFormat::Text)?;
                false
            }
            SlashCommand::Memory => {
                Self::print_memory()?;
                false
            }
            SlashCommand::Init => {
                run_init(CliOutputFormat::Text)?;
                false
            }
            SlashCommand::Diff => {
                Self::print_diff()?;
                false
            }
            SlashCommand::Version => {
                Self::print_version(CliOutputFormat::Text);
                false
            }
            SlashCommand::Export { path } => {
                self.export_session(path.as_deref())?;
                false
            }
            SlashCommand::Session { action, target } => {
                self.handle_session_command(action.as_deref(), target.as_deref())?
            }
            SlashCommand::Plugins { action, target } => {
                self.handle_plugins_command(action.as_deref(), target.as_deref())?
            }
            SlashCommand::Agents { args } => {
                Self::print_agents(args.as_deref(), CliOutputFormat::Text)?;
                false
            }
            SlashCommand::Skills { args } => {
                match classify_skills_slash_command(args.as_deref()) {
                    SkillSlashDispatch::Invoke(prompt) => self.run_turn(&prompt)?,
                    SkillSlashDispatch::Local => {
                        Self::print_skills(args.as_deref(), CliOutputFormat::Text)?;
                    }
                }
                false
            }
            SlashCommand::Doctor => {
                println!("{}", render_doctor_report()?.render());
                false
            }
            SlashCommand::Stats => {
                let usage = UsageTracker::from_session(self.runtime.session()).cumulative_usage();
                println!("{}", format_cost_report(usage));
                false
            }
            SlashCommand::LocalCommand { name, args } => {
                run_local_command(
                    parse_local_cli_command_from_parts(&name, args.as_deref())?,
                    CliOutputFormat::Text,
                )?;
                false
            }
            SlashCommand::Review { scope } => {
                run_local_command(LocalCliCommand::Review { scope }, CliOutputFormat::Text)?;
                false
            }
            SlashCommand::Workspace { path } => {
                run_local_command(
                    LocalCliCommand::Workspace {
                        path: path.map(PathBuf::from),
                    },
                    CliOutputFormat::Text,
                )?;
                false
            }
            SlashCommand::Diagnostics { path } => {
                run_local_command(LocalCliCommand::Diagnostics { path }, CliOutputFormat::Text)?;
                false
            }
            SlashCommand::History { count } => {
                self.print_prompt_history(count.as_deref());
                false
            }
            SlashCommand::Plan { mode } => {
                let prompt = mode.unwrap_or_default();
                if prompt.trim().is_empty() {
                    eprintln!("/plan requires a task description");
                } else {
                    run_plan_command(&prompt, CliOutputFormat::Text, self.permission_mode)?;
                }
                false
            }
            SlashCommand::Tasks { args } => {
                run_task_command(
                    parse_task_cli_command(&split_slash_remainder(args.as_deref()))?,
                    CliOutputFormat::Text,
                    self.model.clone(),
                    self.allowed_tools.clone(),
                    self.permission_mode,
                    false,
                    None,
                )?;
                false
            }
            SlashCommand::Cron { args } => {
                run_cron_command(
                    parse_cron_cli_command(&split_slash_remainder(args.as_deref()))?,
                    CliOutputFormat::Text,
                    PermissionMode::ReadOnly,
                )?;
                false
            }
            SlashCommand::Benchmark { args } => {
                run_benchmark_command(
                    parse_benchmark_cli_command(&split_slash_remainder(args.as_deref()))?,
                    CliOutputFormat::Text,
                    &self.model,
                )?;
                false
            }
            SlashCommand::Login
            | SlashCommand::Logout
            | SlashCommand::Vim
            | SlashCommand::Upgrade
            | SlashCommand::Share
            | SlashCommand::Feedback
            | SlashCommand::Files
            | SlashCommand::Fast
            | SlashCommand::Exit
            | SlashCommand::Summary
            | SlashCommand::Desktop
            | SlashCommand::Brief
            | SlashCommand::Advisor
            | SlashCommand::Stickers
            | SlashCommand::Insights
            | SlashCommand::Thinkback
            | SlashCommand::ReleaseNotes
            | SlashCommand::SecurityReview
            | SlashCommand::Keybindings
            | SlashCommand::PrivacySettings
            | SlashCommand::Theme { .. }
            | SlashCommand::Voice { .. }
            | SlashCommand::Usage { .. }
            | SlashCommand::Rename { .. }
            | SlashCommand::Copy { .. }
            | SlashCommand::Hooks { .. }
            | SlashCommand::Context { .. }
            | SlashCommand::Color { .. }
            | SlashCommand::Effort { .. }
            | SlashCommand::Branch { .. }
            | SlashCommand::Rewind { .. }
            | SlashCommand::Ide { .. }
            | SlashCommand::Tag { .. }
            | SlashCommand::OutputStyle { .. }
            | SlashCommand::AddDir { .. } => {
                let cmd_name = command.slash_name();
                eprintln!("{cmd_name} is not yet implemented in this build.");
                false
            }
            SlashCommand::Unknown(name) => {
                eprintln!("{}", format_unknown_slash_command(&name));
                false
            }
        })
    }

    fn persist_session(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime.session().save_to_path(&self.session.path)?;
        save_task_registry(self.runtime.task_registry())?;
        Ok(())
    }

    fn print_status(&self) {
        let cumulative = self.runtime.usage().cumulative_usage();
        let latest = self.runtime.usage().current_turn_usage();
        println!(
            "{}",
            format_status_report(
                &self.model,
                StatusUsage {
                    message_count: self.runtime.session().messages.len(),
                    turns: self.runtime.usage().turns(),
                    latest,
                    cumulative,
                    estimated_tokens: self.runtime.estimated_tokens(),
                },
                self.permission_mode.as_str(),
                &status_context(Some(&self.session.path)).expect("status context should load"),
            )
        );
    }

    fn record_prompt_history(&mut self, prompt: &str) {
        let timestamp_ms = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map_or(self.runtime.session().updated_at_ms, |duration| {
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
            });
        let entry = PromptHistoryEntry {
            timestamp_ms,
            text: prompt.to_string(),
        };
        self.prompt_history.push(entry);
        if let Err(error) = self.runtime.session_mut().push_prompt_entry(prompt) {
            eprintln!("warning: failed to persist prompt history: {error}");
        }
    }

    fn print_prompt_history(&self, count: Option<&str>) {
        let limit = match parse_history_count(count) {
            Ok(limit) => limit,
            Err(message) => {
                eprintln!("{message}");
                return;
            }
        };
        let session_entries = &self.runtime.session().prompt_history;
        let entries = if session_entries.is_empty() {
            if self.prompt_history.is_empty() {
                collect_session_prompt_history(self.runtime.session())
            } else {
                self.prompt_history
                    .iter()
                    .map(|entry| PromptHistoryEntry {
                        timestamp_ms: entry.timestamp_ms,
                        text: entry.text.clone(),
                    })
                    .collect()
            }
        } else {
            session_entries
                .iter()
                .map(|entry| PromptHistoryEntry {
                    timestamp_ms: entry.timestamp_ms,
                    text: entry.text.clone(),
                })
                .collect()
        };
        println!("{}", render_prompt_history_report(&entries, limit));
    }

    fn print_sandbox_status() {
        let cwd = env::current_dir().expect("current dir");
        let loader = ConfigLoader::default_for(&cwd);
        let runtime_config = loader
            .load()
            .unwrap_or_else(|_| runtime::RuntimeConfig::empty());
        println!(
            "{}",
            format_sandbox_report(&resolve_sandbox_status(runtime_config.sandbox(), &cwd))
        );
    }

    fn set_model(&mut self, model: Option<String>) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(model) = model else {
            println!(
                "{}",
                format_model_report(
                    &self.model,
                    self.runtime.session().messages.len(),
                    self.runtime.usage().turns(),
                )
            );
            return Ok(false);
        };

        let model = resolve_model_alias_with_config(&model);

        if model == self.model {
            println!(
                "{}",
                format_model_report(
                    &self.model,
                    self.runtime.session().messages.len(),
                    self.runtime.usage().turns(),
                )
            );
            return Ok(false);
        }

        let previous = self.model.clone();
        let session = self.runtime.session().clone();
        let message_count = session.messages.len();
        let runtime = build_runtime(
            session,
            &self.session.id,
            model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            false,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.replace_runtime(runtime)?;
        self.model.clone_from(&model);
        println!(
            "{}",
            format_model_switch_report(&previous, &model, message_count)
        );
        Ok(true)
    }

    /// Re-run the interactive model selection wizard from the REPL (via
    /// `/model wizard`) so the user can switch provider/model without
    /// restarting the CLI. The selection is persisted and applied immediately.
    fn run_model_wizard(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let Some(selection) = model_selector::run_wizard() else {
            println!("Model wizard cancelled.");
            return Ok(());
        };
        if let Some(ref url) = selection.base_url {
            env::set_var("OPENAI_BASE_URL", url);
        }
        if let Some(ref key) = selection.api_key {
            env::set_var("OPENAI_API_KEY", key);
        }
        if let Ok(cwd) = env::current_dir() {
            let _ = provider_config::persist_wizard_selection(&selection, &cwd);
        }
        let model = resolve_model_alias_with_config(&selection.model);
        let previous = self.model.clone();
        self.model = model.clone();
        let session = self.runtime.session().clone();
        let message_count = session.messages.len();
        self.runtime = build_runtime(
            session,
            &self.session.id,
            model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            false,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        println!(
            "{}",
            format_model_switch_report(&previous, &model, message_count)
        );
        Ok(())
    }

    fn set_permissions(
        &mut self,
        mode: Option<String>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(mode) = mode else {
            println!(
                "{}",
                format_permissions_report(self.permission_mode.as_str())
            );
            return Ok(false);
        };

        let normalized = normalize_permission_mode(&mode).ok_or_else(|| {
            format!(
                "unsupported permission mode '{mode}'. Use {}.",
                public_permission_labels_for_sentence()
            )
        })?;

        if normalized == self.permission_mode.as_str() {
            println!("{}", format_permissions_report(normalized));
            return Ok(false);
        }

        let previous = self.permission_mode.as_str().to_string();
        let session = self.runtime.session().clone();
        self.permission_mode = permission_mode_from_label(normalized);
        let runtime = build_runtime(
            session,
            &self.session.id,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            false,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.replace_runtime(runtime)?;
        println!(
            "{}",
            format_permissions_switch_report(&previous, normalized)
        );
        Ok(true)
    }

    fn clear_session(&mut self, confirm: bool) -> Result<bool, Box<dyn std::error::Error>> {
        if !confirm {
            println!(
                "clear: confirmation required; run /clear --confirm to start a fresh session."
            );
            return Ok(false);
        }

        let previous_session = self.session.clone();
        let workspace_root = env::current_dir()?;
        let session_state = Session::new().with_workspace_root(workspace_root);
        self.session = create_managed_session_handle(&session_state.session_id)?;
        let runtime = build_runtime(
            session_state.with_persistence_path(self.session.path.clone()),
            &self.session.id,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            false,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.replace_runtime(runtime)?;
        println!(
            "Session cleared\n  Mode             fresh session\n  Previous session {}\n  Resume previous  /resume {}\n  Preserved model  {}\n  Permission mode  {}\n  New session      {}\n  Session file     {}",
            previous_session.id,
            previous_session.id,
            self.model,
            self.permission_mode.as_str(),
            self.session.id,
            self.session.path.display(),
        );
        Ok(true)
    }

    fn print_cost(&self) {
        let cumulative = self.runtime.usage().cumulative_usage();
        println!("{}", format_cost_report(cumulative));
    }

    fn resume_session(
        &mut self,
        session_path: Option<String>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(session_ref) = session_path else {
            println!("{}", render_resume_usage());
            return Ok(false);
        };

        let handle = resolve_session_reference(&session_ref)?;
        let session = Session::load_from_path(&handle.path)?;
        let message_count = session.messages.len();
        let session_id = session.session_id.clone();
        let runtime = build_runtime(
            session,
            &handle.id,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            false,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.replace_runtime(runtime)?;
        self.session = SessionHandle {
            id: session_id,
            path: handle.path,
        };
        println!(
            "{}",
            format_resume_report(
                &self.session.path.display().to_string(),
                message_count,
                self.runtime.usage().turns(),
            )
        );
        Ok(true)
    }

    fn print_config(section: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", render_config_report(section)?);
        Ok(())
    }

    fn print_memory() -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", render_memory_report()?);
        Ok(())
    }

    fn print_agents(
        args: Option<&str>,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        match output_format {
            CliOutputFormat::Text => println!("{}", handle_agents_slash_command(args, &cwd)?),
            CliOutputFormat::Json | CliOutputFormat::StreamJson => println!(
                "{}",
                serde_json::to_string_pretty(&handle_agents_slash_command_json(args, &cwd)?)?
            ),
        }
        Ok(())
    }

    fn print_mcp(
        args: Option<&str>,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // `Himalaya mcp serve` starts a stdio MCP server exposing Himalaya's built-in
        // tools. All other `mcp` subcommands fall through to the existing
        // configured-server reporter (`list`, `status`, ...).
        if matches!(args.map(str::trim), Some("serve")) {
            return run_mcp_serve();
        }
        let cwd = env::current_dir()?;
        match output_format {
            CliOutputFormat::Text => println!("{}", handle_mcp_slash_command(args, &cwd)?),
            CliOutputFormat::Json | CliOutputFormat::StreamJson => println!(
                "{}",
                serde_json::to_string_pretty(&handle_mcp_slash_command_json(args, &cwd)?)?
            ),
        }
        Ok(())
    }

    fn print_skills(
        args: Option<&str>,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        match output_format {
            CliOutputFormat::Text => println!("{}", handle_skills_slash_command(args, &cwd)?),
            CliOutputFormat::Json | CliOutputFormat::StreamJson => println!(
                "{}",
                serde_json::to_string_pretty(&handle_skills_slash_command_json(args, &cwd)?)?
            ),
        }
        Ok(())
    }

    fn print_plugins(
        action: Option<&str>,
        target: Option<&str>,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        let loader = ConfigLoader::default_for(&cwd);
        let runtime_config = loader.load()?;
        let mut manager = build_plugin_manager(&cwd, &loader, &runtime_config);
        let result = handle_plugins_slash_command(action, target, &mut manager)?;
        match output_format {
            CliOutputFormat::Text => println!("{}", result.message),
            CliOutputFormat::Json | CliOutputFormat::StreamJson => println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "kind": "plugin",
                    "action": action.unwrap_or("list"),
                    "target": target,
                    "message": result.message,
                    "reload_runtime": result.reload_runtime,
                }))?
            ),
        }
        Ok(())
    }

    fn print_diff() -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", render_diff_report()?);
        Ok(())
    }

    fn print_version(output_format: CliOutputFormat) {
        let _ = crate::print_version(output_format);
    }

    fn export_session(
        &self,
        requested_path: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let export_path = resolve_export_path(requested_path, self.runtime.session())?;
        fs::write(&export_path, render_export_text(self.runtime.session()))?;
        println!(
            "Export\n  Result           wrote transcript\n  File             {}\n  Messages         {}",
            export_path.display(),
            self.runtime.session().messages.len(),
        );
        Ok(())
    }

    fn handle_session_command(
        &mut self,
        action: Option<&str>,
        target: Option<&str>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        match action {
            None | Some("list") => {
                println!("{}", render_session_list(&self.session.id)?);
                Ok(false)
            }
            Some("switch") => {
                let Some(target) = target else {
                    println!("Usage: /session switch <session-id>");
                    return Ok(false);
                };
                let handle = resolve_session_reference(target)?;
                let session = Session::load_from_path(&handle.path)?;
                let message_count = session.messages.len();
                let session_id = session.session_id.clone();
                let runtime = build_runtime(
                    session,
                    &handle.id,
                    self.model.clone(),
                    self.system_prompt.clone(),
                    true,
                    true,
                    false,
                    self.allowed_tools.clone(),
                    self.permission_mode,
                    None,
                )?;
                self.replace_runtime(runtime)?;
                self.session = SessionHandle {
                    id: session_id,
                    path: handle.path,
                };
                println!(
                    "Session switched\n  Active session   {}\n  File             {}\n  Messages         {}",
                    self.session.id,
                    self.session.path.display(),
                    message_count,
                );
                Ok(true)
            }
            Some("fork") => {
                let forked = self.runtime.fork_session(target.map(ToOwned::to_owned));
                let parent_session_id = self.session.id.clone();
                let handle = create_managed_session_handle(&forked.session_id)?;
                let branch_name = forked
                    .fork
                    .as_ref()
                    .and_then(|fork| fork.branch_name.clone());
                let forked = forked.with_persistence_path(handle.path.clone());
                let message_count = forked.messages.len();
                forked.save_to_path(&handle.path)?;
                let runtime = build_runtime(
                    forked,
                    &handle.id,
                    self.model.clone(),
                    self.system_prompt.clone(),
                    true,
                    true,
                    false,
                    self.allowed_tools.clone(),
                    self.permission_mode,
                    None,
                )?;
                self.replace_runtime(runtime)?;
                self.session = handle;
                println!(
                    "Session forked\n  Parent session   {}\n  Active session   {}\n  Branch           {}\n  File             {}\n  Messages         {}",
                    parent_session_id,
                    self.session.id,
                    branch_name.as_deref().unwrap_or("(unnamed)"),
                    self.session.path.display(),
                    message_count,
                );
                Ok(true)
            }
            Some("delete") => {
                let Some(target) = target else {
                    println!("Usage: /session delete <session-id> [--force]");
                    return Ok(false);
                };
                let handle = resolve_session_reference(target)?;
                if handle.id == self.session.id {
                    println!(
                        "delete: refusing to delete the active session '{}'.\nSwitch to another session first with /session switch <session-id>.",
                        handle.id
                    );
                    return Ok(false);
                }
                if !confirm_session_deletion(&handle.id) {
                    println!("delete: cancelled.");
                    return Ok(false);
                }
                delete_managed_session(&handle.path)?;
                println!(
                    "Session deleted\n  Deleted session  {}\n  File             {}",
                    handle.id,
                    handle.path.display(),
                );
                Ok(false)
            }
            Some("delete-force") => {
                let Some(target) = target else {
                    println!("Usage: /session delete <session-id> [--force]");
                    return Ok(false);
                };
                let handle = resolve_session_reference(target)?;
                if handle.id == self.session.id {
                    println!(
                        "delete: refusing to delete the active session '{}'.\nSwitch to another session first with /session switch <session-id>.",
                        handle.id
                    );
                    return Ok(false);
                }
                delete_managed_session(&handle.path)?;
                println!(
                    "Session deleted\n  Deleted session  {}\n  File             {}",
                    handle.id,
                    handle.path.display(),
                );
                Ok(false)
            }
            Some(other) => {
                println!(
                    "Unknown /session action '{other}'. Use /session list, /session switch <session-id>, /session fork [branch-name], or /session delete <session-id> [--force]."
                );
                Ok(false)
            }
        }
    }

    fn handle_plugins_command(
        &mut self,
        action: Option<&str>,
        target: Option<&str>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let cwd = env::current_dir()?;
        let loader = ConfigLoader::default_for(&cwd);
        let runtime_config = loader.load()?;
        let mut manager = build_plugin_manager(&cwd, &loader, &runtime_config);
        let result = handle_plugins_slash_command(action, target, &mut manager)?;
        println!("{}", result.message);
        if result.reload_runtime {
            self.reload_runtime_features()?;
        }
        Ok(false)
    }

    fn reload_runtime_features(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let runtime = build_runtime(
            self.runtime.session().clone(),
            &self.session.id,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            false,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.replace_runtime(runtime)?;
        self.persist_session()
    }

    fn compact(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let result = self.runtime.compact(CompactionConfig::default());
        let removed = result.removed_message_count;
        let kept = result.compacted_session.messages.len();
        let skipped = removed == 0;
        let runtime = build_runtime(
            result.compacted_session,
            &self.session.id,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            true,
            false,
            self.allowed_tools.clone(),
            self.permission_mode,
            None,
        )?;
        self.replace_runtime(runtime)?;
        self.persist_session()?;
        println!("{}", format_compact_report(removed, kept, skipped));
        Ok(())
    }

    fn run_internal_prompt_text_with_progress(
        &self,
        prompt: &str,
        enable_tools: bool,
        progress: Option<InternalPromptProgressReporter>,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let session = self.runtime.session().clone();
        let mut runtime = build_runtime(
            session,
            &self.session.id,
            self.model.clone(),
            self.system_prompt.clone(),
            enable_tools,
            false,
            false,
            self.allowed_tools.clone(),
            self.permission_mode,
            progress,
        )?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let summary = runtime.run_turn(prompt, Some(&mut permission_prompter))?;
        let text = final_assistant_text(&summary).trim().to_string();
        runtime.shutdown_plugins()?;
        Ok(text)
    }

    fn run_internal_prompt_text(
        &self,
        prompt: &str,
        enable_tools: bool,
    ) -> Result<String, Box<dyn std::error::Error>> {
        self.run_internal_prompt_text_with_progress(prompt, enable_tools, None)
    }

    fn run_bughunter(&self, scope: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", format_bughunter_report(scope));
        Ok(())
    }

    fn run_ultraplan(&self, task: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", format_ultraplan_report(task));
        Ok(())
    }

    fn run_teleport(target: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        let Some(target) = target.map(str::trim).filter(|value| !value.is_empty()) else {
            println!("Usage: /teleport <symbol-or-path>");
            return Ok(());
        };

        println!("{}", render_teleport_report(target)?);
        Ok(())
    }

    fn run_debug_tool_call(&self, args: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        validate_no_args("/debug-tool-call", args)?;
        println!("{}", render_last_tool_debug_report(self.runtime.session())?);
        Ok(())
    }

    fn run_commit(&mut self, args: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        validate_no_args("/commit", args)?;
        let status = git_output(&["status", "--short", "--branch"])?;
        let summary = parse_git_workspace_summary(Some(&status));
        let branch = parse_git_status_branch(Some(&status));
        if summary.is_clean() {
            println!("{}", format_commit_skipped_report());
            return Ok(());
        }

        println!(
            "{}",
            format_commit_preflight_report(branch.as_deref(), summary)
        );
        Ok(())
    }

    fn run_pr(&self, context: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        let branch =
            resolve_git_branch_for(&env::current_dir()?).unwrap_or_else(|| "unknown".to_string());
        println!("{}", format_pr_report(&branch, context));
        Ok(())
    }

    fn run_issue(&self, context: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", format_issue_report(context));
        Ok(())
    }
}

fn format_task_summary(task: &runtime::task_registry::Task) -> String {
    format!(
        "{id:<28} {status:<24} attempts={attempt:<3} checkpoints={checkpoints:<3} prompt={prompt}",
        id = task.task_id,
        status = task.status,
        attempt = task.attempt,
        checkpoints = task.checkpoints.len(),
        prompt = task.prompt.replace('\n', " ")
    )
}

fn render_task_list(
    status: Option<runtime::TaskStatus>,
) -> Result<String, Box<dyn std::error::Error>> {
    let registry = load_task_registry()?;
    let mut tasks = registry.list(status);
    tasks.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    let mut lines = vec![format!(
        "Tasks\n  Directory         {}",
        task_registry_dir()?.display()
    )];
    if tasks.is_empty() {
        lines.push("  No tasks saved yet.".to_string());
        return Ok(lines.join("\n"));
    }
    lines.extend(tasks.iter().map(format_task_summary));
    Ok(lines.join("\n"))
}

fn render_task_show(task_id: &str) -> Result<String, Box<dyn std::error::Error>> {
    let registry = load_task_registry()?;
    let task = registry
        .get(task_id)
        .ok_or_else(|| format!("task not found: {task_id}"))?;
    let mut lines = vec![
        format!("Task {}", task.task_id),
        format!("  Status            {}", task.status),
        format!("  Attempts          {}", task.attempt),
        format!("  Prompt            {}", task.prompt.replace('\n', " ")),
        format!(
            "  Description       {}",
            task.description.as_deref().unwrap_or("")
        ),
        format!(
            "  Team              {}",
            task.team_id.as_deref().unwrap_or("")
        ),
        format!("  Checkpoints       {}", task.checkpoints.len()),
        format!("  Plan persisted    {}", task.plan.is_some()),
        format!("  Recovery events   {}", task.recovery_events.len()),
        format!("  Team events       {}", task.team_events.len()),
        format!(
            "  Recovery actions  {}",
            task.recovery_action_executions.len()
        ),
        format!("  Route feedback    {}", task.route_feedback.len()),
    ];
    if let Some(plan) = &task.plan {
        if let Some(cursor) = &plan.resume_cursor {
            lines.push(format!(
                "  Resume cursor     node={} resumable={} completed={}",
                cursor.node_id.as_deref().unwrap_or(""),
                cursor.resumable_nodes.len(),
                cursor.completed_nodes.len()
            ));
        }
    }
    if let Some(result) = &task.verification_result {
        lines.push(format!(
            "  Verification      passed={} {}",
            result.passed, result.summary
        ));
    }
    let ledger = registry.ledger_for_task(task_id);
    if !ledger.is_empty() {
        lines.push("  Ledger".to_string());
        lines.extend(ledger.into_iter().map(|entry| {
            format!(
                "    #{:<3} {:<24} {:<24} {}",
                entry.seq,
                entry.event,
                entry.status,
                entry.message.unwrap_or_default()
            )
        }));
    }
    Ok(lines.join("\n"))
}

fn format_cron_summary(entry: &runtime::team_cron_registry::CronEntry) -> String {
    format!(
        "{id:<28} {enabled:<8} runs={runs:<3} schedule={schedule:<14} prompt={prompt}",
        id = entry.cron_id,
        enabled = if entry.enabled { "enabled" } else { "disabled" },
        runs = entry.run_count,
        schedule = entry.schedule,
        prompt = entry.prompt.replace('\n', " ")
    )
}

fn render_cron_list() -> Result<String, Box<dyn std::error::Error>> {
    let registry = load_cron_registry()?;
    let mut entries = registry.list(false);
    entries.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    let mut lines = vec![format!(
        "Cron\n  Directory         {}",
        cron_registry_dir()?.display()
    )];
    if entries.is_empty() {
        lines.push("  No scheduled tasks saved yet.".to_string());
        return Ok(lines.join("\n"));
    }
    lines.extend(entries.iter().map(format_cron_summary));
    Ok(lines.join("\n"))
}

fn print_cron_json(value: Value) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn print_cron_output(
    value: Value,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    match output_format {
        CliOutputFormat::StreamJson => print_stream_json_event(value),
        CliOutputFormat::Json | CliOutputFormat::Text => print_cron_json(value)?,
    }
    Ok(())
}

fn run_cron_command(
    command: CronCliCommand,
    output_format: CliOutputFormat,
    permission_mode: PermissionMode,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        CronCliCommand::List => match output_format {
            CliOutputFormat::Text => println!("{}", render_cron_list()?),
            CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                let registry = load_cron_registry()?;
                print_cron_output(
                    json!({"type":"cron_list","crons":registry.list(false)}),
                    output_format,
                )?;
            }
        },
        CronCliCommand::Add {
            schedule,
            prompt,
            description,
        } => {
            let registry = load_cron_registry()?;
            let entry = registry.create(&schedule, &prompt, description.as_deref());
            save_cron_registry(&registry)?;
            match output_format {
                CliOutputFormat::Text => println!("created cron {}", entry.cron_id),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_cron_output(json!({"type":"cron_create","cron":entry}), output_format)?;
                }
            }
        }
        CronCliCommand::Remove { cron_id } => {
            let registry = load_cron_registry()?;
            let entry = registry.delete(&cron_id)?;
            save_cron_registry(&registry)?;
            match output_format {
                CliOutputFormat::Text => println!("removed cron {}", entry.cron_id),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_cron_output(json!({"type":"cron_delete","cron":entry}), output_format)?;
                }
            }
        }
        CronCliCommand::Run {
            max_fires,
            max_ticks,
        } => {
            run_cron_tick_command(max_fires, max_ticks, output_format, permission_mode)?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
struct CronScheduledTaskReport {
    cron_id: String,
    task_id: String,
    task_type: String,
    prompt: String,
    memory_context: runtime::TaskMemoryContext,
}

#[derive(Debug, Clone, Serialize)]
struct CronRunFailure {
    cron_id: String,
    reason: String,
}

#[derive(Debug, Clone, Serialize)]
struct CronRunSummary {
    due: usize,
    fired: usize,
    failed: usize,
    scheduler_runs: usize,
    scheduler_errors: usize,
    max_fires: usize,
    max_ticks: usize,
    permission_mode: String,
}

/// Fire all cron entries due now by creating durable task packets and handing
/// them to the persistent scheduler daemon. A cron run is recorded only after
/// its task has been created and planned successfully, so crash recovery can
/// resume from `.Himalaya/tasks` and `.Himalaya/scheduler`.
fn run_cron_tick_command(
    max_fires: usize,
    max_ticks: usize,
    output_format: CliOutputFormat,
    permission_mode: PermissionMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let registry = load_cron_registry()?;
    let task_registry = load_task_registry()?;
    let worker_registry = load_worker_registry()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let due_count = registry.due_entries(now).len();
    let mut warnings = Vec::new();
    let memory_store = load_task_memory_store()
        .ok()
        .filter(|store| !store.entries().is_empty())
        .unwrap_or_else(|| runtime::TaskMemoryStore::from_tasks(&task_registry.list(None)));
    if memory_store.entries().is_empty() {
        warnings
            .push("task memory store is empty; cron task starts without prior memory".to_string());
    }
    let mut created_tasks = Vec::new();
    let mut report = runtime::team_cron_registry::CronTickReport::default();
    let mut due_entries = registry.due_entries(now);
    due_entries.sort_by(|left, right| left.cron_id.cmp(&right.cron_id));
    for entry in due_entries.into_iter().take(max_fires) {
        match create_durable_cron_task(&task_registry, &entry, &memory_store).and_then(
            |scheduled| {
                save_task_registry(&task_registry).map_err(|error| error.to_string())?;
                registry.record_run(&entry.cron_id)?;
                save_cron_registry(&registry).map_err(|error| error.to_string())?;
                Ok(scheduled)
            },
        ) {
            Ok(scheduled) => {
                report.fired.push(entry.cron_id);
                created_tasks.push(scheduled);
            }
            Err(reason) => report.failed.push((entry.cron_id, reason)),
        }
    }
    save_task_registry(&task_registry)?;
    save_worker_registry(&worker_registry)?;
    save_cron_registry(&registry)?;

    let coordinator =
        build_autonomous_run_coordinator(&task_registry, &worker_registry, permission_mode)?;
    let mut autonomous_run = None;
    let mut scheduler_errors = Vec::new();
    if !created_tasks.is_empty() {
        match coordinator.run_with_persist(max_ticks, |_| {
            save_task_registry(&task_registry)
                .map_err(|error| io::Error::other(error.to_string()))?;
            save_worker_registry(&worker_registry)
                .map_err(|error| io::Error::other(error.to_string()))?;
            Ok(())
        }) {
            Ok(report) => {
                autonomous_run = Some(report);
                save_task_registry(&task_registry)?;
                save_worker_registry(&worker_registry)?;
            }
            Err(error) => {
                scheduler_errors.push(error.to_string());
            }
        }
    }
    let scheduler_runs = autonomous_run
        .as_ref()
        .map(|report| report.scheduler_runs.clone())
        .unwrap_or_default();
    let scheduler_state = autonomous_run
        .as_ref()
        .and_then(|report| report.latest_daemon_state.clone())
        .or_else(|| {
            let scheduler = runtime::DurableTaskScheduler::with_workers(
                task_registry.clone(),
                runtime::VerificationRunner::new(Some(env::current_dir().ok()?)),
                worker_registry.clone(),
            )
            .with_permission_mode(permission_mode);
            let daemon = runtime::SchedulerDaemon::new(scheduler, scheduler_state_dir().ok()?);
            daemon.load_state().ok()
        });

    let failures = report
        .failed
        .iter()
        .map(|(cron_id, reason)| CronRunFailure {
            cron_id: cron_id.clone(),
            reason: reason.clone(),
        })
        .collect::<Vec<_>>();
    let summary = CronRunSummary {
        due: due_count,
        fired: created_tasks.len(),
        failed: failures.len(),
        scheduler_runs: scheduler_runs.len(),
        scheduler_errors: scheduler_errors.len(),
        max_fires,
        max_ticks,
        permission_mode: permission_mode.as_str().to_string(),
    };
    let value = json!({
        "type": "cron_run",
        "summary": summary,
        "created_tasks": created_tasks,
        "failed": failures,
        "scheduler": {
            "runs": scheduler_runs,
            "errors": scheduler_errors,
            "state": scheduler_state,
            "state_path": scheduler_state_dir()?.join("state.json"),
            "events_path": scheduler_state_dir()?.join("events.jsonl"),
            "runs_path": coordinator.runs_path(),
        },
        "autonomous_run": autonomous_run,
        "paths": {
            "cron_registry": cron_registry_dir()?.join("crons.json"),
            "task_registry": task_registry_dir()?.join("tasks.json"),
            "worker_registry": worker_registry_dir()?.join("workers.json"),
            "task_memory": task_memory_dir()?.join("tasks.json"),
            "route_feedback": route_feedback_dir()?.join("feedback.json"),
        },
        "warnings": warnings,
    });

    match output_format {
        CliOutputFormat::Text => {
            if value["summary"]["due"].as_u64().unwrap_or(0) == 0 {
                println!("no cron entries were due");
            } else {
                println!("{}", render_cron_run_text(&value));
            }
        }
        CliOutputFormat::Json => print_cron_output(value, output_format)?,
        CliOutputFormat::StreamJson => {
            for task in value["created_tasks"].as_array().into_iter().flatten() {
                print_cron_output(
                    json!({
                        "type":"cron_fired",
                        "cron_id":task["cron_id"],
                        "task_id":task["task_id"],
                    }),
                    output_format,
                )?;
            }
            for failure in value["failed"].as_array().into_iter().flatten() {
                print_cron_output(
                    json!({
                        "type":"cron_fire_failed",
                        "cron_id":failure["cron_id"],
                        "reason":failure["reason"],
                    }),
                    output_format,
                )?;
            }
            print_cron_output(value, output_format)?;
        }
    }
    Ok(())
}

fn create_durable_cron_task(
    registry: &runtime::TaskRegistry,
    entry: &runtime::team_cron_registry::CronEntry,
    memory_store: &runtime::TaskMemoryStore,
) -> Result<CronScheduledTaskReport, String> {
    let packet = runtime::TaskPacket {
        objective: entry.prompt.trim().to_string(),
        scope: "cron-scheduled-agent".to_string(),
        repo: ".".to_string(),
        branch_policy: format!("cron:{}; use current branch", entry.cron_id),
        acceptance_tests: Vec::new(),
        commit_policy: "do not commit automatically from cron".to_string(),
        reporting_contract: "enqueue durable task, run scheduler daemon, persist task memory and route feedback".to_string(),
        escalation_policy: "background cron recovery is policy-gated; block when user permission or elevated access is required".to_string(),
    };
    let task = registry
        .create_from_packet(packet)
        .map_err(|error| error.to_string())?;
    let task = persist_task_packet_plan(registry, &task).map_err(|error| error.to_string())?;
    let memory_context = memory_store.context_for_task(&task);
    Ok(CronScheduledTaskReport {
        cron_id: entry.cron_id.clone(),
        task_id: task.task_id.clone(),
        task_type: runtime::TaskMemoryStore::task_type_for(&task),
        prompt: entry.prompt.clone(),
        memory_context,
    })
}

fn render_cron_run_text(value: &Value) -> String {
    let summary = &value["summary"];
    let mut lines = vec![format!(
        "Cron run\n  Due               {}\n  Fired             {}\n  Failed            {}\n  Scheduler ticks   {}\n  Permission mode   {}",
        summary["due"].as_u64().unwrap_or(0),
        summary["fired"].as_u64().unwrap_or(0),
        summary["failed"].as_u64().unwrap_or(0),
        summary["scheduler_runs"].as_u64().unwrap_or(0),
        summary["permission_mode"].as_str().unwrap_or("read-only"),
    )];
    for task in value["created_tasks"].as_array().into_iter().flatten() {
        lines.push(format!(
            "  Fired cron        {} -> {}",
            task["cron_id"].as_str().unwrap_or_default(),
            task["task_id"].as_str().unwrap_or_default()
        ));
    }
    for failure in value["failed"].as_array().into_iter().flatten() {
        lines.push(format!(
            "  Failed cron       {}: {}",
            failure["cron_id"].as_str().unwrap_or_default(),
            failure["reason"].as_str().unwrap_or_default()
        ));
    }
    for error in value["scheduler"]["errors"]
        .as_array()
        .into_iter()
        .flatten()
    {
        lines.push(format!(
            "  Scheduler error   {}",
            error.as_str().unwrap_or_default()
        ));
    }
    lines.join("\n")
}
fn print_task_json(value: Value) -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn print_task_output(
    value: Value,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    match output_format {
        CliOutputFormat::StreamJson => print_stream_json_event(value),
        CliOutputFormat::Json => print_task_json(value)?,
        CliOutputFormat::Text => print_task_json(value)?,
    }
    Ok(())
}

fn run_task_command(
    command: TaskCliCommand,
    output_format: CliOutputFormat,
    model: String,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    compact: bool,
    reasoning_effort: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        TaskCliCommand::List { status } => match output_format {
            CliOutputFormat::Text => println!("{}", render_task_list(status)?),
            CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                let registry = load_task_registry()?;
                print_task_output(
                    json!({"type":"task_list","tasks":registry.list(status)}),
                    output_format,
                )?;
            }
        },
        TaskCliCommand::Show { task_id } => match output_format {
            CliOutputFormat::Text => println!("{}", render_task_show(&task_id)?),
            CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                let registry = load_task_registry()?;
                let task = registry
                    .get(&task_id)
                    .ok_or_else(|| format!("task not found: {task_id}"))?;
                print_task_output(
                    json!({
                        "type":"task_show",
                        "task":task,
                        "ledger":registry.ledger_for_task(&task_id),
                        "event_log":registry.event_log_for_task(&task_id),
                    }),
                    output_format,
                )?;
            }
        },
        TaskCliCommand::Status { task_id } => {
            let registry = load_task_registry()?;
            let task = registry
                .get(&task_id)
                .ok_or_else(|| format!("task not found: {task_id}"))?;
            let value = task_status_value(&registry, task, "task_status");
            match output_format {
                CliOutputFormat::Text => println!("{}", render_task_status_text(&value)),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_task_output(value, output_format)?;
                }
            }
        }
        TaskCliCommand::Report { task_id } => {
            let registry = load_task_registry()?;
            let task = registry
                .get(&task_id)
                .ok_or_else(|| format!("task not found: {task_id}"))?;
            let value = task_report_value(&registry, task)?;
            match output_format {
                CliOutputFormat::Text => println!("{}", render_task_report_text(&value)),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_task_output(value, output_format)?;
                }
            }
        }
        TaskCliCommand::Review { task_id } => {
            let registry = load_task_registry()?;
            let task = registry
                .get(&task_id)
                .ok_or_else(|| format!("task not found: {task_id}"))?;
            let value = task_review_value(&registry, task)?;
            match output_format {
                CliOutputFormat::Text => println!("{}", render_task_review_text(&value)),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_task_output(value, output_format)?;
                }
            }
        }
        TaskCliCommand::Packet { command } => {
            run_task_packet_command(command, output_format)?;
        }
        TaskCliCommand::Scheduler { command } => {
            run_task_scheduler_command(command, output_format, permission_mode)?;
        }
        TaskCliCommand::Daemon { command } => {
            run_task_daemon_command(command, output_format, permission_mode)?;
        }
        TaskCliCommand::Execute { task_id, from_node } => {
            let registry = load_task_registry()?;
            let worker_registry = load_worker_registry()?;
            let runner = runtime::VerificationRunner::new(Some(env::current_dir()?));
            let engine = runtime::TaskExecutionEngine::with_workers(
                registry.clone(),
                runner,
                worker_registry.clone(),
            );
            let _ = engine.assign_team(&task_id)?;
            let report =
                engine.execute_with_recovery(&task_id, from_node.as_deref(), permission_mode)?;
            save_task_registry(&registry)?;
            save_worker_registry(&worker_registry)?;
            match output_format {
                CliOutputFormat::Text => println!("{}", report.message),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_task_output(
                        json!({"type":"task_execution","outcome":report.outcome,"report":report}),
                        output_format,
                    )?;
                }
            }
        }
        TaskCliCommand::Recover { task_id } => {
            let registry = load_task_registry()?;
            let task = registry
                .get(&task_id)
                .ok_or_else(|| format!("task not found: {task_id}"))?;
            let Some(last_event) = task.recovery_events.last().cloned() else {
                return Err(format!("task {task_id} has no recovery event to execute").into());
            };
            let scenario = match last_event {
                runtime::RecoveryEvent::RecoveryAttempted { scenario, .. } => scenario,
                runtime::RecoveryEvent::RecoverySucceeded
                | runtime::RecoveryEvent::RecoveryFailed
                | runtime::RecoveryEvent::Escalated => runtime::FailureScenario::ProviderFailure,
            };
            let mut orchestrator = runtime::RecoveryOrchestrator::new();
            let outcome = orchestrator.recover_once(scenario);
            let engine = runtime::RecoveryActionEngine::new();
            let node_id = task
                .plan
                .and_then(|plan| plan.resume_cursor)
                .and_then(|cursor| cursor.node_id);
            let plan = engine.plan(task_id.clone(), &outcome, node_id);
            let execution = engine.execute_against_registry(plan, permission_mode, &registry);
            save_task_registry(&registry)?;
            match output_format {
                CliOutputFormat::Text => println!(
                    "executed {} recovery action(s) for task {task_id}",
                    execution.results.len()
                ),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_task_output(
                        json!({"type":"task_recovery","execution":execution}),
                        output_format,
                    )?;
                }
            }
        }
        TaskCliCommand::VerifyTask { task_id } => {
            let registry = load_task_registry()?;
            let task = registry
                .get(&task_id)
                .ok_or_else(|| format!("task not found: {task_id}"))?;
            let Some(packet) = task.task_packet.as_ref() else {
                return Err(format!("task {task_id} has no task packet acceptance tests").into());
            };
            let request = runtime::build_verification_request(
                &task_id,
                packet,
                runtime::infer_verification_policy(task.task_packet.as_ref()),
            );
            let runner = runtime::VerificationRunner::new(Some(env::current_dir()?));
            let result = runner.run(&request);
            let _ = registry.record_verification(&task_id, result.clone())?;
            save_task_registry(&registry)?;
            match output_format {
                CliOutputFormat::Text => println!("{}", result.summary),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_task_output(
                        json!({"type":"task_verification","result":result}),
                        output_format,
                    )?;
                }
            }
        }
        TaskCliCommand::Retry { task_id, node_id } => {
            let registry = load_task_registry()?;
            let task = registry.retry_plan_node(&task_id, &node_id)?;
            save_task_registry(&registry)?;
            match output_format {
                CliOutputFormat::Text => println!(
                    "scheduled retry for node {node_id} on task {}",
                    task.task_id
                ),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_task_output(
                        json!({"type":"task_node_retry","task":task,"node_id":node_id}),
                        output_format,
                    )?;
                }
            }
        }
        TaskCliCommand::Verify {
            task_id,
            node_id,
            command,
        } => {
            let registry = load_task_registry()?;
            let task =
                registry.attach_node_verification(&task_id, &node_id, command.clone(), true)?;
            save_task_registry(&registry)?;
            match output_format {
                CliOutputFormat::Text => {
                    println!("recorded verification gate for node {node_id}: {command}")
                }
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_task_output(
                        json!({"type":"task_node_verification","task":task,"node_id":node_id,"command":command}),
                        output_format,
                    )?;
                }
            }
        }
        TaskCliCommand::Compact { task_id, keep_last } => {
            let registry = load_task_registry()?;
            let task = registry.compact_task_events(&task_id, keep_last)?;
            save_task_registry(&registry)?;
            match output_format {
                CliOutputFormat::Text => println!(
                    "compacted task {} events to keep_last={keep_last}",
                    task.task_id
                ),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_task_output(
                        json!({"type":"task_compacted","task":task,"keep_last":keep_last}),
                        output_format,
                    )?;
                }
            }
        }
        TaskCliCommand::Cancel { task_id } => {
            let registry = load_task_registry()?;
            let task = registry.cancel(&task_id)?;
            save_task_registry(&registry)?;
            match output_format {
                CliOutputFormat::Text => println!("cancelled task {}", task.task_id),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_task_output(json!({"type":"task_cancelled","task":task}), output_format)?;
                }
            }
        }
        TaskCliCommand::Resume {
            task_id,
            from_node,
            prompt,
        } => {
            if let Some(node_id) = from_node.as_ref() {
                let registry = load_task_registry()?;
                let _ = registry.retry_plan_node(&task_id, node_id)?;
                save_task_registry(&registry)?;
            }
            let mut cli = LiveCli::new(model.clone(), true, allowed_tools, permission_mode)?;
            cli.set_reasoning_effort(reasoning_effort);
            cli.resume_task_once(task_id.clone());
            let prompt = prompt.unwrap_or_else(|| {
                from_node
                    .map(|node_id| format!("Resume task {task_id} from node {node_id}"))
                    .unwrap_or_else(|| format!("Resume task {task_id}"))
            });
            cli.run_turn_with_output(&prompt, output_format, compact)?;
        }
    }
    Ok(())
}

fn print_local_command_output(
    value: Value,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    match output_format {
        CliOutputFormat::StreamJson => print_stream_json_event(value),
        CliOutputFormat::Json => print_task_json(value)?,
        CliOutputFormat::Text => println!("{}", render_local_command_text(&value)),
    }
    Ok(())
}

fn run_local_command(
    command: LocalCliCommand,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let value = local_command_value(command)?;
    print_local_command_output(value, output_format)
}

fn local_command_value(command: LocalCliCommand) -> Result<Value, Box<dyn std::error::Error>> {
    match command {
        LocalCliCommand::Test { filter } => verification_local_command_value("test", filter),
        LocalCliCommand::Lint { filter } => verification_local_command_value("lint", filter),
        LocalCliCommand::Build { target } => verification_local_command_value("build", target),
        LocalCliCommand::Review { scope } => review_local_command_value(scope),
        LocalCliCommand::Diagnostics { path } => diagnostics_local_command_value(path),
        LocalCliCommand::Workspace { path } => workspace_local_command_value(path),
    }
}

fn verification_local_command_value(
    command: &str,
    argument: Option<String>,
) -> Result<Value, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let Some(executable) = detect_local_verification_command(command, argument.as_deref(), &cwd)
    else {
        return Ok(json!({
            "type": "local_command",
            "command": command,
            "status": "skipped",
            "cwd": cwd,
            "request": { "argument": argument },
            "execution": null,
            "summary": format!("no {command} command detected for this workspace"),
        }));
    };
    let request = runtime::VerificationRequest {
        task_id: format!("local-{command}"),
        objective: format!("Run local {command}"),
        scope: cwd.display().to_string(),
        acceptance_tests: vec![executable.clone()],
        reporting_contract: "return local command status and captured output".to_string(),
        policy: runtime::VerificationPolicy::Targeted,
        required_green_level: runtime::VerificationPolicy::Targeted.required_green_level(),
    };
    let runner = runtime::VerificationRunner::new(Some(cwd.clone()));
    let result = runner.run(&request);
    Ok(json!({
        "type": "local_command",
        "command": command,
        "status": if result.passed { "passed" } else { "failed" },
        "cwd": cwd,
        "request": { "argument": argument },
        "execution": {
            "command": executable,
            "passed": result.passed,
            "summary": result.summary,
            "evidence": result.evidence,
        },
        "summary": if result.passed { format!("{command} passed") } else { format!("{command} failed") },
    }))
}

fn detect_local_verification_command(
    command: &str,
    argument: Option<&str>,
    cwd: &Path,
) -> Option<String> {
    if package_script_exists(cwd, command) {
        let mut executable = format!("npm run {command}");
        if let Some(argument) = argument.filter(|value| !value.trim().is_empty()) {
            executable.push_str(" -- ");
            executable.push_str(argument.trim());
        }
        return Some(executable);
    }

    let cargo_manifest = if cwd.join("Cargo.toml").exists() {
        Some(PathBuf::from("Cargo.toml"))
    } else if cwd.join("rust").join("Cargo.toml").exists() {
        Some(PathBuf::from("rust/Cargo.toml"))
    } else {
        None
    }?;
    let manifest_arg = if cargo_manifest == PathBuf::from("Cargo.toml") {
        String::new()
    } else {
        format!(" --manifest-path {}", cargo_manifest.display())
    };

    match command {
        "test" => {
            let mut executable = format!("cargo test{manifest_arg}");
            if let Some(argument) = argument.filter(|value| !value.trim().is_empty()) {
                executable.push(' ');
                executable.push_str(argument.trim());
            }
            Some(executable)
        }
        "lint" => Some(format!("cargo fmt{manifest_arg} --check")),
        "build" => {
            let mut executable = format!("cargo build{manifest_arg}");
            if let Some(argument) = argument.filter(|value| !value.trim().is_empty()) {
                executable.push(' ');
                if argument.trim() == "release" {
                    executable.push_str("--release");
                } else {
                    executable.push_str(argument.trim());
                }
            }
            Some(executable)
        }
        _ => None,
    }
}

fn package_script_exists(cwd: &Path, script: &str) -> bool {
    let package_json = cwd.join("package.json");
    let Ok(raw) = fs::read_to_string(package_json) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return false;
    };
    value
        .get("scripts")
        .and_then(Value::as_object)
        .and_then(|scripts| scripts.get(script))
        .and_then(Value::as_str)
        .is_some_and(|script| !script.trim().is_empty())
}

fn review_local_command_value(scope: Option<String>) -> Result<Value, Box<dyn std::error::Error>> {
    let context = status_context(None)?;
    let diff_stat = git_diff_stat().unwrap_or_else(|| "git diff is unavailable".to_string());
    let status = if context.git_summary.changed_files == 0 {
        "clean"
    } else {
        "needs_review"
    };
    Ok(json!({
        "type": "local_command",
        "command": "review",
        "status": status,
        "cwd": context.cwd,
        "request": { "scope": scope },
        "workspace": {
            "project_root": context.project_root,
            "git_branch": context.git_branch,
            "git_state": context.git_summary.headline(),
            "changed_files": context.git_summary.changed_files,
            "staged_files": context.git_summary.staged_files,
            "unstaged_files": context.git_summary.unstaged_files,
            "untracked_files": context.git_summary.untracked_files,
        },
        "diff_stat": diff_stat,
        "summary": if context.git_summary.changed_files == 0 {
            "workspace is clean; no local changes to review".to_string()
        } else {
            "review local changes with /diff before committing".to_string()
        },
    }))
}

fn diagnostics_local_command_value(
    path: Option<String>,
) -> Result<Value, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let registry = runtime::lsp_client::LspRegistry::new();
    let diagnostics = registry.dispatch("diagnostics", path.as_deref(), None, None, None)?;
    Ok(json!({
        "type": "local_command",
        "command": "diagnostics",
        "status": "ok",
        "cwd": cwd,
        "request": { "path": path },
        "diagnostics": diagnostics,
        "summary": "no cached LSP diagnostics are available in this CLI process",
    }))
}

fn workspace_local_command_value(
    path: Option<PathBuf>,
) -> Result<Value, Box<dyn std::error::Error>> {
    let requested_path = path.clone();
    if let Some(path) = path.as_ref() {
        env::set_current_dir(path)?;
    }
    let context = status_context(None)?;
    Ok(json!({
        "type": "local_command",
        "command": "workspace",
        "status": "ok",
        "cwd": context.cwd,
        "request": { "path": requested_path },
        "workspace": {
            "project_root": context.project_root,
            "git_branch": context.git_branch,
            "git_state": context.git_summary.headline(),
            "changed_files": context.git_summary.changed_files,
            "staged_files": context.git_summary.staged_files,
            "unstaged_files": context.git_summary.unstaged_files,
            "untracked_files": context.git_summary.untracked_files,
            "loaded_config_files": context.loaded_config_files,
            "discovered_config_files": context.discovered_config_files,
            "memory_file_count": context.memory_file_count,
        },
        "summary": "workspace context loaded",
    }))
}

fn git_diff_stat() -> Option<String> {
    let output = Command::new("git")
        .args(["diff", "--stat", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Some(if stdout.is_empty() {
        "no diff against HEAD".to_string()
    } else {
        stdout
    })
}

fn render_local_command_text(value: &Value) -> String {
    let command = value["command"].as_str().unwrap_or("local");
    let status = value["status"].as_str().unwrap_or("unknown");
    let summary = value["summary"].as_str().unwrap_or("");
    let cwd = value["cwd"].as_str().unwrap_or("");
    let mut lines = vec![format!(
        "Local command\n  Command          /{command}\n  Status           {status}\n  Cwd              {cwd}\n  Summary          {summary}"
    )];
    if let Some(execution) = value.get("execution").filter(|value| value.is_object()) {
        if let Some(command_line) = execution.get("command").and_then(Value::as_str) {
            lines.push(format!("  Executed         {command_line}"));
        }
        if let Some(detail) = execution.get("summary").and_then(Value::as_str) {
            lines.push(format!("  Detail           {detail}"));
        }
    }
    if let Some(workspace) = value.get("workspace").filter(|value| value.is_object()) {
        if let Some(git_state) = workspace.get("git_state").and_then(Value::as_str) {
            lines.push(format!("  Git state        {git_state}"));
        }
    }
    if let Some(diff_stat) = value.get("diff_stat").and_then(Value::as_str) {
        lines.push("  Diff stat".to_string());
        lines.extend(diff_stat.lines().map(|line| format!("    {line}")));
    }
    lines.join("\n")
}

fn print_worker_output(
    value: Value,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    match output_format {
        CliOutputFormat::StreamJson => print_stream_json_event(value),
        CliOutputFormat::Json => print_task_json(value)?,
        CliOutputFormat::Text => print_task_json(value)?,
    }
    Ok(())
}

fn print_route_output(
    value: Value,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    match output_format {
        CliOutputFormat::StreamJson => print_stream_json_event(value),
        CliOutputFormat::Json => print_task_json(value)?,
        CliOutputFormat::Text => println!("{}", render_route_output_text(&value)),
    }
    Ok(())
}

fn render_route_output_text(value: &Value) -> String {
    match value["type"].as_str() {
        Some("autonomous_preflight_blocked") => render_autonomous_preflight_blocked_text(value),
        Some("route_optimizer_report") => render_route_optimizer_text(value),
        Some("route_optimizer_replay") => render_route_optimizer_replay_text(value),
        Some("route_policy_proposal") => render_route_policy_proposal_text(value),
        Some("route_policy_apply") => render_route_policy_apply_text(value),
        Some("route_policy_rollback") => render_route_policy_rollback_text(value),
        Some("route_policy_list") => render_route_policy_list_text(value),
        _ => render_route_feedback_summary_text(value),
    }
}

fn render_route_feedback_summary_text(value: &Value) -> String {
    let feedback_count = value["feedback_count"].as_u64().unwrap_or(0);
    let summaries = value["summaries"].as_array().cloned().unwrap_or_default();
    if summaries.is_empty() {
        return format!(
            "Route feedback summary
  Feedback entries  {feedback_count}
  Routes            none"
        );
    }
    let mut lines = vec![format!(
        "Route feedback summary
  Feedback entries  {feedback_count}
  Routes            {}

  Phase        Provider      Model       Total  Success  Fail  Recovery  Latency  Tokens  Cost",
        summaries.len()
    )];
    for summary in summaries {
        let phase = summary["phase"].as_str().unwrap_or("unknown");
        let provider = summary["provider"].as_str().unwrap_or("default");
        let model = summary["model"].as_str().unwrap_or("unknown");
        let total = summary["total"].as_u64().unwrap_or(0);
        let failures = summary["failures"].as_u64().unwrap_or(0);
        let recovery = summary["recovery_triggered"].as_u64().unwrap_or(0);
        let success_rate = summary["success_rate"].as_f64().unwrap_or(0.0) * 100.0;
        let latency = summary["avg_latency_ms"]
            .as_f64()
            .map_or("n/a".to_string(), |value| format!("{value:.0}ms"));
        let tokens = summary["avg_tokens"]
            .as_f64()
            .map_or("n/a".to_string(), |value| format!("{value:.0}"));
        let cost = summary["avg_cost_usd"]
            .as_f64()
            .map_or("n/a".to_string(), |value| format!("${value:.4}"));
        let risk = if failures > 0 || recovery > 0 {
            "!"
        } else {
            " "
        };
        lines.push(format!(
            "  {risk} {phase:<11} {provider:<12} {model:<10} {total:>5}  {success_rate:>6.0}%  {failures:>4}  {recovery:>8}  {latency:>7}  {tokens:>6}  {cost:>8}"
        ));
    }
    lines.join(
        "
",
    )
}

fn render_route_optimizer_text(value: &Value) -> String {
    let report = &value["report"];
    let feedback_count = report["feedback_count"].as_u64().unwrap_or(0);
    let candidates = report["candidates"].as_array().cloned().unwrap_or_default();
    let mut lines = vec![format!(
        "Route optimizer\n  Feedback entries  {feedback_count}\n  Candidates        {}",
        candidates.len()
    )];
    if let Some(health) = report["health"].as_array() {
        lines.push("Health:".to_string());
        for entry in health {
            let phase = entry["phase"].as_str().unwrap_or("unknown");
            let model = entry["model"].as_str().unwrap_or("unknown");
            let health = entry["health"].as_str().unwrap_or("unknown");
            let success = entry["success_rate"].as_f64().unwrap_or(0.0) * 100.0;
            lines.push(format!(
                "  - {phase}/{model}: {health} ({success:.0}% success)"
            ));
        }
    }
    if !candidates.is_empty() {
        lines.push("Candidates:".to_string());
        for candidate in candidates {
            let phase = candidate["phase"].as_str().unwrap_or("unknown");
            let model = candidate["model"].as_str().unwrap_or("unknown");
            let kind = candidate["kind"].as_str().unwrap_or("observe_more");
            let fallback = candidate["fallback_model"].as_str().unwrap_or("none");
            lines.push(format!("  - {phase}/{model}: {kind}, fallback={fallback}"));
        }
    }
    if let Some(recommendations) = report["recommendations"].as_array() {
        lines.push("Recommendations:".to_string());
        for recommendation in recommendations.iter().filter_map(Value::as_str) {
            lines.push(format!("  - {recommendation}"));
        }
    }
    lines.join("\n")
}

fn render_route_optimizer_replay_text(value: &Value) -> String {
    let replay = &value["replay"];
    let feedback_count = replay["feedback_count"].as_u64().unwrap_or(0);
    let changed_routes = replay["changed_routes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let success_delta = replay["estimated_success_delta"]
        .as_f64()
        .unwrap_or_default()
        * 100.0;
    let mut lines = vec![format!(
        "Route optimizer replay\n  Feedback entries  {feedback_count}\n  Changed routes    {}\n  Est. success delta {success_delta:.0}%",
        changed_routes.len()
    )];
    if !changed_routes.is_empty() {
        lines.push("Changes:".to_string());
        for change in changed_routes {
            let phase = change["phase"].as_str().unwrap_or("unknown");
            let current = change["current_model"].as_str().unwrap_or("unknown");
            let candidate = change["candidate_model"].as_str().unwrap_or("unknown");
            lines.push(format!("  - {phase}: {current} -> {candidate}"));
        }
    }
    if let Some(recommendations) = replay["recommendations"].as_array() {
        lines.push("Recommendations:".to_string());
        for recommendation in recommendations.iter().filter_map(Value::as_str) {
            lines.push(format!("  - {recommendation}"));
        }
    }
    lines.join("\n")
}

fn render_route_policy_proposal_text(value: &Value) -> String {
    let proposal = &value["proposal"];
    let id = proposal["id"].as_str().unwrap_or("unknown");
    let status = proposal["status"].as_str().unwrap_or("unknown");
    let changes = proposal["changes"].as_array().cloned().unwrap_or_default();
    let blockers = proposal["gates"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|gate| {
            gate["passed"].as_bool() == Some(false) && gate["level"].as_str() == Some("blocker")
        })
        .count();
    let mut lines = vec![format!(
        "Route policy proposal\n  ID          {id}\n  Status      {status}\n  Changes     {}\n  Blockers    {blockers}",
        changes.len()
    )];
    for change in changes {
        let phase = change["phase"].as_str().unwrap_or("unknown");
        let current = change["current_model"].as_str().unwrap_or("unknown");
        let proposed = change["proposed_model"].as_str().unwrap_or("unknown");
        lines.push(format!("  - {phase}: {current} -> {proposed}"));
    }
    lines.join("\n")
}

fn render_route_policy_apply_text(value: &Value) -> String {
    let apply = &value["apply"];
    let proposal_id = apply["proposal_id"].as_str().unwrap_or("unknown");
    let dry_run = apply["dry_run"].as_bool().unwrap_or(false);
    let applied = apply["applied"].as_bool().unwrap_or(false);
    let blockers = apply["blockers"].as_array().cloned().unwrap_or_default();
    let mut lines = vec![format!(
        "Route policy apply\n  Proposal    {proposal_id}\n  Dry run     {dry_run}\n  Applied     {applied}\n  Blockers    {}",
        blockers.len()
    )];
    for blocker in blockers.iter().filter_map(Value::as_str) {
        lines.push(format!("  - {blocker}"));
    }
    lines.join("\n")
}

fn render_route_policy_rollback_text(value: &Value) -> String {
    let rollback = &value["rollback"];
    let proposal_id = rollback["proposal_id"].as_str().unwrap_or("unknown");
    let rolled_back = rollback["rolled_back"].as_bool().unwrap_or(false);
    format!("Route policy rollback\n  Proposal    {proposal_id}\n  Rolled back {rolled_back}")
}

fn render_route_policy_list_text(value: &Value) -> String {
    let proposals = value["proposals"].as_array().cloned().unwrap_or_default();
    let mut lines = vec![format!(
        "Route policy proposals\n  Proposals  {}",
        proposals.len()
    )];
    for proposal in proposals {
        let id = proposal["id"].as_str().unwrap_or("unknown");
        let status = proposal["status"].as_str().unwrap_or("unknown");
        let changes = proposal["changes"].as_array().map_or(0, Vec::len);
        lines.push(format!("  - {id}: {status}, changes={changes}"));
    }
    lines.join("\n")
}

fn run_route_command(
    command: RouteCliCommand,
    output_format: CliOutputFormat,
    model: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        RouteCliCommand::FeedbackSummary => {
            let store = load_route_feedback_store()?;
            print_route_output(
                json!({
                    "type": "route_feedback_summary",
                    "feedback_count": store.feedback().len(),
                    "summaries": store.summaries(),
                    "feedback_path": route_feedback_dir()?.join("feedback.json"),
                }),
                output_format,
            )?;
        }
        RouteCliCommand::List => {
            let store = load_route_policy_proposal_store()?;
            let proposals = store.list()?;
            print_route_output(
                json!({
                    "type": "route_policy_list",
                    "proposals": proposals,
                    "proposals_path": store.proposals_path(),
                    "applied_policy_path": store.applied_policy_path(),
                }),
                output_format,
            )?;
        }
        RouteCliCommand::Optimize {
            min_samples,
            threshold_percent,
        } => {
            let store = load_route_feedback_store()?;
            let report = runtime::evaluate_routing_feedback(
                &store,
                min_samples,
                f32::from(threshold_percent) / 100.0,
            );
            print_route_output(
                json!({
                    "type": "route_optimizer_report",
                    "report": report,
                    "feedback_path": route_feedback_dir()?.join("feedback.json"),
                }),
                output_format,
            )?;
        }
        RouteCliCommand::Replay {
            min_samples,
            threshold_percent,
        } => {
            let store = load_route_feedback_store()?;
            let policy = load_effective_model_routing_policy(model)?;
            let replay = runtime::replay_routing_optimizer(
                policy,
                &store,
                min_samples,
                f32::from(threshold_percent) / 100.0,
            );
            print_route_output(
                json!({
                    "type": "route_optimizer_replay",
                    "replay": replay,
                    "feedback_path": route_feedback_dir()?.join("feedback.json"),
                }),
                output_format,
            )?;
        }
        RouteCliCommand::Propose {
            min_samples,
            threshold_percent,
        } => {
            let feedback = load_route_feedback_store()?;
            let baseline_policy = load_effective_model_routing_policy(model)?;
            let proposal_store = load_route_policy_proposal_store()?;
            let proposal = proposal_store.propose(
                baseline_policy,
                &feedback,
                min_samples,
                f32::from(threshold_percent) / 100.0,
            )?;
            print_route_output(
                json!({
                    "type": "route_policy_proposal",
                    "proposal": proposal,
                    "proposals_path": proposal_store.proposals_path(),
                    "applied_policy_path": proposal_store.applied_policy_path(),
                }),
                output_format,
            )?;
        }
        RouteCliCommand::Apply {
            proposal_id,
            dry_run,
        } => {
            let proposal_store = load_route_policy_proposal_store()?;
            let apply = proposal_store.apply(&proposal_id, dry_run)?;
            print_route_output(
                json!({
                    "type": "route_policy_apply",
                    "apply": apply,
                }),
                output_format,
            )?;
        }
        RouteCliCommand::Rollback { proposal_id } => {
            let proposal_store = load_route_policy_proposal_store()?;
            let rollback = proposal_store.rollback(&proposal_id)?;
            print_route_output(
                json!({
                    "type": "route_policy_rollback",
                    "rollback": rollback,
                }),
                output_format,
            )?;
        }
    }
    Ok(())
}

fn run_policy_command(
    command: PolicyCliCommand,
    output_format: CliOutputFormat,
    permission_mode: PermissionMode,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        PolicyCliCommand::Review {
            limit,
            max_ticks,
            record,
        } => {
            let input = build_policy_governance_input(limit, max_ticks, permission_mode)?;
            let ledger = runtime::PolicyGovernanceLedger::new(policy_governance_dir()?);
            let review = runtime::review_policy_governance(input);
            let review = if record {
                ledger.record_review(review)?
            } else {
                review
            };
            print_policy_output(
                json!({
                    "type": "policy_review",
                    "review": review,
                    "recorded": record,
                    "ledger_path": ledger.ledger_path(),
                }),
                output_format,
            )?;
        }
        PolicyCliCommand::Ledger { limit } => {
            let ledger = runtime::PolicyGovernanceLedger::new(policy_governance_dir()?);
            let load = ledger.load(limit)?;
            print_policy_output(
                json!({
                    "type": "policy_ledger",
                    "ledger": load,
                    "ledger_path": ledger.ledger_path(),
                }),
                output_format,
            )?;
        }
        PolicyCliCommand::Replay { limit } => {
            let policy_dir = policy_governance_dir()?;
            let replay = runtime::replay_policy_lifecycle(&policy_dir, limit)?;
            print_policy_output(
                json!({
                    "type": "policy_replay",
                    "replay": replay,
                    "ledger_path": runtime::policy_governance_ledger_path(&policy_dir),
                }),
                output_format,
            )?;
        }
        PolicyCliCommand::Plan { limit, max_ticks } => {
            let input = build_policy_governance_input(limit, max_ticks, permission_mode)?;
            let scheduler_state = input.scheduler_state.clone();
            let autonomous_evaluation = input.autonomous_evaluation.clone();
            let review = runtime::review_policy_governance(input);
            let coordinator =
                runtime::PolicyApplyCoordinator::new(load_route_policy_proposal_store()?)
                    .with_scheduler_state(scheduler_state)
                    .with_autonomous_evaluation(autonomous_evaluation);
            let plan = coordinator.plan_apply(review, None, None, false);
            let ledger = runtime::PolicyGovernanceLedger::new(policy_governance_dir()?);
            let ledger_entry = ledger.record_apply_plan(&plan)?;
            print_policy_output(
                json!({
                    "type": "policy_apply_plan",
                    "plan": plan,
                    "recorded": true,
                    "ledger_entry": ledger_entry,
                    "ledger_path": ledger.ledger_path(),
                }),
                output_format,
            )?;
        }
        PolicyCliCommand::Apply {
            limit,
            max_ticks,
            domain,
            proposal_id,
            dry_run,
        } => {
            if !dry_run {
                let integration_review =
                    build_autonomous_integration_review(limit, max_ticks, permission_mode)?;
                if !integration_review.health.safe_to_apply_policy {
                    print_policy_output(
                        autonomous_preflight_blocked_value("policy apply", &integration_review),
                        output_format,
                    )?;
                    return Ok(());
                }
            }
            let input = build_policy_governance_input(limit, max_ticks, permission_mode)?;
            let scheduler_state = input.scheduler_state.clone();
            let autonomous_evaluation = input.autonomous_evaluation.clone();
            let review = runtime::review_policy_governance(input);
            let coordinator =
                runtime::PolicyApplyCoordinator::new(load_route_policy_proposal_store()?)
                    .with_scheduler_state(scheduler_state)
                    .with_autonomous_evaluation(autonomous_evaluation);
            let plan = coordinator.plan_apply(review, domain, proposal_id.as_deref(), dry_run);
            let apply = coordinator.apply(&plan)?;
            let ledger = runtime::PolicyGovernanceLedger::new(policy_governance_dir()?);
            let ledger_entry = ledger.record_apply_report(&apply)?;
            print_policy_output(
                json!({
                    "type": "policy_apply",
                    "apply": apply,
                    "recorded": true,
                    "ledger_entry": ledger_entry,
                    "ledger_path": ledger.ledger_path(),
                }),
                output_format,
            )?;
        }
        PolicyCliCommand::Rollback {
            limit,
            max_ticks,
            domain,
            proposal_id,
        } => {
            let input = build_policy_governance_input(limit, max_ticks, permission_mode)?;
            let scheduler_state = input.scheduler_state.clone();
            let autonomous_evaluation = input.autonomous_evaluation.clone();
            let review = runtime::review_policy_governance(input);
            let coordinator =
                runtime::PolicyApplyCoordinator::new(load_route_policy_proposal_store()?)
                    .with_scheduler_state(scheduler_state)
                    .with_autonomous_evaluation(autonomous_evaluation);
            let plan = coordinator.plan_rollback(review, domain, proposal_id.as_deref());
            let rollback = coordinator.rollback(&plan)?;
            let ledger = runtime::PolicyGovernanceLedger::new(policy_governance_dir()?);
            let ledger_entry = ledger.record_rollback_report(&rollback)?;
            print_policy_output(
                json!({
                    "type": "policy_rollback",
                    "rollback": rollback,
                    "recorded": true,
                    "ledger_entry": ledger_entry,
                    "ledger_path": ledger.ledger_path(),
                }),
                output_format,
            )?;
        }
    }
    Ok(())
}

fn build_policy_governance_input(
    limit: usize,
    max_ticks: usize,
    permission_mode: PermissionMode,
) -> Result<runtime::PolicyGovernanceInput, Box<dyn std::error::Error>> {
    let autonomous_evaluation =
        match build_autonomous_evaluation_input(limit, max_ticks, permission_mode) {
            Ok(input) => Some(runtime::evaluate_autonomous_loop(input)),
            Err(_) => None,
        };
    let routing_proposals = runtime::load_routing_policy_proposals(&route_feedback_dir()?)
        .map(|snapshot| snapshot.proposals)
        .unwrap_or_default();
    let applied_routing_policy = runtime::load_applied_routing_policy(&route_feedback_dir()?)?;
    let scheduler_state = runtime::SchedulerDaemon::new(
        runtime::DurableTaskScheduler::new(
            load_task_registry()?,
            runtime::VerificationRunner::new(Some(env::current_dir()?)),
        )
        .with_permission_mode(permission_mode),
        scheduler_state_dir()?,
    )
    .load_state()
    .ok();
    Ok(runtime::PolicyGovernanceInput {
        autonomous_evaluation,
        routing_proposals,
        applied_routing_policy,
        scheduler_state,
    })
}

fn print_policy_output(
    value: Value,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    match output_format {
        CliOutputFormat::StreamJson => print_stream_json_event(value),
        CliOutputFormat::Json => print_task_json(value)?,
        CliOutputFormat::Text => println!("{}", render_policy_output_text(&value)),
    }
    Ok(())
}

fn render_policy_output_text(value: &Value) -> String {
    match value["type"].as_str() {
        Some("autonomous_preflight_blocked") => render_autonomous_preflight_blocked_text(value),
        Some("policy_ledger") => render_policy_ledger_text(value),
        Some("policy_replay") => render_policy_replay_text(value),
        Some("policy_apply_plan") => render_policy_apply_plan_text(value),
        Some("policy_apply") => render_governed_policy_apply_text(value),
        Some("policy_rollback") => render_governed_policy_rollback_text(value),
        _ => render_policy_review_text(value),
    }
}

fn render_policy_review_text(value: &Value) -> String {
    let entry = &value["review"]["ledger_entry"];
    let summary = &entry["summary"];
    let status = entry["status"].as_str().unwrap_or("unknown");
    let proposals = summary["proposal_count"].as_u64().unwrap_or(0);
    let conflicts = summary["conflict_count"].as_u64().unwrap_or(0);
    let failed_gates = summary["failed_gate_count"].as_u64().unwrap_or(0);
    let recorded = value["recorded"].as_bool().unwrap_or(false);
    let mut lines = vec![format!(
        "Policy review\n  Status      {status}\n  Proposals   {proposals}\n  Conflicts   {conflicts}\n  Failed gates {failed_gates}\n  Recorded    {recorded}"
    )];
    if let Some(recommendations) = entry["recommendations"].as_array() {
        lines.push("Recommendations:".to_string());
        for recommendation in recommendations.iter().filter_map(Value::as_str) {
            lines.push(format!("  - {recommendation}"));
        }
    }
    lines.join("\n")
}

fn render_policy_ledger_text(value: &Value) -> String {
    let ledger = &value["ledger"];
    let entries = ledger["entries"].as_array().cloned().unwrap_or_default();
    let malformed = ledger["malformed_lines"].as_u64().unwrap_or(0);
    let mut lines = vec![format!(
        "Policy ledger\n  Entries   {}\n  Malformed {malformed}",
        entries.len()
    )];
    for entry in entries {
        let id = entry["id"].as_str().unwrap_or("unknown");
        let status = entry["status"].as_str().unwrap_or("unknown");
        let conflicts = entry["summary"]["conflict_count"].as_u64().unwrap_or(0);
        lines.push(format!("  - {id}: {status}, conflicts={conflicts}"));
    }
    lines.join("\n")
}

fn render_policy_replay_text(value: &Value) -> String {
    let replay = &value["replay"];
    let summary = &replay["summary"];
    let lifecycles = summary["lifecycle_count"].as_u64().unwrap_or(0);
    let events = summary["event_count"].as_u64().unwrap_or(0);
    let anomalies = summary["anomaly_count"].as_u64().unwrap_or(0);
    let malformed = summary["malformed_lines"].as_u64().unwrap_or(0);
    let mut lines = vec![format!(
        "Policy replay\n  Lifecycles {lifecycles}\n  Events     {events}\n  Anomalies  {anomalies}\n  Malformed  {malformed}"
    )];
    if let Some(domains) = render_json_count_map(&summary["domain_counts"], 5) {
        lines.push(format!("  Domains   {domains}"));
    }
    if let Some(actions) = render_json_count_map(&summary["action_counts"], 5) {
        lines.push(format!("  Actions   {actions}"));
    }
    if let Some(anomaly_kinds) = render_json_count_map(&summary["anomaly_kind_counts"], 5) {
        lines.push(format!("  Anomaly kinds {anomaly_kinds}"));
    }
    for lifecycle in replay["lifecycles"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .take(10)
    {
        let domain = lifecycle["domain"].as_str().unwrap_or("unknown");
        let proposal = lifecycle["proposal_id"].as_str().unwrap_or("unknown");
        let status = lifecycle["current_status"].as_str().unwrap_or("unknown");
        let count = lifecycle["events"].as_array().map_or(0, Vec::len);
        lines.push(format!("  - {domain}/{proposal}: {status}, events={count}"));
    }
    lines.join("\n")
}

fn render_policy_apply_plan_text(value: &Value) -> String {
    let plan = &value["plan"];
    let status = plan["status"].as_str().unwrap_or("unknown");
    let actions = plan["actions"].as_array().map_or(0, Vec::len);
    let adapters = plan["adapters"].as_array().map_or(0, Vec::len);
    let blockers = plan["blockers"].as_array().map_or(0, Vec::len);
    let recorded = value["recorded"].as_bool().unwrap_or(false);
    let mut lines = vec![format!(
        "Policy apply plan\n  Status   {status}\n  Actions  {actions}\n  Adapters {adapters}\n  Blockers {blockers}\n  Recorded {recorded}"
    )];
    if let Some(adapters) = plan["adapters"].as_array() {
        lines.push("Adapters:".to_string());
        for adapter in adapters.iter().take(6) {
            let domain = adapter["domain"].as_str().unwrap_or("unknown");
            let name = adapter["name"].as_str().unwrap_or("unknown");
            let persistent = adapter["supports_persistent_apply"]
                .as_bool()
                .unwrap_or(false);
            let dry_run = adapter["supports_dry_run"].as_bool().unwrap_or(false);
            let rollback = adapter["supports_rollback"].as_bool().unwrap_or(false);
            let planned_only = adapter["planned_only"].as_bool().unwrap_or(false);
            lines.push(format!(
                "  - {domain}/{name}: persistent={persistent}, dry_run={dry_run}, rollback={rollback}, planned_only={planned_only}"
            ));
        }
    }
    if let Some(actions) = plan["actions"].as_array() {
        lines.push("Actions:".to_string());
        for action in actions {
            let domain = action["domain"].as_str().unwrap_or("unknown");
            let proposal = action["proposal_id"].as_str().unwrap_or("none");
            let status = action["status"].as_str().unwrap_or("unknown");
            let executable = action["executable"].as_bool().unwrap_or(false);
            lines.push(format!(
                "  - {domain}/{proposal}: {status}, executable={executable}"
            ));
        }
    }
    lines.join("\n")
}

fn render_governed_policy_apply_text(value: &Value) -> String {
    let apply = &value["apply"];
    let status = apply["status"].as_str().unwrap_or("unknown");
    let dry_run = apply["dry_run"].as_bool().unwrap_or(false);
    let applied = apply["applied"].as_bool().unwrap_or(false);
    let blockers = apply["blockers"].as_array().map_or(0, Vec::len);
    let receipt = apply["receipt"]["id"].as_str().unwrap_or("unknown");
    let adapter = apply["receipt"]["adapter"].as_str().unwrap_or("none");
    let adapter_report_kind = apply["adapter_report"]["kind"].as_str().unwrap_or("none");
    let mut lines = vec![format!(
        "Policy apply\n  Status   {status}\n  Dry run  {dry_run}\n  Applied  {applied}\n  Blockers {blockers}"
    )];
    lines.push(format!("  Receipt  {receipt}"));
    lines.push(format!("  Adapter  {adapter}"));
    lines.push(format!("  Adapter report {adapter_report_kind}"));
    for blocker in apply["structured_blockers"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .take(3)
    {
        let kind = blocker["kind"].as_str().unwrap_or("unknown");
        let reason = blocker["reason"].as_str().unwrap_or("unknown");
        lines.push(format!("  - {kind}: {reason}"));
    }
    lines.join("\n")
}

fn render_governed_policy_rollback_text(value: &Value) -> String {
    let rollback = &value["rollback"];
    let status = rollback["status"].as_str().unwrap_or("unknown");
    let rolled_back = rollback["rolled_back"].as_bool().unwrap_or(false);
    let blockers = rollback["blockers"].as_array().map_or(0, Vec::len);
    let receipt = rollback["receipt"]["id"].as_str().unwrap_or("unknown");
    let adapter = rollback["receipt"]["adapter"].as_str().unwrap_or("none");
    let adapter_report_kind = rollback["adapter_report"]["kind"]
        .as_str()
        .unwrap_or("none");
    let mut lines = vec![format!(
        "Policy rollback\n  Status      {status}\n  Rolled back {rolled_back}\n  Blockers    {blockers}"
    )];
    lines.push(format!("  Receipt     {receipt}"));
    lines.push(format!("  Adapter     {adapter}"));
    lines.push(format!("  Adapter report {adapter_report_kind}"));
    for blocker in rollback["structured_blockers"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .take(3)
    {
        let kind = blocker["kind"].as_str().unwrap_or("unknown");
        let reason = blocker["reason"].as_str().unwrap_or("unknown");
        lines.push(format!("  - {kind}: {reason}"));
    }
    lines.join("\n")
}

fn render_json_count_map(value: &Value, limit: usize) -> Option<String> {
    let object = value.as_object()?;
    if object.is_empty() {
        return None;
    }
    let mut counts = object
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_u64().unwrap_or(0)))
        .collect::<Vec<_>>();
    counts.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    Some(
        counts
            .into_iter()
            .take(limit)
            .map(|(key, count)| format!("{key}={count}"))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn run_worker_command(
    command: WorkerCliCommand,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        WorkerCliCommand::List => {
            let registry = load_worker_registry()?;
            let workers = registry.list();
            match output_format {
                CliOutputFormat::Text => {
                    if workers.is_empty() {
                        println!("No workers.");
                    } else {
                        for worker in workers {
                            println!("{}\t{}\t{}", worker.worker_id, worker.status, worker.cwd);
                        }
                    }
                }
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_worker_output(
                        json!({"type":"worker_list","workers":workers}),
                        output_format,
                    )?;
                }
            }
        }
        WorkerCliCommand::Create { cwd, trusted_roots } => {
            let registry = load_worker_registry()?;
            let cwd = cwd.unwrap_or(env::current_dir()?);
            let worker = registry.create(&cwd.to_string_lossy(), &trusted_roots, true);
            save_worker_registry(&registry)?;
            match output_format {
                CliOutputFormat::Text => println!("created worker {}", worker.worker_id),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_worker_output(
                        json!({"type":"worker_create","worker":worker}),
                        output_format,
                    )?;
                }
            }
        }
        WorkerCliCommand::Spawn {
            cwd,
            trusted_roots,
            isolate_worktree,
            worktree_root,
            command,
        } => {
            let registry = load_worker_registry()?;
            let cwd = cwd.unwrap_or(env::current_dir()?);
            let mut spec = runtime::WorkerProcessSpec::new(command, cwd.clone())
                .with_trusted_roots(trusted_roots);
            if isolate_worktree {
                let root = worktree_root
                    .unwrap_or_else(|| cwd.join(".Himalaya").join("workers").join("worktrees"));
                spec = spec.with_isolation(runtime::WorkerIsolationSpec::GitWorktree { root });
            }
            let handle = registry.spawn_process(spec)?;
            let worker = handle.worker;
            save_worker_registry(&registry)?;
            match output_format {
                CliOutputFormat::Text => println!("spawned worker {}", worker.worker_id),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_worker_output(
                        json!({"type":"worker_spawn","worker":worker}),
                        output_format,
                    )?;
                }
            }
        }
        WorkerCliCommand::Probe { worker_id } => {
            let registry = load_worker_registry()?;
            let worker = registry.probe_process(&worker_id)?;
            save_worker_registry(&registry)?;
            match output_format {
                CliOutputFormat::Text => println!("{}", worker.status),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_worker_output(
                        json!({"type":"worker_probe","worker":worker}),
                        output_format,
                    )?;
                }
            }
        }
        WorkerCliCommand::Observe {
            worker_id,
            screen_text,
        } => {
            let registry = load_worker_registry()?;
            let worker = registry.observe(&worker_id, &screen_text)?;
            save_worker_registry(&registry)?;
            match output_format {
                CliOutputFormat::Text => println!("{}", worker.status),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_worker_output(
                        json!({"type":"worker_observe","worker":worker}),
                        output_format,
                    )?;
                }
            }
        }
        WorkerCliCommand::Ready { worker_id } => {
            let registry = load_worker_registry()?;
            let ready = registry.await_ready(&worker_id)?;
            match output_format {
                CliOutputFormat::Text => println!("{}", ready.status),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_worker_output(
                        json!({"type":"worker_ready","ready":ready}),
                        output_format,
                    )?;
                }
            }
        }
        WorkerCliCommand::ResolveTrust { worker_id } => {
            let registry = load_worker_registry()?;
            let worker = registry.resolve_trust(&worker_id)?;
            save_worker_registry(&registry)?;
            match output_format {
                CliOutputFormat::Text => println!("{}", worker.status),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_worker_output(
                        json!({"type":"worker_resolve_trust","worker":worker}),
                        output_format,
                    )?;
                }
            }
        }
        WorkerCliCommand::Prompt { worker_id, prompt } => {
            let registry = load_worker_registry()?;
            let worker = registry.send_prompt(&worker_id, prompt.as_deref())?;
            save_worker_registry(&registry)?;
            match output_format {
                CliOutputFormat::Text => println!("{}", worker.status),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_worker_output(
                        json!({"type":"worker_prompt","worker":worker}),
                        output_format,
                    )?;
                }
            }
        }
        WorkerCliCommand::Complete {
            worker_id,
            finish_reason,
            tokens_output,
        } => {
            let registry = load_worker_registry()?;
            let worker = registry.observe_completion(&worker_id, &finish_reason, tokens_output)?;
            save_worker_registry(&registry)?;
            match output_format {
                CliOutputFormat::Text => println!("{}", worker.status),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_worker_output(
                        json!({"type":"worker_complete","worker":worker}),
                        output_format,
                    )?;
                }
            }
        }
        WorkerCliCommand::Restart { worker_id } => {
            let registry = load_worker_registry()?;
            let worker = registry.restart(&worker_id)?;
            save_worker_registry(&registry)?;
            match output_format {
                CliOutputFormat::Text => println!("{}", worker.status),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_worker_output(
                        json!({"type":"worker_restart","worker":worker}),
                        output_format,
                    )?;
                }
            }
        }
        WorkerCliCommand::Terminate { worker_id } => {
            let registry = load_worker_registry()?;
            let worker = registry.terminate(&worker_id)?;
            save_worker_registry(&registry)?;
            match output_format {
                CliOutputFormat::Text => println!("{}", worker.status),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_worker_output(
                        json!({"type":"worker_terminate","worker":worker}),
                        output_format,
                    )?;
                }
            }
        }
        WorkerCliCommand::Cleanup { include_stale } => {
            let registry = load_worker_registry()?;
            let report = if include_stale {
                let now = std::time::SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |duration| duration.as_secs());
                registry.cleanup_stale(now)
            } else {
                registry.cleanup_finished()
            };
            save_worker_registry(&registry)?;
            match output_format {
                CliOutputFormat::Text => println!(
                    "removed {} workers; retained {}; removed {} worktrees",
                    report.removed_workers.len(),
                    report.retained_workers,
                    report.removed_worktrees.len()
                ),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_worker_output(
                        json!({"type":"worker_cleanup","include_stale":include_stale,"report":report}),
                        output_format,
                    )?;
                }
            }
        }
        WorkerCliCommand::Supervise => {
            let workers = load_worker_registry()?;
            let tasks = load_task_registry()?;
            let runner = runtime::VerificationRunner::new(Some(env::current_dir()?));
            let supervisor = runtime::WorkerSupervisor::new(workers.clone(), tasks.clone(), runner);
            let tick = supervisor.tick()?;
            save_worker_registry(&workers)?;
            save_task_registry(&tasks)?;
            match output_format {
                CliOutputFormat::Text => println!("{}", tick.message),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_worker_output(
                        json!({"type":"worker_supervisor_tick","tick":tick}),
                        output_format,
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn task_registry_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    Ok(cwd.join(".Himalaya").join("tasks"))
}

fn scheduler_state_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    Ok(cwd.join(".Himalaya").join("scheduler"))
}

fn build_autonomous_run_coordinator(
    registry: &runtime::TaskRegistry,
    worker_registry: &runtime::WorkerRegistry,
    permission_mode: PermissionMode,
) -> Result<runtime::AutonomousRunCoordinator, Box<dyn std::error::Error>> {
    let runner = runtime::VerificationRunner::new(Some(env::current_dir()?));
    let scheduler = runtime::DurableTaskScheduler::with_workers(
        registry.clone(),
        runner.clone(),
        worker_registry.clone(),
    )
    .with_permission_mode(permission_mode);
    let state_dir = scheduler_state_dir()?;
    let daemon = runtime::SchedulerDaemon::new(scheduler, &state_dir);
    let supervisor =
        runtime::WorkerSupervisor::new(worker_registry.clone(), registry.clone(), runner);
    Ok(runtime::AutonomousRunCoordinator::new(
        daemon,
        supervisor,
        registry.clone(),
        state_dir,
        permission_mode,
    ))
}

fn cron_registry_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    Ok(cwd.join(".Himalaya").join("cron"))
}

fn load_cron_registry(
) -> Result<runtime::team_cron_registry::CronRegistry, Box<dyn std::error::Error>> {
    let dir = cron_registry_dir()?;
    if dir.join("crons.json").exists() {
        return Ok(runtime::team_cron_registry::CronRegistry::load_from_dir(
            &dir,
        )?);
    }
    Ok(runtime::team_cron_registry::CronRegistry::new())
}

fn save_cron_registry(
    registry: &runtime::team_cron_registry::CronRegistry,
) -> Result<(), Box<dyn std::error::Error>> {
    registry.save_to_dir(&cron_registry_dir()?)?;
    Ok(())
}

fn worker_registry_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    Ok(cwd.join(".Himalaya").join("workers"))
}

fn load_worker_registry() -> Result<runtime::WorkerRegistry, Box<dyn std::error::Error>> {
    let dir = worker_registry_dir()?;
    if dir.join("workers.json").exists() {
        return Ok(runtime::WorkerRegistry::load_from_dir(&dir)?);
    }
    Ok(runtime::WorkerRegistry::new())
}

fn save_worker_registry(
    registry: &runtime::WorkerRegistry,
) -> Result<(), Box<dyn std::error::Error>> {
    registry.save_to_dir(&worker_registry_dir()?)?;
    Ok(())
}

fn route_feedback_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    Ok(cwd.join(".Himalaya").join("routes"))
}

fn task_memory_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    Ok(cwd.join(".Himalaya").join("memory"))
}

fn policy_governance_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    Ok(cwd.join(".Himalaya").join("policy"))
}

fn load_route_feedback_store() -> Result<runtime::RouteFeedbackStore, Box<dyn std::error::Error>> {
    Ok(runtime::RouteFeedbackStore::load_from_dir(
        &route_feedback_dir()?,
    )?)
}

fn load_route_policy_proposal_store(
) -> Result<runtime::RoutingPolicyProposalStore, Box<dyn std::error::Error>> {
    Ok(runtime::RoutingPolicyProposalStore::new(
        route_feedback_dir()?,
    ))
}

fn load_effective_model_routing_policy(
    model: &str,
) -> Result<runtime::MoERoutingPolicy, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let runtime_config = ConfigLoader::default_for(&cwd).load()?;
    let configured = runtime_config
        .feature_config()
        .model_routing()
        .to_policy(model);
    let applied = runtime::load_applied_routing_policy(&route_feedback_dir()?)?;
    Ok(applied.map_or(configured, |applied| applied.policy))
}

fn save_route_feedback_store(
    store: &runtime::RouteFeedbackStore,
) -> Result<(), Box<dyn std::error::Error>> {
    store.save_to_dir(&route_feedback_dir()?)?;
    Ok(())
}

fn load_task_memory_store() -> Result<runtime::TaskMemoryStore, Box<dyn std::error::Error>> {
    Ok(runtime::TaskMemoryStore::load_from_dir(&task_memory_dir()?)?)
}

fn save_task_memory_store(
    store: &runtime::TaskMemoryStore,
) -> Result<(), Box<dyn std::error::Error>> {
    store.save_to_dir(&task_memory_dir()?)?;
    Ok(())
}

fn load_task_registry() -> Result<runtime::TaskRegistry, Box<dyn std::error::Error>> {
    let dir = task_registry_dir()?;
    if dir.join("tasks.json").exists() {
        return Ok(runtime::TaskRegistry::load_from_dir(&dir)?);
    }
    Ok(runtime::TaskRegistry::new())
}

fn save_task_registry(registry: &runtime::TaskRegistry) -> Result<(), Box<dyn std::error::Error>> {
    registry.save_to_dir(&task_registry_dir()?)?;
    let tasks = registry.list(None);
    let mut store = load_route_feedback_store()?;
    for task in &tasks {
        for feedback in task.route_feedback.iter().cloned() {
            store.record(feedback);
        }
    }
    save_route_feedback_store(&store)?;
    save_task_memory_store(&runtime::TaskMemoryStore::from_tasks(&tasks))?;
    Ok(())
}

fn read_task_packet(path: &Path) -> Result<runtime::TaskPacket, Box<dyn std::error::Error>> {
    let contents = fs::read_to_string(path)?;
    let packet = serde_json::from_str::<runtime::TaskPacket>(&contents)?;
    Ok(packet)
}

fn persist_task_packet_plan(
    registry: &runtime::TaskRegistry,
    task: &runtime::task_registry::Task,
) -> Result<runtime::task_registry::Task, Box<dyn std::error::Error>> {
    let capabilities = task
        .task_packet
        .as_ref()
        .map(task_packet_capabilities)
        .unwrap_or_else(|| infer_task_capabilities(&task.prompt));
    let mut constraints = vec![
        format!("scope:{}", task.description.as_deref().unwrap_or_default()),
        "source:task_packet".to_string(),
    ];
    if let Some(packet) = task.task_packet.as_ref() {
        constraints.push(format!("repo:{}", packet.repo));
        constraints.push(format!("branch-policy:{}", packet.branch_policy));
        constraints.push(format!("commit-policy:{}", packet.commit_policy));
    }
    let decision_task = runtime::Task::new(
        &task.task_id,
        task.prompt.trim().to_string(),
        estimate_task_complexity(&task.prompt, &capabilities),
        capabilities,
        constraints,
    );
    let available_tools = mvp_tool_specs()
        .into_iter()
        .map(|spec| {
            runtime::tool_from_profile(spec.name, Some(spec.description), Some(&spec.input_schema))
        })
        .collect::<Vec<_>>();
    let reasoning_context = runtime::ReasoningContext {
        workspace_root: env::current_dir().ok(),
        active_constraints: decision_task.constraints.clone(),
        max_parallelism: 4,
        ..runtime::ReasoningContext::default()
    };
    let engine = runtime::DecisioningEngine::new(
        runtime::ToolSelector::new(available_tools, reasoning_context),
        runtime::TaskPlanner::new(4),
        runtime::SafetyPolicy::default(),
    );
    let snapshot = engine.analyze(&decision_task);
    let dag = runtime::build_plan_dag(&decision_task, &snapshot.plan, &snapshot.selected_tools);
    Ok(registry.record_plan(
        &task.task_id,
        dag.clone(),
        runtime::PlanExecution::new(&dag),
    )?)
}

fn task_packet_capabilities(packet: &runtime::TaskPacket) -> Vec<String> {
    let mut text = format!(
        "{} {} {} {}",
        packet.objective, packet.scope, packet.reporting_contract, packet.commit_policy
    );
    if !packet.acceptance_tests.is_empty() {
        text.push_str(" test verify");
        text.push_str(&packet.acceptance_tests.join(" "));
    }
    let mut capabilities = infer_task_capabilities(&text);
    for capability in ["read", "edit", "test", "agent"] {
        if !capabilities.iter().any(|item| item == capability) {
            capabilities.push(capability.to_string());
        }
    }
    capabilities
}

fn task_packet_verification_handoff(task: &runtime::task_registry::Task) -> Value {
    let policy = runtime::infer_verification_policy(task.task_packet.as_ref());
    let request = task
        .task_packet
        .as_ref()
        .map(|packet| runtime::build_verification_request(&task.task_id, packet, policy));
    json!({
        "policy": policy,
        "request": request,
        "agent": {
            "tool": "Agent",
            "subagent_type": "Verification",
            "description": format!("Verify task {}", task.task_id),
            "prompt": format!(
                "Verify task `{}` before it is reported complete.\n\nObjective:\n{}\n",
                task.task_id, task.prompt
            ),
        },
    })
}

fn task_packet_status_value(
    registry: &runtime::TaskRegistry,
    task: runtime::task_registry::Task,
    event_type: &str,
) -> Value {
    let ledger = registry.ledger_for_task(&task.task_id);
    let event_log = registry.event_log_for_task(&task.task_id);
    let verification_handoff = task_packet_verification_handoff(&task);
    json!({
        "type": event_type,
        "task": task,
        "ledger": ledger,
        "event_log": event_log,
        "verification_handoff": verification_handoff,
    })
}

fn task_status_value(
    registry: &runtime::TaskRegistry,
    task: runtime::task_registry::Task,
    event_type: &str,
) -> Value {
    let task_id = task.task_id.clone();
    let policy = runtime::infer_verification_policy(task.task_packet.as_ref());
    let verification_decision = if task_plan_all_succeeded(&task) {
        runtime::evaluate_verification_result(policy, task.verification_result.as_ref())
    } else {
        task.task_packet
            .as_ref()
            .map_or(runtime::VerificationDecision::NotRequired, |packet| {
                runtime::VerificationDecision::Required(runtime::build_verification_request(
                    &task.task_id,
                    packet,
                    policy,
                ))
            })
    };
    let plan_progress = runtime::task_plan_progress(&task);
    let failed_node = task
        .plan
        .as_ref()
        .and_then(|plan| {
            plan.execution
                .nodes
                .values()
                .find(|node| node.status == runtime::PlanNodeStatus::Failed)
        })
        .map(|node| {
            json!({
                "node_id": node.node_id.clone(),
                "failure_class": node.failure_class.clone(),
                "output_summary": node.output_summary.clone(),
                "retry_count": node.retry_count,
            })
        });
    let failure = task_failure_classification(&task, &verification_decision);
    let current_blocker = task_current_blocker(&task, &verification_decision, failed_node.as_ref());
    let latest_recovery = task.recovery_events.last().cloned();
    let latest_recovery_action = task.recovery_action_executions.last().cloned();
    let ledger = registry.ledger_for_task(&task_id);
    let event_log = registry.event_log_for_task(&task_id);
    json!({
        "type": event_type,
        "task": task,
        "plan_progress": plan_progress,
        "verification": {
            "policy": policy,
            "decision": verification_decision,
        },
        "failure": failure,
        "current_blocker": current_blocker,
        "failed_node": failed_node,
        "latest_recovery": latest_recovery,
        "latest_recovery_action": latest_recovery_action,
        "ledger": ledger,
        "event_log": event_log,
    })
}

fn task_report_value(
    registry: &runtime::TaskRegistry,
    task: runtime::task_registry::Task,
) -> Result<Value, Box<dyn std::error::Error>> {
    let task_id = task.task_id.clone();
    let status_snapshot = task_status_value(registry, task.clone(), "task_status");
    let latest_execution_report = task.execution_reports.last().cloned();
    let latest_recovery = task.recovery_events.last().cloned();
    let latest_recovery_action = task.recovery_action_executions.last().cloned();
    let registry_tasks = registry.list(None);
    let memory_store = load_task_memory_store()
        .ok()
        .filter(|store| !store.entries().is_empty())
        .unwrap_or_else(|| runtime::TaskMemoryStore::from_tasks(&registry_tasks));
    let memory_entry = memory_store
        .entry_for_task(&task_id)
        .cloned()
        .unwrap_or_else(|| runtime::TaskMemoryEntry::from_task(&task));
    let memory_context = memory_store.context_for_entry(&memory_entry);
    let similar_memory_count = memory_store
        .similar_entries(&memory_entry.task_type)
        .into_iter()
        .filter(|entry| entry.task_id != task_id)
        .count();
    let local_feedback_count = task.route_feedback.len();
    let mut route_feedback = runtime::RouteFeedbackStore::new();
    for feedback in task.route_feedback.iter().cloned() {
        route_feedback.record(feedback);
    }
    let workspace_store = load_route_feedback_store()?;
    let mut workspace_feedback_count = 0_usize;
    for feedback in workspace_store
        .feedback()
        .iter()
        .filter(|feedback| feedback.task_id == task_id)
        .cloned()
    {
        workspace_feedback_count += 1;
        route_feedback.record(feedback);
    }
    let route_feedback_summary = route_feedback.summaries();
    let latest_failure = latest_execution_report
        .as_ref()
        .and_then(|report| report.failure.clone())
        .or_else(|| {
            serde_json::from_value(status_snapshot["failure"].clone())
                .ok()
                .flatten()
        });
    Ok(json!({
        "type": "task_report",
        "task": task,
        "status_snapshot": status_snapshot,
        "latest_execution_report": latest_execution_report,
        "latest_failure": latest_failure,
        "latest_recovery": latest_recovery,
        "latest_recovery_action": latest_recovery_action,
        "task_memory": {
            "entry": memory_entry,
            "context": memory_context,
            "similar_count": similar_memory_count,
            "summaries": memory_store.summaries(),
            "recovery_actions": memory_store.recovery_action_summaries(),
            "memory_path": task_memory_dir()?.join("tasks.json"),
        },
        "route_feedback": {
            "local_count": local_feedback_count,
            "workspace_count": workspace_feedback_count,
            "combined_count": route_feedback.feedback().len(),
            "summaries": route_feedback_summary,
            "feedback_path": route_feedback_dir()?.join("feedback.json"),
        },
        "ledger": registry.ledger_for_task(&task_id),
        "event_log": registry.event_log_for_task(&task_id),
    }))
}

fn task_review_value(
    registry: &runtime::TaskRegistry,
    task: runtime::task_registry::Task,
) -> Result<Value, Box<dyn std::error::Error>> {
    let report = task_report_value(registry, task)?;
    let recommendations = task_review_recommendations(&report);
    let latest_report = &report["latest_execution_report"];
    let latest_failure = &report["latest_failure"];
    let route_feedback = &report["route_feedback"];
    let recovery_action = &report["latest_recovery_action"];
    let memory_context = report["task_memory"]["context"].clone();
    let route_summary_count = route_feedback["summaries"].as_array().map_or(0, Vec::len);
    let recovery_results = recovery_action["results"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let recovery_executed = recovery_results
        .iter()
        .filter(|result| result["executed"].as_bool() == Some(true))
        .count();
    let recovery_blocked = recovery_results
        .iter()
        .filter(|result| result["blocked"].as_bool() == Some(true))
        .count();
    Ok(json!({
        "type": "task_review",
        "task": report["task"].clone(),
        "report": report.clone(),
        "failure_summary": {
            "class": latest_failure["failure_class"].as_str(),
            "reason": latest_failure["reason"].as_str(),
            "latest_report_message": latest_report["message"].as_str(),
        },
        "recovery_summary": {
            "actions": recovery_results.len(),
            "executed": recovery_executed,
            "blocked": recovery_blocked,
            "effective": latest_report["completed"].as_bool() == Some(true) && recovery_executed > 0,
        },
        "route_summary": {
            "feedback_count": route_feedback["combined_count"].as_u64().unwrap_or(0),
            "summary_count": route_summary_count,
            "routes": route_feedback["summaries"].clone(),
        },
        "memory_summary": report["task_memory"].clone(),
        "planning_context": {
            "task_type": memory_context["task_type"].clone(),
            "similar_count": memory_context["similar_count"].clone(),
            "successful_acceptance_tests": memory_context["successful_acceptance_tests"].clone(),
            "common_failure_classes": memory_context["common_failure_classes"].clone(),
            "recommendations": memory_context["recommendations"].clone(),
        },
        "recovery_policy": {
            "actions": memory_context["recovery_actions"].clone(),
            "route_failure_rate": memory_context["route_failure_rate"].clone(),
        },
        "recommendations": recommendations,
    }))
}

fn task_review_recommendations(report: &Value) -> Vec<String> {
    let mut recommendations = Vec::new();
    let latest_report = &report["latest_execution_report"];
    let completed = latest_report["completed"].as_bool().unwrap_or(false);
    let blocked = latest_report["blocked"].as_bool().unwrap_or(false);
    if latest_report.is_null() {
        recommendations
            .push("Run the task execution loop before reviewing adaptive outcomes.".to_string());
    }
    if let Some(failure_class) = report["latest_failure"]["failure_class"].as_str() {
        recommendations.push(format!(
            "Treat future similar failures as `{failure_class}` and compare recovery outcomes before retrying."
        ));
    }
    if report["latest_recovery_action"]["results"]
        .as_array()
        .is_some_and(|results| {
            results
                .iter()
                .any(|result| result["blocked"].as_bool() == Some(true))
        })
    {
        recommendations.push(
            "A recovery action was blocked; keep the task blocked or raise permission before retrying.".to_string(),
        );
    }
    if report["route_feedback"]["summaries"]
        .as_array()
        .is_some_and(|summaries| {
            summaries.iter().any(|summary| {
                summary["failures"].as_u64().unwrap_or(0) > 0
                    || summary["recovery_triggered"].as_u64().unwrap_or(0) > 0
            })
        })
    {
        recommendations.push(
            "Use route feedback to prefer models with higher verification success and lower recovery rate.".to_string(),
        );
    }
    let similar_count = report["task_memory"]["similar_count"].as_u64().unwrap_or(0);
    if similar_count > 0 {
        recommendations.push(format!(
            "Consult {similar_count} similar task memory entry/entries before planning the next task."
        ));
    }
    if let Some(items) = report["task_memory"]["context"]["recommendations"].as_array() {
        recommendations.extend(
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|item| format!("Memory policy: {item}")),
        );
    }
    if completed && !blocked {
        recommendations.push(
            "Reuse the recorded acceptance tests and route pattern for similar future tasks."
                .to_string(),
        );
    }
    if recommendations.is_empty() {
        recommendations.push("No adaptive recommendation is available yet.".to_string());
    }
    recommendations
}

fn task_failure_classification(
    task: &runtime::task_registry::Task,
    verification_decision: &runtime::VerificationDecision,
) -> Option<runtime::FailureClassification> {
    let classifier = runtime::FailureClassifier::new();
    if let runtime::VerificationDecision::Failed { .. } = verification_decision {
        if task.plan.as_ref().is_some_and(|plan| {
            plan.execution
                .nodes
                .values()
                .all(|node| node.status == runtime::PlanNodeStatus::Succeeded)
        }) {
            return classifier.classify_verification_decision(verification_decision);
        }
    }
    task.plan.as_ref().and_then(|plan| {
        plan.execution
            .nodes
            .values()
            .find(|node| node.status == runtime::PlanNodeStatus::Failed)
            .and_then(|node| node.failure_class.as_deref())
            .map(|reason| classifier.classify_reason(reason))
    })
}

fn task_plan_all_succeeded(task: &runtime::task_registry::Task) -> bool {
    task.plan.as_ref().is_some_and(|plan| {
        !plan.execution.nodes.is_empty()
            && plan
                .execution
                .nodes
                .values()
                .all(|node| node.status == runtime::PlanNodeStatus::Succeeded)
    })
}

fn task_current_blocker(
    task: &runtime::task_registry::Task,
    verification_decision: &runtime::VerificationDecision,
    failed_node: Option<&Value>,
) -> Value {
    let blocker = match task.status {
        runtime::TaskStatus::WaitingForPermission => Some("permission"),
        runtime::TaskStatus::WaitingForVerification => Some("verification"),
        runtime::TaskStatus::Blocked | runtime::TaskStatus::Failed => Some("blocked"),
        _ => None,
    };
    let reason = match blocker {
        Some("verification") => match verification_decision {
            runtime::VerificationDecision::Failed { reason } => Some(reason.clone()),
            runtime::VerificationDecision::Required(_) => Some("verification required".to_string()),
            runtime::VerificationDecision::NotRequired | runtime::VerificationDecision::Passed => {
                None
            }
        },
        Some("blocked") => failed_node
            .and_then(|node| node["failure_class"].as_str())
            .map(ToOwned::to_owned)
            .or_else(|| match verification_decision {
                runtime::VerificationDecision::Failed { reason } => Some(reason.clone()),
                runtime::VerificationDecision::Required(_) => {
                    Some("verification required".to_string())
                }
                runtime::VerificationDecision::NotRequired
                | runtime::VerificationDecision::Passed => None,
            }),
        Some("permission") => Some("permission required".to_string()),
        _ => None,
    };
    json!({
        "task_id": task.task_id.clone(),
        "status": task.status,
        "kind": blocker,
        "reason": reason,
    })
}

fn render_task_status_text(value: &Value) -> String {
    let task = &value["task"];
    let task_id = task["task_id"].as_str().unwrap_or("unknown");
    let status = task["status"].as_str().unwrap_or("unknown");
    let message = value["current_blocker"]["reason"]
        .as_str()
        .unwrap_or("no current blocker");
    let mut lines = vec![format!(
        "Task status\n  Task              {task_id}\n  Status            {status}\n  Blocker           {message}"
    )];
    if let Some(progress) = value.get("plan_progress").filter(|value| value.is_object()) {
        lines.push(format!(
            "  Plan              {}/{} succeeded, {} failed, {} running",
            progress["succeeded"].as_u64().unwrap_or(0),
            progress["total"].as_u64().unwrap_or(0),
            progress["failed"].as_u64().unwrap_or(0),
            progress["running"].as_u64().unwrap_or(0)
        ));
    }
    if let Some(policy) = value["verification"]["policy"].as_str() {
        lines.push(format!("  Verification      {policy}"));
    }
    lines.join("\n")
}

fn render_task_report_text(value: &Value) -> String {
    let task = &value["task"];
    let task_id = task["task_id"].as_str().unwrap_or("unknown");
    let status = task["status"].as_str().unwrap_or("unknown");
    let reports = task["execution_reports"]
        .as_array()
        .map_or(0, std::vec::Vec::len);
    let mut lines = vec![format!(
        "Task report\n  Task              {task_id}\n  Status            {status}\n  Execution reports {reports}"
    )];
    if let Some(report) = value
        .get("latest_execution_report")
        .filter(|value| value.is_object())
    {
        lines.push(format!(
            "  Last report       {} - {}",
            report["final_status"].as_str().unwrap_or("unknown"),
            report["message"].as_str().unwrap_or("")
        ));
        if let Some(progress) = report
            .get("plan_progress")
            .filter(|value| value.is_object())
        {
            lines.push(format!(
                "  Plan              {}/{} succeeded, {} failed, {} running",
                progress["succeeded"].as_u64().unwrap_or(0),
                progress["total"].as_u64().unwrap_or(0),
                progress["failed"].as_u64().unwrap_or(0),
                progress["running"].as_u64().unwrap_or(0)
            ));
        }
    }
    if let Some(failure) = value
        .get("latest_failure")
        .filter(|value| value.is_object())
    {
        lines.push(format!(
            "  Failure           {} - {}",
            failure["failure_class"].as_str().unwrap_or("unknown"),
            failure["reason"].as_str().unwrap_or("")
        ));
    }
    if let Some(action) = value
        .get("latest_recovery_action")
        .filter(|value| value.is_object())
    {
        let results = action["results"].as_array().map_or(0, std::vec::Vec::len);
        let executed = action["results"].as_array().map_or(0, |items| {
            items
                .iter()
                .filter(|item| item["executed"].as_bool() == Some(true))
                .count()
        });
        lines.push(format!("  Recovery action   {executed}/{results} executed"));
    }
    let route_feedback = &value["route_feedback"];
    lines.push(format!(
        "  Route feedback    {} combined ({} workspace)",
        route_feedback["combined_count"].as_u64().unwrap_or(0),
        route_feedback["workspace_count"].as_u64().unwrap_or(0)
    ));
    let task_memory = &value["task_memory"];
    if let Some(task_type) = task_memory["entry"]["task_type"].as_str() {
        lines.push(format!(
            "  Task memory       type={} similar={}",
            task_type,
            task_memory["similar_count"].as_u64().unwrap_or(0)
        ));
    }
    if let Some(recommendations) = task_memory["context"]["recommendations"].as_array() {
        if !recommendations.is_empty() {
            lines.push("  Memory policy".to_string());
            lines.extend(
                recommendations
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|item| format!("    - {item}")),
            );
        }
    }
    lines.join("\n")
}

fn render_task_review_text(value: &Value) -> String {
    let task = &value["task"];
    let task_id = task["task_id"].as_str().unwrap_or("unknown");
    let status = task["status"].as_str().unwrap_or("unknown");
    let mut lines = vec![format!(
        "Task review\n  Task              {task_id}\n  Status            {status}"
    )];
    let failure = &value["failure_summary"];
    if let Some(class) = failure["class"].as_str() {
        lines.push(format!(
            "  Failure           {} - {}",
            class,
            failure["reason"].as_str().unwrap_or("")
        ));
    }
    let recovery = &value["recovery_summary"];
    lines.push(format!(
        "  Recovery          {}/{} executed, {} blocked",
        recovery["executed"].as_u64().unwrap_or(0),
        recovery["actions"].as_u64().unwrap_or(0),
        recovery["blocked"].as_u64().unwrap_or(0)
    ));
    let route = &value["route_summary"];
    lines.push(format!(
        "  Routes            {} feedback entries across {} route(s)",
        route["feedback_count"].as_u64().unwrap_or(0),
        route["summary_count"].as_u64().unwrap_or(0)
    ));
    if let Some(recommendations) = value["recommendations"].as_array() {
        lines.push("  Recommendations".to_string());
        lines.extend(
            recommendations
                .iter()
                .filter_map(Value::as_str)
                .map(|item| format!("    - {item}")),
        );
    }
    lines.join("\n")
}

fn render_scheduler_explain_text(value: &runtime::DurableSchedulerExplain) -> String {
    let mut lines = vec![format!(
        "Scheduler explain\n  Task              {}\n  Would select      {}\n  Priority          {}\n  Runnable          {}\n  Reason            {}",
        value.task_id, value.would_select, value.task.priority, value.task.runnable, value.reason
    )];
    if let Some(selected) = value.selected_task_id.as_ref() {
        lines.push(format!("  Selected task     {selected}"));
    }
    if let Some(skip_reason) = value.task.skip_reason.as_ref() {
        lines.push(format!("  Skip reason       {skip_reason}"));
    }
    lines.join("\n")
}

fn run_task_packet_command(
    command: TaskPacketCliCommand,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        TaskPacketCliCommand::Create { path } => {
            let packet = read_task_packet(&path)?;
            let registry = load_task_registry()?;
            let task = registry.create_from_packet(packet)?;
            let task = persist_task_packet_plan(&registry, &task)?;
            save_task_registry(&registry)?;
            print_task_output(
                task_packet_status_value(&registry, task, "task_packet_create"),
                output_format,
            )?;
        }
        TaskPacketCliCommand::Run { path } => {
            let packet = read_task_packet(&path)?;
            let registry = load_task_registry()?;
            let task = registry.create_from_packet(packet)?;
            let task = persist_task_packet_plan(&registry, &task)?;
            registry.set_status(&task.task_id, runtime::TaskStatus::Running)?;
            let task = registry
                .get(&task.task_id)
                .ok_or_else(|| format!("task not found: {}", task.task_id))?;
            save_task_registry(&registry)?;
            print_task_output(
                task_packet_status_value(&registry, task, "task_packet_run"),
                output_format,
            )?;
        }
        TaskPacketCliCommand::Status { task_id } => {
            let registry = load_task_registry()?;
            let task = registry
                .get(&task_id)
                .ok_or_else(|| format!("task not found: {task_id}"))?;
            if task.task_packet.is_none() {
                return Err(format!("task {task_id} was not created from a task packet").into());
            }
            print_task_output(
                task_packet_status_value(&registry, task, "task_packet_status"),
                output_format,
            )?;
        }
    }
    Ok(())
}

fn run_task_scheduler_command(
    command: TaskSchedulerCliCommand,
    output_format: CliOutputFormat,
    permission_mode: PermissionMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let registry = load_task_registry()?;
    let worker_registry = load_worker_registry()?;
    let runner = runtime::VerificationRunner::new(Some(env::current_dir()?));
    let scheduler = runtime::DurableTaskScheduler::with_workers(
        registry.clone(),
        runner,
        worker_registry.clone(),
    )
    .with_permission_mode(permission_mode);
    match command {
        TaskSchedulerCliCommand::Tick => {
            let tick = scheduler.tick()?;
            save_task_registry(&registry)?;
            save_worker_registry(&worker_registry)?;
            match output_format {
                CliOutputFormat::Text => println!("{}", tick.message),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_task_output(
                        json!({"type":"task_scheduler_tick","tick":tick}),
                        output_format,
                    )?;
                }
            }
        }
        TaskSchedulerCliCommand::Queue => {
            let queue = scheduler.queue();
            match output_format {
                CliOutputFormat::Text => {
                    if queue.is_empty() {
                        println!("No scheduled tasks.");
                    } else {
                        for task in queue {
                            println!("{}\t{}\t{}", task.task_id, task.status, task.task_status);
                        }
                    }
                }
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_task_output(
                        json!({"type":"task_scheduler_queue","queue":queue}),
                        output_format,
                    )?;
                }
            }
        }
        TaskSchedulerCliCommand::Explain { task_id } => {
            let explanation = scheduler.explain(&task_id)?;
            match output_format {
                CliOutputFormat::Text => {
                    println!("{}", render_scheduler_explain_text(&explanation))
                }
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_task_output(
                        json!({"type":"task_scheduler_explain","explanation":explanation}),
                        output_format,
                    )?;
                }
            }
        }
        TaskSchedulerCliCommand::Run { max_ticks } => {
            let daemon = runtime::SchedulerDaemon::new(scheduler, scheduler_state_dir()?);
            let mut runs = Vec::new();
            for _ in 0..max_ticks {
                let run = daemon.run_once()?;
                let idle = run.tick.status == runtime::DurableSchedulerStatus::Idle;
                runs.push(run);
                save_task_registry(&registry)?;
                save_worker_registry(&worker_registry)?;
                if idle {
                    break;
                }
            }
            let state = runs.last().map(|run| run.state.clone());
            match output_format {
                CliOutputFormat::Text => {
                    if let Some(state) = &state {
                        println!(
                            "scheduler {:?}: {} ({} tick(s))",
                            state.status, state.message, state.tick_count
                        );
                    } else {
                        println!("scheduler did not run");
                    }
                }
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_task_output(
                        json!({
                            "type":"task_scheduler_daemon_run",
                            "runs":runs,
                            "state":state,
                        }),
                        output_format,
                    )?;
                }
            }
        }
        TaskSchedulerCliCommand::Status => {
            let daemon = runtime::SchedulerDaemon::new(scheduler, scheduler_state_dir()?);
            let state = daemon.load_state().ok();
            match output_format {
                CliOutputFormat::Text => {
                    if let Some(state) = &state {
                        println!(
                            "scheduler {:?}: {} ({} tick(s))",
                            state.status, state.message, state.tick_count
                        );
                    } else {
                        println!("scheduler has not run");
                    }
                }
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_task_output(
                        json!({
                            "type":"task_scheduler_daemon_status",
                            "state":state,
                            "state_path":daemon.state_path(),
                            "events_path":daemon.events_path(),
                        }),
                        output_format,
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn run_task_daemon_command(
    command: TaskDaemonCliCommand,
    output_format: CliOutputFormat,
    permission_mode: PermissionMode,
) -> Result<(), Box<dyn std::error::Error>> {
    let registry = load_task_registry()?;
    let worker_registry = load_worker_registry()?;
    let coordinator =
        build_autonomous_run_coordinator(&registry, &worker_registry, permission_mode)?;
    let daemon = runtime::SchedulerDaemon::new(
        runtime::DurableTaskScheduler::with_workers(
            registry.clone(),
            runtime::VerificationRunner::new(Some(env::current_dir()?)),
            worker_registry.clone(),
        )
        .with_permission_mode(permission_mode),
        scheduler_state_dir()?,
    );
    let report_limit = 20_usize;
    match command {
        TaskDaemonCliCommand::Start { max_ticks } => {
            let integration_review =
                build_autonomous_integration_review(report_limit, max_ticks, permission_mode)?;
            if !integration_review.health.safe_to_iterate {
                output_structured_report(
                    autonomous_preflight_blocked_value("tasks daemon start", &integration_review),
                    output_format,
                    render_autonomous_preflight_blocked_text,
                )?;
                return Ok(());
            }
            let run = coordinator.run_with_persist(max_ticks, |_| {
                save_task_registry(&registry)
                    .map_err(|error| io::Error::other(error.to_string()))?;
                save_worker_registry(&worker_registry)
                    .map_err(|error| io::Error::other(error.to_string()))?;
                Ok(())
            })?;
            save_task_registry(&registry)?;
            save_worker_registry(&worker_registry)?;
            let runs = run.scheduler_runs.clone();
            let state = run.latest_daemon_state.clone();
            let review = coordinator.review_policy(report_limit, max_ticks)?;
            match output_format {
                CliOutputFormat::Text => {
                    println!(
                        "daemon {}: {} ({} autonomous tick(s))",
                        run.status, run.message, run.tick_count
                    );
                    println!("{}", render_autonomous_policy_summary_text(&review));
                }
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_task_output(
                        json!({
                            "type":"task_scheduler_daemon_run",
                            "command":"start",
                            "runs":runs,
                            "state":state,
                            "run":run,
                            "summary":review.summary,
                            "policy_recommendation":review.recommendation,
                            "state_path":daemon.state_path(),
                            "events_path":daemon.events_path(),
                            "runs_path":coordinator.runs_path(),
                        }),
                        output_format,
                    )?;
                }
            }
        }
        TaskDaemonCliCommand::Status => {
            let state = daemon.load_state().ok();
            let latest_run = coordinator.latest_run().ok().flatten();
            let review = coordinator.review_policy(report_limit, 1)?;
            let integration_review =
                build_autonomous_integration_review(report_limit, 1, permission_mode)?;
            let health = serde_json::to_value(&integration_review.health)?;
            match output_format {
                CliOutputFormat::Text => {
                    if let Some(run) = &latest_run {
                        println!(
                            "daemon {}: {} ({} autonomous tick(s))",
                            run.status, run.message, run.tick_count
                        );
                    } else if let Some(state) = &state {
                        println!(
                            "daemon {:?}: {} ({} tick(s))",
                            state.status, state.message, state.tick_count
                        );
                    } else {
                        println!("daemon has not run");
                    }
                    println!("{}", render_autonomous_policy_summary_text(&review));
                    println!();
                    println!("{}", render_autonomous_health_checkpoint_text(&health));
                }
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_task_output(
                        json!({
                            "type":"task_scheduler_daemon_status",
                            "state":state,
                            "latest_run":latest_run,
                            "summary":review.summary,
                            "policy_recommendation":review.recommendation,
                            "health":health,
                            "state_path":daemon.state_path(),
                            "events_path":daemon.events_path(),
                            "runs_path":coordinator.runs_path(),
                        }),
                        output_format,
                    )?;
                }
            }
        }
        TaskDaemonCliCommand::Stop => {
            let state = daemon.stop()?;
            let latest_run = coordinator.latest_run().ok().flatten();
            match output_format {
                CliOutputFormat::Text => println!(
                    "daemon {:?}: {} ({} tick(s))",
                    state.status, state.message, state.tick_count
                ),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_task_output(
                        json!({
                            "type":"task_scheduler_daemon_status",
                            "command":"stop",
                            "state":state,
                            "latest_run":latest_run,
                            "state_path":daemon.state_path(),
                            "events_path":daemon.events_path(),
                            "runs_path":coordinator.runs_path(),
                        }),
                        output_format,
                    )?;
                }
            }
        }
        TaskDaemonCliCommand::Logs { limit } => {
            let events = daemon.load_events().unwrap_or_default();
            let start = events.len().saturating_sub(limit);
            let events = events.into_iter().skip(start).collect::<Vec<_>>();
            let recent_runs = coordinator.load_runs(limit).unwrap_or_default();
            let review = coordinator.review_policy(limit, 1)?;
            let integration_review =
                build_autonomous_integration_review(limit, 1, permission_mode)?;
            let health = serde_json::to_value(&integration_review.health)?;
            match output_format {
                CliOutputFormat::Text => {
                    if events.is_empty() && recent_runs.is_empty() {
                        println!("No daemon events or autonomous runs.");
                    }
                    for run in recent_runs {
                        println!(
                            "{}\t{}\t{}\t{}",
                            run.run_id, run.status, run.tick_count, run.message
                        );
                    }
                    for event in events {
                        println!(
                            "{}\t{}\t{:?}\t{}",
                            event.seq, event.event, event.status, event.message
                        );
                    }
                    println!("{}", render_autonomous_policy_summary_text(&review));
                    println!();
                    println!("{}", render_autonomous_health_checkpoint_text(&health));
                }
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_task_output(
                        json!({
                            "type":"task_scheduler_daemon_logs",
                            "events":events,
                            "runs":recent_runs,
                            "summary":review.summary,
                            "policy_recommendation":review.recommendation,
                            "health":health,
                            "events_path":daemon.events_path(),
                            "runs_path":coordinator.runs_path(),
                        }),
                        output_format,
                    )?;
                }
            }
        }
        TaskDaemonCliCommand::Report { limit, max_ticks } => {
            let state = daemon.load_state().ok();
            let latest_run = coordinator.latest_run().ok().flatten();
            let recent_runs = coordinator.load_runs(limit).unwrap_or_default();
            let review = coordinator.review_policy(limit, max_ticks)?;
            let integration_review =
                build_autonomous_integration_review(limit, max_ticks, permission_mode)?;
            match output_format {
                CliOutputFormat::Text => {
                    println!(
                        "{}",
                        render_autonomous_daemon_report_text(&review, &coordinator.runs_path())
                    );
                    println!();
                    println!(
                        "{}",
                        render_autonomous_integration_text(&json!({
                            "integration": integration_review.integration,
                            "health": integration_review.health,
                        }))
                    );
                }
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_task_output(
                        json!({
                            "type":"task_scheduler_daemon_report",
                            "state":state,
                            "latest_run":latest_run,
                            "runs":recent_runs,
                            "summary":review.summary,
                            "policy_recommendation":review.recommendation,
                            "integration":integration_review.integration,
                            "health":integration_review.health,
                            "state_path":daemon.state_path(),
                            "events_path":daemon.events_path(),
                            "runs_path":coordinator.runs_path(),
                        }),
                        output_format,
                    )?;
                }
            }
        }
        TaskDaemonCliCommand::Evaluate { limit, max_ticks } => {
            let state = daemon.load_state().ok();
            let latest_run = coordinator.latest_run().ok().flatten();
            let input = build_autonomous_evaluation_input(limit, max_ticks, permission_mode)?;
            let evaluation = runtime::evaluate_autonomous_loop(input);
            let integration_review =
                build_autonomous_integration_review(limit, max_ticks, permission_mode)?;
            let value = json!({
                "type":"task_scheduler_daemon_evaluation",
                "state":state,
                "latest_run":latest_run,
                "evaluation":evaluation,
                "integration":integration_review.integration,
                "health":integration_review.health,
                "state_path":daemon.state_path(),
                "events_path":daemon.events_path(),
                "runs_path":coordinator.runs_path(),
            });
            match output_format {
                CliOutputFormat::Text => {
                    println!("{}", render_autonomous_evaluation_text(&value));
                    println!();
                    println!("{}", render_autonomous_integration_text(&value));
                }
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_task_output(value, output_format)?;
                }
            }
        }
        TaskDaemonCliCommand::Replay { limit, max_ticks } => {
            let run_load = runtime::load_autonomous_run_reports_with_diagnostics(
                &scheduler_state_dir()?,
                limit,
            )?;
            let replay = runtime::replay_autonomous_trace(&run_load, permission_mode, max_ticks);
            let value = json!({
                "type":"task_scheduler_daemon_replay",
                "replay":replay,
                "malformed_lines":run_load.malformed_lines,
                "warnings":run_load.warnings,
                "runs_path":coordinator.runs_path(),
            });
            match output_format {
                CliOutputFormat::Text => println!("{}", render_autonomous_replay_text(&value)),
                CliOutputFormat::Json | CliOutputFormat::StreamJson => {
                    print_task_output(value, output_format)?;
                }
            }
        }
    }
    Ok(())
}

fn sessions_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let store = runtime::SessionStore::from_cwd(&cwd)
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)?;
    Ok(store.sessions_dir().to_path_buf())
}

fn create_managed_session_handle(
    session_id: &str,
) -> Result<SessionHandle, Box<dyn std::error::Error>> {
    let id = session_id.to_string();
    let path = sessions_dir()?.join(format!("{id}.{PRIMARY_SESSION_EXTENSION}"));
    Ok(SessionHandle { id, path })
}

fn resolve_session_reference(reference: &str) -> Result<SessionHandle, Box<dyn std::error::Error>> {
    if SESSION_REFERENCE_ALIASES
        .iter()
        .any(|alias| reference.eq_ignore_ascii_case(alias))
    {
        let latest = latest_managed_session()?;
        return Ok(SessionHandle {
            id: latest.id,
            path: latest.path,
        });
    }

    let direct = PathBuf::from(reference);
    let looks_like_path = direct.extension().is_some() || direct.components().count() > 1;
    let path = if direct.exists() {
        direct
    } else if looks_like_path {
        return Err(format_missing_session_reference(reference).into());
    } else {
        resolve_managed_session_path(reference)?
    };
    let id = path
        .file_name()
        .and_then(|value| value.to_str())
        .and_then(|name| {
            name.strip_suffix(&format!(".{PRIMARY_SESSION_EXTENSION}"))
                .or_else(|| name.strip_suffix(&format!(".{LEGACY_SESSION_EXTENSION}")))
        })
        .unwrap_or(reference)
        .to_string();
    Ok(SessionHandle { id, path })
}

fn resolve_managed_session_path(session_id: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    for directory in session_search_dirs()? {
        for extension in [PRIMARY_SESSION_EXTENSION, LEGACY_SESSION_EXTENSION] {
            let path = directory.join(format!("{session_id}.{extension}"));
            if path.exists() {
                return Ok(path);
            }
        }
    }
    Err(format_missing_session_reference(session_id).into())
}

fn is_managed_session_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|extension| {
            extension == PRIMARY_SESSION_EXTENSION || extension == LEGACY_SESSION_EXTENSION
        })
}

fn collect_sessions_from_dir(
    directory: &Path,
    sessions: &mut Vec<ManagedSessionSummary>,
) -> Result<(), Box<dyn std::error::Error>> {
    if !directory.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if !is_managed_session_file(&path) {
            continue;
        }
        let metadata = entry.metadata()?;
        let modified_epoch_millis = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis())
            .unwrap_or_default();
        let (id, message_count, parent_session_id, branch_name) =
            match Session::load_from_path(&path) {
                Ok(session) => {
                    let parent_session_id = session
                        .fork
                        .as_ref()
                        .map(|fork| fork.parent_session_id.clone());
                    let branch_name = session
                        .fork
                        .as_ref()
                        .and_then(|fork| fork.branch_name.clone());
                    (
                        session.session_id,
                        session.messages.len(),
                        parent_session_id,
                        branch_name,
                    )
                }
                Err(_) => (
                    path.file_stem()
                        .and_then(|value| value.to_str())
                        .unwrap_or("unknown")
                        .to_string(),
                    0,
                    None,
                    None,
                ),
            };
        sessions.push(ManagedSessionSummary {
            id,
            path,
            modified_epoch_millis,
            message_count,
            parent_session_id,
            branch_name,
        });
    }
    Ok(())
}

fn list_managed_sessions() -> Result<Vec<ManagedSessionSummary>, Box<dyn std::error::Error>> {
    let mut sessions = Vec::new();
    for directory in session_search_dirs()? {
        collect_sessions_from_dir(&directory, &mut sessions)?;
    }

    sessions.sort_by(|left, right| {
        right
            .modified_epoch_millis
            .cmp(&left.modified_epoch_millis)
            .then_with(|| right.id.cmp(&left.id))
    });
    Ok(sessions)
}

fn latest_managed_session() -> Result<ManagedSessionSummary, Box<dyn std::error::Error>> {
    list_managed_sessions()?
        .into_iter()
        .next()
        .ok_or_else(|| format_no_managed_sessions().into())
}

fn delete_managed_session(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if !path.exists() {
        return Err(format!("session file does not exist: {}", path.display()).into());
    }
    fs::remove_file(path)?;
    Ok(())
}

fn confirm_session_deletion(session_id: &str) -> bool {
    print!("Delete session '{session_id}'? This cannot be undone. [y/N]: ");
    io::stdout().flush().unwrap_or(());
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim(), "y" | "Y" | "yes" | "Yes" | "YES")
}

fn format_missing_session_reference(reference: &str) -> String {
    format!(
        "session not found: {reference}\nHint: managed sessions live in .Himalaya/sessions/. Try `{LATEST_SESSION_REFERENCE}` for the most recent session or `/session list` in the REPL."
    )
}

fn format_no_managed_sessions() -> String {
    format!(
        "no managed sessions found in .Himalaya/sessions/\nStart `Himalaya` to create a session, then rerun with `--resume {LATEST_SESSION_REFERENCE}`."
    )
}

fn session_search_dirs() -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let primary_dir = sessions_dir()?;
    let mut dirs = vec![primary_dir.clone()];

    let Some(root) = primary_dir
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "sessions"))
        .map(Path::to_path_buf)
    else {
        return Ok(dirs);
    };

    push_unique_session_dir(&mut dirs, root.clone());

    let mut namespace_dirs = match fs::read_dir(&root) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter_map(|entry| match entry.file_type() {
                Ok(file_type) if file_type.is_dir() => Some(entry.path()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    namespace_dirs.sort();
    for directory in namespace_dirs {
        push_unique_session_dir(&mut dirs, directory);
    }

    Ok(dirs)
}

fn push_unique_session_dir(dirs: &mut Vec<PathBuf>, directory: PathBuf) {
    if !dirs.iter().any(|existing| existing == &directory) {
        dirs.push(directory);
    }
}

fn render_session_list(active_session_id: &str) -> Result<String, Box<dyn std::error::Error>> {
    let sessions = list_managed_sessions()?;
    let mut lines = vec![
        "Sessions".to_string(),
        format!("  Directory         {}", sessions_dir()?.display()),
    ];
    if sessions.is_empty() {
        lines.push("  No managed sessions saved yet.".to_string());
        return Ok(lines.join("\n"));
    }
    for session in sessions {
        let marker = if session.id == active_session_id {
            "● current"
        } else {
            "○ saved"
        };
        let lineage = match (
            session.branch_name.as_deref(),
            session.parent_session_id.as_deref(),
        ) {
            (Some(branch_name), Some(parent_session_id)) => {
                format!(" branch={branch_name} from={parent_session_id}")
            }
            (None, Some(parent_session_id)) => format!(" from={parent_session_id}"),
            (Some(branch_name), None) => format!(" branch={branch_name}"),
            (None, None) => String::new(),
        };
        lines.push(format!(
            "  {id:<20} {marker:<10} msgs={msgs:<4} modified={modified}{lineage} path={path}",
            id = session.id,
            msgs = session.message_count,
            modified = format_session_modified_age(session.modified_epoch_millis),
            lineage = lineage,
            path = session.path.display(),
        ));
    }
    Ok(lines.join("\n"))
}

fn format_session_modified_age(modified_epoch_millis: u128) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map_or(modified_epoch_millis, |duration| duration.as_millis());
    let delta_seconds = now
        .saturating_sub(modified_epoch_millis)
        .checked_div(1_000)
        .unwrap_or_default();
    match delta_seconds {
        0..=4 => "just-now".to_string(),
        5..=59 => format!("{delta_seconds}s-ago"),
        60..=3_599 => format!("{}m-ago", delta_seconds / 60),
        3_600..=86_399 => format!("{}h-ago", delta_seconds / 3_600),
        _ => format!("{}d-ago", delta_seconds / 86_400),
    }
}

fn write_session_clear_backup(
    session: &Session,
    session_path: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let backup_path = session_clear_backup_path(session_path);
    session.save_to_path(&backup_path)?;
    Ok(backup_path)
}

fn session_clear_backup_path(session_path: &Path) -> PathBuf {
    let timestamp = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map_or(0, |duration| duration.as_millis());
    let file_name = session_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("session.jsonl");
    session_path.with_file_name(format!("{file_name}.before-clear-{timestamp}.bak"))
}

fn render_repl_help() -> String {
    [
        "REPL".to_string(),
        "  /exit                Quit the REPL".to_string(),
        "  /quit                Quit the REPL".to_string(),
        "  Up/Down              Navigate prompt history".to_string(),
        "  Ctrl-R               Reverse-search prompt history".to_string(),
        "  Tab                  Complete commands, modes, and recent sessions".to_string(),
        "  Ctrl-C               Clear input (or exit on empty prompt)".to_string(),
        "  Shift+Enter/Ctrl+J   Insert a newline".to_string(),
        "  Auto-save            .Himalaya/sessions/<session-id>.jsonl".to_string(),
        "  Resume latest        /resume latest".to_string(),
        "  Browse sessions      /session list".to_string(),
        "  Show prompt history  /history [count]".to_string(),
        String::new(),
        render_slash_command_help_filtered(),
    ]
    .join(
        "
",
    )
}

fn print_status_snapshot(
    model: &str,
    permission_mode: PermissionMode,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let usage = StatusUsage {
        message_count: 0,
        turns: 0,
        latest: TokenUsage::default(),
        cumulative: TokenUsage::default(),
        estimated_tokens: 0,
    };
    let context = status_context(None)?;
    match output_format {
        CliOutputFormat::Text => println!(
            "{}",
            format_status_report(model, usage, permission_mode.as_str(), &context)
        ),
        CliOutputFormat::Json | CliOutputFormat::StreamJson => println!(
            "{}",
            serde_json::to_string_pretty(&status_json_value(
                Some(model),
                usage,
                permission_mode.as_str(),
                &context,
            ))?
        ),
    }
    Ok(())
}

fn status_json_value(
    model: Option<&str>,
    usage: StatusUsage,
    permission_mode: &str,
    context: &StatusContext,
) -> serde_json::Value {
    json!({
        "kind": "status",
        "model": model,
        "permission_mode": permission_mode,
        "usage": {
            "messages": usage.message_count,
            "turns": usage.turns,
            "latest_total": usage.latest.total_tokens(),
            "cumulative_input": usage.cumulative.input_tokens,
            "cumulative_output": usage.cumulative.output_tokens,
            "cumulative_total": usage.cumulative.total_tokens(),
            "estimated_tokens": usage.estimated_tokens,
        },
        "workspace": {
            "cwd": context.cwd,
            "project_root": context.project_root,
            "git_branch": context.git_branch,
            "git_state": context.git_summary.headline(),
            "changed_files": context.git_summary.changed_files,
            "staged_files": context.git_summary.staged_files,
            "unstaged_files": context.git_summary.unstaged_files,
            "untracked_files": context.git_summary.untracked_files,
            "session": context.session_path.as_ref().map_or_else(|| "live-repl".to_string(), |path| path.display().to_string()),
            "session_id": context.session_path.as_ref().and_then(|path| {
                // Session files are named <session-id>.jsonl directly under
                // .Himalaya/sessions/. Extract the stem (drop the .jsonl extension).
                path.file_stem().map(|n| n.to_string_lossy().into_owned())
            }),
            "loaded_config_files": context.loaded_config_files,
            "discovered_config_files": context.discovered_config_files,
            "memory_file_count": context.memory_file_count,
        },
        "sandbox": {
            "enabled": context.sandbox_status.enabled,
            "active": context.sandbox_status.active,
            "supported": context.sandbox_status.supported,
            "in_container": context.sandbox_status.in_container,
            "requested_namespace": context.sandbox_status.requested.namespace_restrictions,
            "active_namespace": context.sandbox_status.namespace_active,
            "requested_network": context.sandbox_status.requested.network_isolation,
            "active_network": context.sandbox_status.network_active,
            "filesystem_mode": context.sandbox_status.filesystem_mode.as_str(),
            "filesystem_active": context.sandbox_status.filesystem_active,
            "allowed_mounts": context.sandbox_status.allowed_mounts,
            "markers": context.sandbox_status.container_markers,
            "fallback_reason": context.sandbox_status.fallback_reason,
        }
    })
}

fn status_context(
    session_path: Option<&Path>,
) -> Result<StatusContext, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let loader = ConfigLoader::default_for(&cwd);
    let discovered_config_files = loader.discover().len();
    let runtime_config = loader.load()?;
    let project_context = ProjectContext::discover_with_git(&cwd, DEFAULT_DATE)?;
    let (project_root, git_branch) =
        parse_git_status_metadata(project_context.git_status.as_deref());
    let git_summary = parse_git_workspace_summary(project_context.git_status.as_deref());
    let sandbox_status = resolve_sandbox_status(runtime_config.sandbox(), &cwd);
    Ok(StatusContext {
        cwd,
        session_path: session_path.map(Path::to_path_buf),
        loaded_config_files: runtime_config.loaded_entries().len(),
        discovered_config_files,
        memory_file_count: project_context.instruction_files.len(),
        project_root,
        git_branch,
        git_summary,
        sandbox_status,
    })
}

fn format_status_report(
    model: &str,
    usage: StatusUsage,
    permission_mode: &str,
    context: &StatusContext,
) -> String {
    [
        format!(
            "Status
  Model            {model}
  Permission mode  {permission_mode}
  Messages         {}
  Turns            {}
  Estimated tokens {}",
            usage.message_count, usage.turns, usage.estimated_tokens,
        ),
        format!(
            "Usage
  Latest total     {}
  Cumulative input {}
  Cumulative output {}
  Cumulative total {}",
            usage.latest.total_tokens(),
            usage.cumulative.input_tokens,
            usage.cumulative.output_tokens,
            usage.cumulative.total_tokens(),
        ),
        format!(
            "Workspace
  Cwd              {}
  Project root     {}
  Git branch       {}
  Git state        {}
  Changed files    {}
  Staged           {}
  Unstaged         {}
  Untracked        {}
  Session          {}
  Config files     loaded {}/{}
  Memory files     {}
  Suggested flow   /status → /diff → /commit",
            context.cwd.display(),
            context
                .project_root
                .as_ref()
                .map_or_else(|| "unknown".to_string(), |path| path.display().to_string()),
            context.git_branch.as_deref().unwrap_or("unknown"),
            context.git_summary.headline(),
            context.git_summary.changed_files,
            context.git_summary.staged_files,
            context.git_summary.unstaged_files,
            context.git_summary.untracked_files,
            context.session_path.as_ref().map_or_else(
                || "live-repl".to_string(),
                |path| path.display().to_string()
            ),
            context.loaded_config_files,
            context.discovered_config_files,
            context.memory_file_count,
        ),
        format_sandbox_report(&context.sandbox_status),
    ]
    .join(
        "

",
    )
}

fn format_sandbox_report(status: &runtime::SandboxStatus) -> String {
    format!(
        "Sandbox
  Enabled           {}
  Active            {}
  Supported         {}
  In container      {}
  Requested ns      {}
  Active ns         {}
  Requested net     {}
  Active net        {}
  Filesystem mode   {}
  Filesystem active {}
  Allowed mounts    {}
  Markers           {}
  Fallback reason   {}",
        status.enabled,
        status.active,
        status.supported,
        status.in_container,
        status.requested.namespace_restrictions,
        status.namespace_active,
        status.requested.network_isolation,
        status.network_active,
        status.filesystem_mode.as_str(),
        status.filesystem_active,
        if status.allowed_mounts.is_empty() {
            "<none>".to_string()
        } else {
            status.allowed_mounts.join(", ")
        },
        if status.container_markers.is_empty() {
            "<none>".to_string()
        } else {
            status.container_markers.join(", ")
        },
        status
            .fallback_reason
            .clone()
            .unwrap_or_else(|| "<none>".to_string()),
    )
}

fn format_commit_preflight_report(branch: Option<&str>, summary: GitWorkspaceSummary) -> String {
    format!(
        "Commit
  Result           ready
  Branch           {}
  Workspace        {}
  Changed files    {}
  Action           create a git commit from the current workspace changes",
        branch.unwrap_or("unknown"),
        summary.headline(),
        summary.changed_files,
    )
}

fn format_commit_skipped_report() -> String {
    "Commit
  Result           skipped
  Reason           no workspace changes
  Action           create a git commit from the current workspace changes
  Next             /status to inspect context · /diff to inspect repo changes"
        .to_string()
}

fn print_sandbox_status_snapshot(
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let loader = ConfigLoader::default_for(&cwd);
    let runtime_config = loader
        .load()
        .unwrap_or_else(|_| runtime::RuntimeConfig::empty());
    let status = resolve_sandbox_status(runtime_config.sandbox(), &cwd);
    match output_format {
        CliOutputFormat::Text => println!("{}", format_sandbox_report(&status)),
        CliOutputFormat::Json | CliOutputFormat::StreamJson => println!(
            "{}",
            serde_json::to_string_pretty(&sandbox_json_value(&status))?
        ),
    }
    Ok(())
}

fn sandbox_json_value(status: &runtime::SandboxStatus) -> serde_json::Value {
    json!({
        "kind": "sandbox",
        "enabled": status.enabled,
        "active": status.active,
        "supported": status.supported,
        "in_container": status.in_container,
        "requested_namespace": status.requested.namespace_restrictions,
        "active_namespace": status.namespace_active,
        "requested_network": status.requested.network_isolation,
        "active_network": status.network_active,
        "filesystem_mode": status.filesystem_mode.as_str(),
        "filesystem_active": status.filesystem_active,
        "allowed_mounts": status.allowed_mounts,
        "markers": status.container_markers,
        "fallback_reason": status.fallback_reason,
    })
}

fn render_help_topic(topic: LocalHelpTopic) -> String {
    match topic {
        LocalHelpTopic::Status => "Status
  Usage            Himalaya status
  Purpose          show the local workspace snapshot without entering the REPL
  Output           model, permissions, git state, config files, and sandbox status
  Related          /status · Himalaya --resume latest /status"
            .to_string(),
        LocalHelpTopic::Sandbox => "Sandbox
  Usage            Himalaya sandbox
  Purpose          inspect the resolved sandbox and isolation state for the current directory
  Output           namespace, network, filesystem, and fallback details
  Related          /sandbox · Himalaya status"
            .to_string(),
        LocalHelpTopic::Doctor => "Doctor
  Usage            Himalaya doctor
  Purpose          diagnose local auth, config, workspace, sandbox, and build metadata
  Output           local-only health report; no provider request or session resume required
  Related          /doctor · Himalaya --resume latest /doctor"
            .to_string(),
    }
}

fn print_help_topic(topic: LocalHelpTopic) {
    println!("{}", render_help_topic(topic));
}

fn render_config_report(section: Option<&str>) -> Result<String, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let loader = ConfigLoader::default_for(&cwd);
    let discovered = loader.discover();
    let runtime_config = loader.load()?;

    let mut lines = vec![
        format!(
            "Config
  Working directory {}
  Loaded files      {}
  Merged keys       {}",
            cwd.display(),
            runtime_config.loaded_entries().len(),
            runtime_config.merged().len()
        ),
        "Discovered files".to_string(),
    ];
    for entry in discovered {
        let source = match entry.source {
            ConfigSource::User => "user",
            ConfigSource::Project => "project",
            ConfigSource::Local => "local",
        };
        let status = if runtime_config
            .loaded_entries()
            .iter()
            .any(|loaded_entry| loaded_entry.path == entry.path)
        {
            "loaded"
        } else {
            "missing"
        };
        lines.push(format!(
            "  {source:<7} {status:<7} {}",
            entry.path.display()
        ));
    }

    if let Some(section) = section {
        lines.push(format!("Merged section: {section}"));
        let value = match section {
            "env" => runtime_config.get("env"),
            "hooks" => runtime_config.get("hooks"),
            "model" => runtime_config.get("model"),
            "plugins" => runtime_config
                .get("plugins")
                .or_else(|| runtime_config.get("enabledPlugins")),
            other => {
                lines.push(format!(
                    "  Unsupported config section '{other}'. Use env, hooks, model, or plugins."
                ));
                return Ok(lines.join(
                    "
",
                ));
            }
        };
        lines.push(format!(
            "  {}",
            match value {
                Some(value) => value.render(),
                None => "<unset>".to_string(),
            }
        ));
        return Ok(lines.join(
            "
",
        ));
    }

    lines.push("Merged JSON".to_string());
    lines.push(format!("  {}", runtime_config.as_json().render()));
    Ok(lines.join(
        "
",
    ))
}

fn render_config_json(
    _section: Option<&str>,
) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let loader = ConfigLoader::default_for(&cwd);
    let discovered = loader.discover();
    let runtime_config = loader.load()?;

    let loaded_paths: Vec<_> = runtime_config
        .loaded_entries()
        .iter()
        .map(|e| e.path.display().to_string())
        .collect();

    let files: Vec<_> = discovered
        .iter()
        .map(|e| {
            let source = match e.source {
                ConfigSource::User => "user",
                ConfigSource::Project => "project",
                ConfigSource::Local => "local",
            };
            let loaded = runtime_config
                .loaded_entries()
                .iter()
                .any(|le| le.path == e.path);
            serde_json::json!({
                "path": e.path.display().to_string(),
                "source": source,
                "loaded": loaded,
            })
        })
        .collect();

    Ok(serde_json::json!({
        "kind": "config",
        "cwd": cwd.display().to_string(),
        "loaded_files": loaded_paths.len(),
        "merged_keys": runtime_config.merged().len(),
        "files": files,
    }))
}

fn render_memory_report() -> Result<String, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let project_context = ProjectContext::discover(&cwd, DEFAULT_DATE)?;
    let mut lines = vec![format!(
        "Memory
  Working directory {}
  Instruction files {}",
        cwd.display(),
        project_context.instruction_files.len()
    )];
    if project_context.instruction_files.is_empty() {
        lines.push("Discovered files".to_string());
        lines.push(
            "  No Himalaya instruction files discovered in the current directory ancestry."
                .to_string(),
        );
    } else {
        lines.push("Discovered files".to_string());
        for (index, file) in project_context.instruction_files.iter().enumerate() {
            let preview = file.content.lines().next().unwrap_or("").trim();
            let preview = if preview.is_empty() {
                "<empty>"
            } else {
                preview
            };
            lines.push(format!("  {}. {}", index + 1, file.path.display(),));
            lines.push(format!(
                "     lines={} preview={}",
                file.content.lines().count(),
                preview
            ));
        }
    }
    Ok(lines.join(
        "
",
    ))
}

fn render_memory_json() -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let project_context = ProjectContext::discover(&cwd, DEFAULT_DATE)?;
    let files: Vec<_> = project_context
        .instruction_files
        .iter()
        .map(|f| {
            json!({
                "path": f.path.display().to_string(),
                "lines": f.content.lines().count(),
                "preview": f.content.lines().next().unwrap_or("").trim(),
            })
        })
        .collect();
    Ok(json!({
        "kind": "memory",
        "cwd": cwd.display().to_string(),
        "instruction_files": files.len(),
        "files": files,
    }))
}

fn init_Himalaya_md() -> Result<String, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    Ok(initialize_repo(&cwd)?.render())
}

fn run_init(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    let message = init_Himalaya_md()?;
    match output_format {
        CliOutputFormat::Text => println!("{message}"),
        CliOutputFormat::Json | CliOutputFormat::StreamJson => println!(
            "{}",
            serde_json::to_string_pretty(&init_json_value(&message))?
        ),
    }
    Ok(())
}

fn init_json_value(message: &str) -> serde_json::Value {
    json!({
        "kind": "init",
        "message": message,
    })
}

fn normalize_permission_mode(mode: &str) -> Option<&'static str> {
    PermissionMode::parse_public(mode).map(PermissionMode::as_str)
}

fn render_diff_report() -> Result<String, Box<dyn std::error::Error>> {
    render_diff_report_for(&env::current_dir()?)
}

fn render_diff_report_for(cwd: &Path) -> Result<String, Box<dyn std::error::Error>> {
    // Verify we are inside a git repository before calling `git diff`.
    // Running `git diff --cached` outside a git tree produces a misleading
    // "unknown option `cached`" error because git falls back to --no-index mode.
    let in_git_repo = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !in_git_repo {
        return Ok(format!(
            "Diff\n  Result           no git repository\n  Detail           {} is not inside a git project",
            cwd.display()
        ));
    }
    let staged = run_git_diff_command_in(cwd, &["diff", "--cached"])?;
    let unstaged = run_git_diff_command_in(cwd, &["diff"])?;
    if staged.trim().is_empty() && unstaged.trim().is_empty() {
        return Ok(
            "Diff\n  Result           clean working tree\n  Detail           no current changes"
                .to_string(),
        );
    }

    let mut sections = Vec::new();
    if !staged.trim().is_empty() {
        sections.push(format!("Staged changes:\n{}", staged.trim_end()));
    }
    if !unstaged.trim().is_empty() {
        sections.push(format!("Unstaged changes:\n{}", unstaged.trim_end()));
    }

    Ok(format!("Diff\n\n{}", sections.join("\n\n")))
}

fn render_diff_json_for(cwd: &Path) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
    let in_git_repo = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !in_git_repo {
        return Ok(serde_json::json!({
            "kind": "diff",
            "result": "no_git_repo",
            "detail": format!("{} is not inside a git project", cwd.display()),
        }));
    }
    let staged = run_git_diff_command_in(cwd, &["diff", "--cached"])?;
    let unstaged = run_git_diff_command_in(cwd, &["diff"])?;
    Ok(serde_json::json!({
        "kind": "diff",
        "result": if staged.trim().is_empty() && unstaged.trim().is_empty() { "clean" } else { "changes" },
        "staged": staged.trim(),
        "unstaged": unstaged.trim(),
    }))
}

fn run_git_diff_command_in(
    cwd: &Path,
    args: &[&str],
) -> Result<String, Box<dyn std::error::Error>> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("git {} failed: {stderr}", args.join(" ")).into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn render_teleport_report(target: &str) -> Result<String, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;

    let file_list = Command::new("rg")
        .args(["--files"])
        .current_dir(&cwd)
        .output()?;
    let file_matches = if file_list.status.success() {
        String::from_utf8(file_list.stdout)?
            .lines()
            .filter(|line| line.contains(target))
            .take(10)
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let content_output = Command::new("rg")
        .args(["-n", "-S", "--color", "never", target, "."])
        .current_dir(&cwd)
        .output()?;

    let mut lines = vec![
        "Teleport".to_string(),
        format!("  Target           {target}"),
        "  Action           search workspace files and content for the target".to_string(),
    ];
    if !file_matches.is_empty() {
        lines.push(String::new());
        lines.push("File matches".to_string());
        lines.extend(file_matches.into_iter().map(|path| format!("  {path}")));
    }

    if content_output.status.success() {
        let matches = String::from_utf8(content_output.stdout)?;
        if !matches.trim().is_empty() {
            lines.push(String::new());
            lines.push("Content matches".to_string());
            lines.push(truncate_for_prompt(&matches, 4_000));
        }
    }

    if lines.len() == 1 {
        lines.push("  Result           no matches found".to_string());
    }

    Ok(lines.join("\n"))
}

fn render_last_tool_debug_report(session: &Session) -> Result<String, Box<dyn std::error::Error>> {
    let last_tool_use = session
        .messages
        .iter()
        .rev()
        .find_map(|message| {
            message.blocks.iter().rev().find_map(|block| match block {
                ContentBlock::ToolUse { id, name, input } => {
                    Some((id.clone(), name.clone(), input.clone()))
                }
                _ => None,
            })
        })
        .ok_or_else(|| "no prior tool call found in session".to_string())?;

    let tool_result = session.messages.iter().rev().find_map(|message| {
        message.blocks.iter().rev().find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                tool_name,
                output,
                is_error,
            } if tool_use_id == &last_tool_use.0 => {
                Some((tool_name.clone(), output.clone(), *is_error))
            }
            _ => None,
        })
    });

    let mut lines = vec![
        "Debug tool call".to_string(),
        "  Action           inspect the last recorded tool call and its result".to_string(),
        format!("  Tool id          {}", last_tool_use.0),
        format!("  Tool name        {}", last_tool_use.1),
        "  Input".to_string(),
        indent_block(&last_tool_use.2, 4),
    ];

    match tool_result {
        Some((tool_name, output, is_error)) => {
            lines.push("  Result".to_string());
            lines.push(format!("    name           {tool_name}"));
            lines.push(format!(
                "    status         {}",
                if is_error { "error" } else { "ok" }
            ));
            lines.push(indent_block(&output, 4));
        }
        None => lines.push("  Result           missing tool result".to_string()),
    }

    Ok(lines.join("\n"))
}

fn indent_block(value: &str, spaces: usize) -> String {
    let indent = " ".repeat(spaces);
    value
        .lines()
        .map(|line| format!("{indent}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn validate_no_args(
    command_name: &str,
    args: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(args) = args.map(str::trim).filter(|value| !value.is_empty()) {
        return Err(format!(
            "{command_name} does not accept arguments. Received: {args}\nUsage: {command_name}"
        )
        .into());
    }
    Ok(())
}

fn format_bughunter_report(scope: Option<&str>) -> String {
    format!(
        "Bughunter
  Scope            {}
  Action           inspect the selected code for likely bugs and correctness issues
  Output           findings should include file paths, severity, and suggested fixes",
        scope.unwrap_or("the current repository")
    )
}

fn format_ultraplan_report(task: Option<&str>) -> String {
    format!(
        "Ultraplan
  Task             {}
  Action           break work into a multi-step execution plan
  Output           plan should cover goals, risks, sequencing, verification, and rollback",
        task.unwrap_or("the current repo work")
    )
}

fn format_pr_report(branch: &str, context: Option<&str>) -> String {
    format!(
        "PR
  Branch           {branch}
  Context          {}
  Action           draft or create a pull request for the current branch
  Output           title and markdown body suitable for GitHub",
        context.unwrap_or("none")
    )
}

fn format_issue_report(context: Option<&str>) -> String {
    format!(
        "Issue
  Context          {}
  Action           draft or create a GitHub issue from the current context
  Output           title and markdown body suitable for GitHub",
        context.unwrap_or("none")
    )
}

fn git_output(args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(env::current_dir()?)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("git {} failed: {stderr}", args.join(" ")).into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn git_status_ok(args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(env::current_dir()?)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("git {} failed: {stderr}", args.join(" ")).into());
    }
    Ok(())
}

fn command_exists(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn write_temp_text_file(
    filename: &str,
    contents: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let path = env::temp_dir().join(filename);
    fs::write(&path, contents)?;
    Ok(path)
}

const DEFAULT_HISTORY_LIMIT: usize = 20;

fn parse_history_count(raw: Option<&str>) -> Result<usize, String> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_HISTORY_LIMIT);
    };
    let parsed: usize = raw
        .parse()
        .map_err(|_| format!("history: invalid count '{raw}'. Expected a positive integer."))?;
    if parsed == 0 {
        return Err("history: count must be greater than 0.".to_string());
    }
    Ok(parsed)
}

fn format_history_timestamp(timestamp_ms: u64) -> String {
    let secs = timestamp_ms / 1_000;
    let subsec_ms = timestamp_ms % 1_000;
    let days_since_epoch = secs / 86_400;
    let seconds_of_day = secs % 86_400;
    let hours = seconds_of_day / 3_600;
    let minutes = (seconds_of_day % 3_600) / 60;
    let seconds = seconds_of_day % 60;

    let (year, month, day) = civil_from_days(i64::try_from(days_since_epoch).unwrap_or(0));
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}.{subsec_ms:03}Z")
}

// Computes civil (Gregorian) year/month/day from days since the Unix epoch
// (1970-01-01) using Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let doe = (z - era * 146_097) as u64; // [0, 146_096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = y + i64::from(m <= 2);
    (y as i32, m as u32, d as u32)
}

fn render_prompt_history_report(entries: &[PromptHistoryEntry], limit: usize) -> String {
    if entries.is_empty() {
        return "Prompt history\n  Result           no prompts recorded yet".to_string();
    }

    let total = entries.len();
    let start = total.saturating_sub(limit);
    let shown = &entries[start..];
    let mut lines = vec![
        "Prompt history".to_string(),
        format!("  Total            {total}"),
        format!("  Showing          {} most recent", shown.len()),
        format!("  Reverse search   Ctrl-R in the REPL"),
        String::new(),
    ];
    for (offset, entry) in shown.iter().enumerate() {
        let absolute_index = start + offset + 1;
        let timestamp = format_history_timestamp(entry.timestamp_ms);
        let first_line = entry.text.lines().next().unwrap_or("").trim();
        let display = if first_line.chars().count() > 80 {
            let truncated: String = first_line.chars().take(77).collect();
            format!("{truncated}...")
        } else {
            first_line.to_string()
        };
        lines.push(format!("  {absolute_index:>3}. [{timestamp}] {display}"));
    }
    lines.join("\n")
}

fn collect_session_prompt_history(session: &Session) -> Vec<PromptHistoryEntry> {
    if !session.prompt_history.is_empty() {
        return session
            .prompt_history
            .iter()
            .map(|entry| PromptHistoryEntry {
                timestamp_ms: entry.timestamp_ms,
                text: entry.text.clone(),
            })
            .collect();
    }
    let timestamp_ms = session.updated_at_ms;
    session
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .filter_map(|message| {
            message.blocks.iter().find_map(|block| match block {
                ContentBlock::Text { text } => Some(PromptHistoryEntry {
                    timestamp_ms,
                    text: text.clone(),
                }),
                _ => None,
            })
        })
        .collect()
}

fn recent_user_context(session: &Session, limit: usize) -> String {
    let requests = session
        .messages
        .iter()
        .filter(|message| message.role == MessageRole::User)
        .filter_map(|message| {
            message.blocks.iter().find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.trim().to_string()),
                _ => None,
            })
        })
        .rev()
        .take(limit)
        .collect::<Vec<_>>();

    if requests.is_empty() {
        "<no prior user messages>".to_string()
    } else {
        requests
            .into_iter()
            .rev()
            .enumerate()
            .map(|(index, text)| format!("{}. {}", index + 1, text))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn truncate_for_prompt(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        value.trim().to_string()
    } else {
        let truncated = value.chars().take(limit).collect::<String>();
        format!("{}\n…[truncated]", truncated.trim_end())
    }
}

fn sanitize_generated_message(value: &str) -> String {
    value.trim().trim_matches('`').trim().replace("\r\n", "\n")
}

fn parse_titled_body(value: &str) -> Option<(String, String)> {
    let normalized = sanitize_generated_message(value);
    let title = normalized
        .lines()
        .find_map(|line| line.strip_prefix("TITLE:").map(str::trim))?;
    let body_start = normalized.find("BODY:")?;
    let body = normalized[body_start + "BODY:".len()..].trim();
    Some((title.to_string(), body.to_string()))
}

fn render_version_report() -> String {
    let git_sha = GIT_SHA.unwrap_or("unknown");
    let target = BUILD_TARGET.unwrap_or("unknown");
    format!(
        "Himalaya Code\n  Version          {VERSION}\n  Git SHA          {git_sha}\n  Target           {target}\n  Build date       {DEFAULT_DATE}"
    )
}

fn render_export_text(session: &Session) -> String {
    let mut lines = vec!["# Conversation Export".to_string(), String::new()];
    for (index, message) in session.messages.iter().enumerate() {
        let role = match message.role {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        };
        lines.push(format!("## {}. {role}", index + 1));
        for block in &message.blocks {
            match block {
                ContentBlock::Text { text } => lines.push(text.clone()),
                ContentBlock::ToolUse { id, name, input } => {
                    lines.push(format!("[tool_use id={id} name={name}] {input}"));
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    tool_name,
                    output,
                    is_error,
                } => {
                    lines.push(format!(
                        "[tool_result id={tool_use_id} name={tool_name} error={is_error}] {output}"
                    ));
                }
                ContentBlock::Image { media_type, .. } => {
                    lines.push(format!("[image: {media_type}]"));
                }
                ContentBlock::Thinking { thinking, .. } => {
                    lines.push(format!("[thinking: {} chars]", thinking.chars().count()));
                }
                ContentBlock::RedactedThinking { .. } => {
                    lines.push("[redacted_thinking]".to_string());
                }
            }
        }
        lines.push(String::new());
    }
    lines.join("\n")
}

fn default_export_filename(session: &Session) -> String {
    let stem = session
        .messages
        .iter()
        .find_map(|message| match message.role {
            MessageRole::User => message.blocks.iter().find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            }),
            _ => None,
        })
        .map_or("conversation", |text| {
            text.lines().next().unwrap_or("conversation")
        })
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .take(8)
        .collect::<Vec<_>>()
        .join("-");
    let fallback = if stem.is_empty() {
        "conversation"
    } else {
        &stem
    };
    format!("{fallback}.txt")
}

fn resolve_export_path(
    requested_path: Option<&str>,
    session: &Session,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let file_name =
        requested_path.map_or_else(|| default_export_filename(session), ToOwned::to_owned);
    let final_name = if Path::new(&file_name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("txt"))
    {
        file_name
    } else {
        format!("{file_name}.txt")
    };
    Ok(cwd.join(final_name))
}

const SESSION_MARKDOWN_TOOL_SUMMARY_LIMIT: usize = 280;

fn summarize_tool_payload_for_markdown(payload: &str) -> String {
    let compact = match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(value) => value.to_string(),
        Err(_) => payload.split_whitespace().collect::<Vec<_>>().join(" "),
    };
    if compact.is_empty() {
        return String::new();
    }
    truncate_for_summary(&compact, SESSION_MARKDOWN_TOOL_SUMMARY_LIMIT)
}

fn run_export(
    session_reference: &str,
    output_path: Option<&Path>,
    output_format: CliOutputFormat,
) -> Result<(), Box<dyn std::error::Error>> {
    let handle = resolve_session_reference(session_reference)?;
    let session = Session::load_from_path(&handle.path)?;
    let markdown = render_session_markdown(&session, &handle.id, &handle.path);

    if let Some(path) = output_path {
        fs::write(path, &markdown)?;
        let report = format!(
            "Export\n  Result           wrote markdown transcript\n  File             {}\n  Session          {}\n  Messages         {}",
            path.display(),
            handle.id,
            session.messages.len(),
        );
        match output_format {
            CliOutputFormat::Text => println!("{report}"),
            CliOutputFormat::Json | CliOutputFormat::StreamJson => println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "kind": "export",
                    "message": report,
                    "session_id": handle.id,
                    "file": path.display().to_string(),
                    "messages": session.messages.len(),
                }))?
            ),
        }
        return Ok(());
    }

    match output_format {
        CliOutputFormat::Text => {
            print!("{markdown}");
            if !markdown.ends_with('\n') {
                println!();
            }
        }
        CliOutputFormat::Json | CliOutputFormat::StreamJson => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "kind": "export",
                "session_id": handle.id,
                "file": handle.path.display().to_string(),
                "messages": session.messages.len(),
                "markdown": markdown,
            }))?
        ),
    }
    Ok(())
}

fn render_session_markdown(session: &Session, session_id: &str, session_path: &Path) -> String {
    let mut lines = vec![
        "# Conversation Export".to_string(),
        String::new(),
        format!("- **Session**: `{session_id}`"),
        format!("- **File**: `{}`", session_path.display()),
        format!("- **Messages**: {}", session.messages.len()),
    ];
    if let Some(workspace_root) = session.workspace_root() {
        lines.push(format!("- **Workspace**: `{}`", workspace_root.display()));
    }
    if let Some(fork) = &session.fork {
        let branch = fork.branch_name.as_deref().unwrap_or("(unnamed)");
        lines.push(format!(
            "- **Forked from**: `{}` (branch `{branch}`)",
            fork.parent_session_id
        ));
    }
    if let Some(compaction) = &session.compaction {
        lines.push(format!(
            "- **Compactions**: {} (last removed {} messages)",
            compaction.count, compaction.removed_message_count
        ));
    }
    lines.push(String::new());
    lines.push("---".to_string());
    lines.push(String::new());

    for (index, message) in session.messages.iter().enumerate() {
        let role = match message.role {
            MessageRole::System => "System",
            MessageRole::User => "User",
            MessageRole::Assistant => "Assistant",
            MessageRole::Tool => "Tool",
        };
        lines.push(format!("## {}. {role}", index + 1));
        lines.push(String::new());
        for block in &message.blocks {
            match block {
                ContentBlock::Text { text } => {
                    let trimmed = text.trim_end();
                    if !trimmed.is_empty() {
                        lines.push(trimmed.to_string());
                        lines.push(String::new());
                    }
                }
                ContentBlock::ToolUse { id, name, input } => {
                    lines.push(format!(
                        "**Tool call** `{name}` _(id `{}`)_",
                        short_tool_id(id)
                    ));
                    let summary = summarize_tool_payload_for_markdown(input);
                    if !summary.is_empty() {
                        lines.push(format!("> {summary}"));
                    }
                    lines.push(String::new());
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    tool_name,
                    output,
                    is_error,
                } => {
                    let status = if *is_error { "error" } else { "ok" };
                    lines.push(format!(
                        "**Tool result** `{tool_name}` _(id `{}`, {status})_",
                        short_tool_id(tool_use_id)
                    ));
                    let summary = summarize_tool_payload_for_markdown(output);
                    if !summary.is_empty() {
                        lines.push(format!("> {summary}"));
                    }
                    lines.push(String::new());
                }
                ContentBlock::Image { media_type, .. } => {
                    lines.push(format!("**Image** _{media_type}_"));
                    lines.push(String::new());
                }
                ContentBlock::Thinking { thinking, .. } => {
                    lines.push(format!(
                        "**Thinking** _({} chars)_",
                        thinking.chars().count()
                    ));
                    lines.push(String::new());
                }
                ContentBlock::RedactedThinking { .. } => {
                    lines.push("**Redacted thinking**".to_string());
                    lines.push(String::new());
                }
            }
        }
        if let Some(usage) = message.usage {
            lines.push(format!(
                "_tokens: in={} out={} cache_create={} cache_read={}_",
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_creation_input_tokens,
                usage.cache_read_input_tokens,
            ));
            lines.push(String::new());
        }
    }
    lines.join("\n")
}

fn short_tool_id(id: &str) -> String {
    let char_count = id.chars().count();
    if char_count <= 12 {
        return id.to_string();
    }
    let prefix: String = id.chars().take(12).collect();
    format!("{prefix}…")
}

fn build_system_prompt() -> Result<Vec<String>, Box<dyn std::error::Error>> {
    Ok(load_system_prompt(
        env::current_dir()?,
        DEFAULT_DATE,
        env::consts::OS,
        "unknown",
    )?)
}

fn build_runtime_plugin_state() -> Result<RuntimePluginState, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let loader = ConfigLoader::default_for(&cwd);
    let runtime_config = loader.load()?;
    build_runtime_plugin_state_with_loader(&cwd, &loader, &runtime_config)
}

fn build_runtime_plugin_state_with_loader(
    cwd: &Path,
    loader: &ConfigLoader,
    runtime_config: &runtime::RuntimeConfig,
) -> Result<RuntimePluginState, Box<dyn std::error::Error>> {
    let plugin_manager = build_plugin_manager(cwd, loader, runtime_config);
    let plugin_registry = plugin_manager.plugin_registry()?;
    let plugin_hook_config =
        runtime_hook_config_from_plugin_hooks(plugin_registry.aggregated_hooks()?);
    let feature_config = runtime_config
        .feature_config()
        .clone()
        .with_hooks(runtime_config.hooks().merged(&plugin_hook_config));
    // When the user has not explicitly configured a decisioning block, enable
    // the lightweight plan/decisioning event stream by default so the task
    // board, current-node, and related panels populate. This is heuristic-only
    // (no extra model calls); structured execution and team convergence stay
    // opt-in via their thresholds, so plain turns keep their latency profile.
    let feature_config = if feature_config.decisioning().user_specified() {
        feature_config
    } else {
        let decisioning = feature_config
            .decisioning()
            .clone()
            .with_enabled(true)
            .with_emit_events(true);
        feature_config.with_decisioning(decisioning)
    };
    let (mcp_state, runtime_tools) = build_runtime_mcp_state(runtime_config)?;
    let tool_registry = GlobalToolRegistry::with_plugin_tools(plugin_registry.aggregated_tools()?)?
        .with_runtime_tools(runtime_tools)?;
    Ok(RuntimePluginState {
        feature_config,
        tool_registry,
        plugin_registry,
        mcp_state,
    })
}

fn build_plugin_manager(
    cwd: &Path,
    loader: &ConfigLoader,
    runtime_config: &runtime::RuntimeConfig,
) -> PluginManager {
    let plugin_settings = runtime_config.plugins();
    let mut plugin_config = PluginManagerConfig::new(loader.config_home().to_path_buf());
    plugin_config.enabled_plugins = plugin_settings.enabled_plugins().clone();
    plugin_config.external_dirs = plugin_settings
        .external_directories()
        .iter()
        .map(|path| resolve_plugin_path(cwd, loader.config_home(), path))
        .collect();
    plugin_config.install_root = plugin_settings
        .install_root()
        .map(|path| resolve_plugin_path(cwd, loader.config_home(), path));
    plugin_config.registry_path = plugin_settings
        .registry_path()
        .map(|path| resolve_plugin_path(cwd, loader.config_home(), path));
    plugin_config.bundled_root = plugin_settings
        .bundled_root()
        .map(|path| resolve_plugin_path(cwd, loader.config_home(), path));
    PluginManager::new(plugin_config)
}

fn resolve_plugin_path(cwd: &Path, config_home: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else if value.starts_with('.') {
        cwd.join(path)
    } else {
        config_home.join(path)
    }
}

fn runtime_hook_config_from_plugin_hooks(hooks: PluginHooks) -> runtime::RuntimeHookConfig {
    runtime::RuntimeHookConfig::new(
        hooks.pre_tool_use,
        hooks.post_tool_use,
        hooks.post_tool_use_failure,
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InternalPromptProgressState {
    command_label: &'static str,
    task_label: String,
    step: usize,
    phase: String,
    detail: Option<String>,
    saw_final_text: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InternalPromptProgressEvent {
    Started,
    Update,
    Heartbeat,
    Complete,
    Failed,
}

#[derive(Debug)]
struct InternalPromptProgressShared {
    state: Mutex<InternalPromptProgressState>,
    output_lock: Mutex<()>,
    started_at: Instant,
}

#[derive(Debug, Clone)]
struct InternalPromptProgressReporter {
    shared: Arc<InternalPromptProgressShared>,
}

#[derive(Debug)]
struct InternalPromptProgressRun {
    reporter: InternalPromptProgressReporter,
    heartbeat_stop: Option<mpsc::Sender<()>>,
    heartbeat_handle: Option<thread::JoinHandle<()>>,
}

impl InternalPromptProgressReporter {
    fn ultraplan(task: &str) -> Self {
        Self {
            shared: Arc::new(InternalPromptProgressShared {
                state: Mutex::new(InternalPromptProgressState {
                    command_label: "Ultraplan",
                    task_label: task.to_string(),
                    step: 0,
                    phase: "planning started".to_string(),
                    detail: Some(format!("task: {task}")),
                    saw_final_text: false,
                }),
                output_lock: Mutex::new(()),
                started_at: Instant::now(),
            }),
        }
    }

    fn emit(&self, event: InternalPromptProgressEvent, error: Option<&str>) {
        let snapshot = self.snapshot();
        let line = format_internal_prompt_progress_line(event, &snapshot, self.elapsed(), error);
        self.write_line(&line);
    }

    fn mark_model_phase(&self) {
        let snapshot = {
            let mut state = self
                .shared
                .state
                .lock()
                .expect("internal prompt progress state poisoned");
            state.step += 1;
            state.phase = if state.step == 1 {
                "analyzing request".to_string()
            } else {
                "reviewing findings".to_string()
            };
            state.detail = Some(format!("task: {}", state.task_label));
            state.clone()
        };
        self.write_line(&format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Update,
            &snapshot,
            self.elapsed(),
            None,
        ));
    }

    fn mark_tool_phase(&self, name: &str, input: &str) {
        let detail = describe_tool_progress(name, input);
        let snapshot = {
            let mut state = self
                .shared
                .state
                .lock()
                .expect("internal prompt progress state poisoned");
            state.step += 1;
            state.phase = format!("running {name}");
            state.detail = Some(detail);
            state.clone()
        };
        self.write_line(&format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Update,
            &snapshot,
            self.elapsed(),
            None,
        ));
    }

    fn mark_text_phase(&self, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let detail = truncate_for_summary(first_visible_line(trimmed), 120);
        let snapshot = {
            let mut state = self
                .shared
                .state
                .lock()
                .expect("internal prompt progress state poisoned");
            if state.saw_final_text {
                return;
            }
            state.saw_final_text = true;
            state.step += 1;
            state.phase = "drafting final plan".to_string();
            state.detail = (!detail.is_empty()).then_some(detail);
            state.clone()
        };
        self.write_line(&format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Update,
            &snapshot,
            self.elapsed(),
            None,
        ));
    }

    fn emit_heartbeat(&self) {
        let snapshot = self.snapshot();
        self.write_line(&format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Heartbeat,
            &snapshot,
            self.elapsed(),
            None,
        ));
    }

    fn snapshot(&self) -> InternalPromptProgressState {
        self.shared
            .state
            .lock()
            .expect("internal prompt progress state poisoned")
            .clone()
    }

    fn elapsed(&self) -> Duration {
        self.shared.started_at.elapsed()
    }

    fn write_line(&self, line: &str) {
        let _guard = self
            .shared
            .output_lock
            .lock()
            .expect("internal prompt progress output lock poisoned");
        let mut stdout = io::stdout();
        let _ = writeln!(stdout, "{line}");
        let _ = stdout.flush();
    }
}

impl InternalPromptProgressRun {
    fn start_ultraplan(task: &str) -> Self {
        let reporter = InternalPromptProgressReporter::ultraplan(task);
        reporter.emit(InternalPromptProgressEvent::Started, None);

        let (heartbeat_stop, heartbeat_rx) = mpsc::channel();
        let heartbeat_reporter = reporter.clone();
        let heartbeat_handle = thread::spawn(move || loop {
            match heartbeat_rx.recv_timeout(INTERNAL_PROGRESS_HEARTBEAT_INTERVAL) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => heartbeat_reporter.emit_heartbeat(),
            }
        });

        Self {
            reporter,
            heartbeat_stop: Some(heartbeat_stop),
            heartbeat_handle: Some(heartbeat_handle),
        }
    }

    fn reporter(&self) -> InternalPromptProgressReporter {
        self.reporter.clone()
    }

    fn finish_success(&mut self) {
        self.stop_heartbeat();
        self.reporter
            .emit(InternalPromptProgressEvent::Complete, None);
    }

    fn finish_failure(&mut self, error: &str) {
        self.stop_heartbeat();
        self.reporter
            .emit(InternalPromptProgressEvent::Failed, Some(error));
    }

    fn stop_heartbeat(&mut self) {
        if let Some(sender) = self.heartbeat_stop.take() {
            let _ = sender.send(());
        }
        if let Some(handle) = self.heartbeat_handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for InternalPromptProgressRun {
    fn drop(&mut self) {
        self.stop_heartbeat();
    }
}

fn format_internal_prompt_progress_line(
    event: InternalPromptProgressEvent,
    snapshot: &InternalPromptProgressState,
    elapsed: Duration,
    error: Option<&str>,
) -> String {
    let elapsed_seconds = elapsed.as_secs();
    let step_label = if snapshot.step == 0 {
        "current step pending".to_string()
    } else {
        format!("current step {}", snapshot.step)
    };
    let mut status_bits = vec![step_label, format!("phase {}", snapshot.phase)];
    if let Some(detail) = snapshot
        .detail
        .as_deref()
        .filter(|detail| !detail.is_empty())
    {
        status_bits.push(detail.to_string());
    }
    let status = status_bits.join(" · ");
    match event {
        InternalPromptProgressEvent::Started => {
            format!(
                "🧭 {} status · planning started · {status}",
                snapshot.command_label
            )
        }
        InternalPromptProgressEvent::Update => {
            format!("… {} status · {status}", snapshot.command_label)
        }
        InternalPromptProgressEvent::Heartbeat => format!(
            "… {} heartbeat · {elapsed_seconds}s elapsed · {status}",
            snapshot.command_label
        ),
        InternalPromptProgressEvent::Complete => format!(
            "✔ {} status · completed · {elapsed_seconds}s elapsed · {} steps total",
            snapshot.command_label, snapshot.step
        ),
        InternalPromptProgressEvent::Failed => format!(
            "✘ {} status · failed · {elapsed_seconds}s elapsed · {}",
            snapshot.command_label,
            error.unwrap_or("unknown error")
        ),
    }
}

fn describe_tool_progress(name: &str, input: &str) -> String {
    let parsed: serde_json::Value =
        serde_json::from_str(input).unwrap_or(serde_json::Value::String(input.to_string()));
    match name {
        "bash" | "Bash" => {
            let command = parsed
                .get("command")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            if command.is_empty() {
                "running shell command".to_string()
            } else {
                format!("command {}", truncate_for_summary(command.trim(), 100))
            }
        }
        "read_file" | "Read" => format!("reading {}", extract_tool_path(&parsed)),
        "write_file" | "Write" => format!("writing {}", extract_tool_path(&parsed)),
        "edit_file" | "Edit" => format!("editing {}", extract_tool_path(&parsed)),
        "glob_search" | "Glob" => {
            let pattern = parsed
                .get("pattern")
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            let scope = parsed
                .get("path")
                .and_then(|value| value.as_str())
                .unwrap_or(".");
            format!("glob `{pattern}` in {scope}")
        }
        "grep_search" | "Grep" => {
            let pattern = parsed
                .get("pattern")
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            let scope = parsed
                .get("path")
                .and_then(|value| value.as_str())
                .unwrap_or(".");
            format!("grep `{pattern}` in {scope}")
        }
        "web_search" | "WebSearch" => parsed
            .get("query")
            .and_then(|value| value.as_str())
            .map_or_else(
                || "running web search".to_string(),
                |query| format!("query {}", truncate_for_summary(query, 100)),
            ),
        _ => {
            let summary = summarize_tool_payload(input);
            if summary.is_empty() {
                format!("running {name}")
            } else {
                format!("{name}: {summary}")
            }
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
#[allow(clippy::too_many_arguments)]
fn build_runtime(
    session: Session,
    session_id: &str,
    model: String,
    system_prompt: Vec<String>,
    enable_tools: bool,
    emit_output: bool,
    stream_json: bool,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    progress_reporter: Option<InternalPromptProgressReporter>,
) -> Result<BuiltRuntime, Box<dyn std::error::Error>> {
    let runtime_plugin_state = build_runtime_plugin_state()?;
    build_runtime_with_plugin_state(
        session,
        session_id,
        model,
        system_prompt,
        enable_tools,
        emit_output,
        stream_json,
        allowed_tools,
        permission_mode,
        progress_reporter,
        runtime_plugin_state,
    )
}

#[allow(clippy::needless_pass_by_value)]
#[allow(clippy::too_many_arguments)]
fn build_runtime_with_plugin_state(
    mut session: Session,
    session_id: &str,
    model: String,
    system_prompt: Vec<String>,
    enable_tools: bool,
    emit_output: bool,
    stream_json: bool,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    progress_reporter: Option<InternalPromptProgressReporter>,
    runtime_plugin_state: RuntimePluginState,
) -> Result<BuiltRuntime, Box<dyn std::error::Error>> {
    // Persist the model in session metadata so resumed sessions can report it.
    if session.model.is_none() {
        session.model = Some(model.clone());
    }
    let RuntimePluginState {
        feature_config,
        mut tool_registry,
        plugin_registry,
        mcp_state,
    } = runtime_plugin_state;
    plugin_registry.initialize()?;
    let policy = permission_policy(permission_mode, &feature_config, &tool_registry)
        .map_err(std::io::Error::other)?;
    let workspace_root = session
        .workspace_root()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    tool_registry = tool_registry.with_enforcer(
        runtime::permission_enforcer::PermissionEnforcer::new(policy.clone()),
    );
    let mut runtime = ConversationRuntime::new_with_features(
        session,
        AnthropicRuntimeClient::new(
            session_id,
            model.clone(),
            enable_tools,
            emit_output,
            stream_json,
            allowed_tools.clone(),
            tool_registry.clone(),
            progress_reporter,
        )?,
        CliToolExecutor::new(
            allowed_tools.clone(),
            emit_output,
            stream_json,
            tool_registry.clone(),
            mcp_state.clone(),
        ),
        policy,
        system_prompt,
        &feature_config,
    );
    runtime = runtime.with_task_registry(load_task_registry()?);
    if stream_json {
        runtime = runtime.with_runtime_event_reporter(CliRuntimeEventReporter);
    } else if emit_output {
        // Interactive (rich) mode: surface runtime progress as compact colorized lines.
        runtime = runtime.with_runtime_event_reporter(CliRichRuntimeEventReporter);
    }
    let route_policy_dir = workspace_root.join(".Himalaya").join("routes");
    let mut model_routing_policy = feature_config.model_routing().to_policy(&model);
    if let Ok(Some(applied)) = runtime::load_applied_routing_policy(&route_policy_dir) {
        model_routing_policy = applied.policy;
    }
    runtime = runtime.with_model_router(runtime::ModelRouter::new(model_routing_policy));
    if let Ok(store) = load_route_feedback_store() {
        runtime = runtime.with_workspace_route_feedback(store.feedback().to_vec());
    }
    if emit_output {
        runtime = runtime.with_hook_progress_reporter(Box::new(CliHookProgressReporter));
    }
    Ok(BuiltRuntime::new(runtime, plugin_registry, mcp_state))
}

struct CliRuntimeEventReporter;

impl runtime::RuntimeEventReporter for CliRuntimeEventReporter {
    fn emit_runtime_event(&self, event: &runtime::RuntimeEvent) {
        let envelope = runtime::RuntimeEventEnvelope::from_runtime_event(event).ok();
        if let Some(envelope) = envelope.as_ref() {
            persist_cli_runtime_event(envelope);
        }
        match event {
            runtime::RuntimeEvent::Decisioning(value) => {
                let mut payload = json!({
                    "type": "decisioning_event",
                    "decisioning_event": value,
                });
                attach_runtime_event_envelope(&mut payload, envelope.as_ref());
                print_stream_json_event(payload);
            }
            runtime::RuntimeEvent::PlanExecution(value) => {
                let mut payload = json!({
                    "type": "plan_execution_event",
                    "plan_execution_event": value,
                });
                attach_runtime_event_envelope(&mut payload, envelope.as_ref());
                print_stream_json_event(payload);
            }
            runtime::RuntimeEvent::TaskLedger(value) => {
                let mut payload = json!({
                    "type": "task_ledger_event",
                    "task_ledger_event": value,
                });
                attach_runtime_event_envelope(&mut payload, envelope.as_ref());
                print_stream_json_event(payload);
            }
            runtime::RuntimeEvent::ModelRoute(value) => {
                let mut payload = json!({
                    "type": "model_route_event",
                    "model_route_event": value,
                });
                attach_runtime_event_envelope(&mut payload, envelope.as_ref());
                print_stream_json_event(payload);
            }
            runtime::RuntimeEvent::TeamExecution(value) => {
                let mut payload = json!({
                    "type": "team_execution_event",
                    "team_execution_event": value,
                });
                attach_runtime_event_envelope(&mut payload, envelope.as_ref());
                print_stream_json_event(payload);
            }
            runtime::RuntimeEvent::Recovery(value) => {
                let mut payload = json!({
                    "type": "recovery_event",
                    "recovery_event": value,
                });
                attach_runtime_event_envelope(&mut payload, envelope.as_ref());
                print_stream_json_event(payload);
            }
            runtime::RuntimeEvent::RecoveryAction(value) => {
                let mut payload = json!({
                    "type": "recovery_action_event",
                    "recovery_action_event": value,
                });
                attach_runtime_event_envelope(&mut payload, envelope.as_ref());
                print_stream_json_event(payload);
            }
            runtime::RuntimeEvent::TaskExecution(value) => {
                let mut payload = json!({
                    "type": "task_execution_event",
                    "task_execution_event": value,
                });
                attach_runtime_event_envelope(&mut payload, envelope.as_ref());
                print_stream_json_event(payload);
            }
            runtime::RuntimeEvent::TaskExecutionReport(value) => {
                let mut payload = json!({
                    "type": "task_execution_report_event",
                    "task_execution_report_event": value,
                    "outcome": value.outcome,
                    "report": value,
                });
                attach_runtime_event_envelope(&mut payload, envelope.as_ref());
                print_stream_json_event(payload);
            }
        }
    }
}

/// Reporter for the interactive (rich) mode: renders runtime events as compact, colorized
/// terminal lines on stderr instead of raw stream-json. Still persists the event log.
struct CliRichRuntimeEventReporter;

impl runtime::RuntimeEventReporter for CliRichRuntimeEventReporter {
    fn emit_runtime_event(&self, event: &runtime::RuntimeEvent) {
        if let Ok(envelope) = runtime::RuntimeEventEnvelope::from_runtime_event(event) {
            persist_cli_runtime_event(&envelope);
        }
        let kind = event.event_type();
        let value = match event {
            runtime::RuntimeEvent::Decisioning(_)
            | runtime::RuntimeEvent::TaskExecutionReport(_) => return,
            runtime::RuntimeEvent::PlanExecution(v) => serde_json::to_value(v).ok(),
            runtime::RuntimeEvent::TaskLedger(v) => serde_json::to_value(v).ok(),
            runtime::RuntimeEvent::ModelRoute(v) => serde_json::to_value(v).ok(),
            runtime::RuntimeEvent::TeamExecution(v) => serde_json::to_value(v).ok(),
            runtime::RuntimeEvent::Recovery(v) => serde_json::to_value(v).ok(),
            runtime::RuntimeEvent::RecoveryAction(v) => serde_json::to_value(v).ok(),
            runtime::RuntimeEvent::TaskExecution(v) => serde_json::to_value(v).ok(),
        };
        if let Some(value) = value {
            if let Some(line) = format_runtime_event_line(kind, &value) {
                eprintln!("{line}");
            }
        }
    }
}

fn runtime_event_log_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    Ok(cwd.join(".Himalaya").join("events").join("runtime.jsonl"))
}

fn attach_runtime_event_envelope(
    payload: &mut Value,
    envelope: Option<&runtime::RuntimeEventEnvelope>,
) {
    let Some(envelope) = envelope else {
        return;
    };
    if let Value::Object(object) = payload {
        object.insert("event".to_string(), json!(envelope));
    }
}

fn persist_cli_runtime_event(envelope: &runtime::RuntimeEventEnvelope) {
    let Ok(path) = runtime_event_log_path() else {
        return;
    };
    if let Err(error) = runtime::append_runtime_event_log(&path, envelope) {
        eprintln!(
            "warning: failed to persist runtime event log at {}: {error}",
            path.display()
        );
    }
}

struct CliDecisioningEventReporter;

impl runtime::DecisioningEventReporter for CliDecisioningEventReporter {
    fn emit_decisioning_event(&self, event: &runtime::DecisioningEvent) {
        print_stream_json_event(json!({
            "type": "decisioning_event",
            "decisioning_event": event,
        }));
    }
}

struct CliPlanExecutionEventReporter;

impl runtime::PlanExecutionEventReporter for CliPlanExecutionEventReporter {
    fn emit_plan_execution_event(&self, event: &runtime::PlanExecutionEvent) {
        print_stream_json_event(json!({
            "type": "plan_execution_event",
            "plan_execution_event": event,
        }));
    }
}

struct CliHookProgressReporter;

impl runtime::HookProgressReporter for CliHookProgressReporter {
    fn on_event(&mut self, event: &runtime::HookProgressEvent) {
        match event {
            runtime::HookProgressEvent::Started {
                event,
                tool_name,
                command,
            } => eprintln!(
                "[hook {event_name}] {tool_name}: {command}",
                event_name = event.as_str()
            ),
            runtime::HookProgressEvent::Completed {
                event,
                tool_name,
                command,
            } => eprintln!(
                "[hook done {event_name}] {tool_name}: {command}",
                event_name = event.as_str()
            ),
            runtime::HookProgressEvent::Cancelled {
                event,
                tool_name,
                command,
            } => eprintln!(
                "[hook cancelled {event_name}] {tool_name}: {command}",
                event_name = event.as_str()
            ),
        }
    }
}

struct CliPermissionPrompter {
    current_mode: PermissionMode,
    stream_json: bool,
    /// Tools the host approved "always" for this session (stream-json mode).
    session_allowed_tools: std::collections::HashSet<String>,
}

impl CliPermissionPrompter {
    fn new(current_mode: PermissionMode) -> Self {
        Self::new_with_stream_json(current_mode, false)
    }

    fn new_with_stream_json(current_mode: PermissionMode, stream_json: bool) -> Self {
        Self {
            current_mode,
            stream_json,
            session_allowed_tools: std::collections::HashSet::new(),
        }
    }
}

impl runtime::PermissionPrompter for CliPermissionPrompter {
    fn notify_request(&mut self, request: &runtime::PermissionRequest) {
        if self.stream_json {
            print_stream_json_event(permission_request_event(request));
        }
    }

    fn decide(
        &mut self,
        request: &runtime::PermissionRequest,
    ) -> runtime::PermissionPromptDecision {
        if self.stream_json {
            // Host-driven approval (VS Code): a tool that needs elevation emits a
            // `permission_request` event; the host replies with a single line
            //   {"type":"permission_response","decision":"allow|deny|allow_always"}
            // read from stdin. This closes the loop so writes/installs are no
            // longer auto-denied. Missing/garbled replies fall back to Deny.
            if self.session_allowed_tools.contains(&request.tool_name) {
                return runtime::PermissionPromptDecision::Allow;
            }
            let deny_reason = || {
                request.reason.clone().unwrap_or_else(|| {
                    format!(
                        "tool '{}' requires {} permission; current mode is {}",
                        request.tool_name,
                        request.required_mode.as_str(),
                        request.current_mode.as_str()
                    )
                })
            };
            let mut response = String::new();
            match std::io::stdin().read_line(&mut response) {
                Ok(0) | Err(_) => {
                    return runtime::PermissionPromptDecision::Deny {
                        reason: deny_reason(),
                    };
                }
                Ok(_) => {}
            }
            let decision = serde_json::from_str::<serde_json::Value>(response.trim())
                .ok()
                .and_then(|value| {
                    value
                        .get("decision")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| response.trim().to_string());
            return match decision.as_str() {
                "allow" | "allow_once" | "yes" | "y" => runtime::PermissionPromptDecision::Allow,
                "allow_always" | "allow_session" => {
                    self.session_allowed_tools.insert(request.tool_name.clone());
                    runtime::PermissionPromptDecision::Allow
                }
                _ => runtime::PermissionPromptDecision::Deny {
                    reason: deny_reason(),
                },
            };
        }

        println!();
        println!("Permission approval required");
        println!("  Tool             {}", request.tool_name);
        println!("  Current mode     {}", self.current_mode.as_str());
        println!("  Required mode    {}", request.required_mode.as_str());
        if let Some(reason) = &request.reason {
            println!("  Reason           {reason}");
        }
        println!("  Input            {}", request.input);
        print!("Approve this tool call? [y/N]: ");
        let _ = io::stdout().flush();

        let mut response = String::new();
        match io::stdin().read_line(&mut response) {
            Ok(_) => {
                let normalized = response.trim().to_ascii_lowercase();
                if matches!(normalized.as_str(), "y" | "yes") {
                    runtime::PermissionPromptDecision::Allow
                } else {
                    runtime::PermissionPromptDecision::Deny {
                        reason: format!(
                            "tool '{}' denied by user approval prompt",
                            request.tool_name
                        ),
                    }
                }
            }
            Err(error) => runtime::PermissionPromptDecision::Deny {
                reason: format!("permission approval failed: {error}"),
            },
        }
    }
}

// NOTE: Despite the historical name `AnthropicRuntimeClient`, this struct
// now holds an `ApiProviderClient` which dispatches to Anthropic, xAI,
// OpenAI, or DashScope at construction time based on
// `detect_provider_kind(&model)`. The struct name is kept to avoid
// churning `BuiltRuntime` and every Deref/DerefMut site that references
// it. See ROADMAP #29 for the provider-dispatch routing fix.
struct AnthropicRuntimeClient {
    runtime: tokio::runtime::Runtime,
    client: ApiProviderClient,
    session_id: String,
    model: String,
    enable_tools: bool,
    emit_output: bool,
    stream_json: bool,
    allowed_tools: Option<AllowedToolSet>,
    tool_registry: GlobalToolRegistry,
    progress_reporter: Option<InternalPromptProgressReporter>,
    reasoning_effort: Option<String>,
}

impl AnthropicRuntimeClient {
    #[allow(clippy::too_many_arguments)]
    fn new(
        session_id: &str,
        model: String,
        enable_tools: bool,
        emit_output: bool,
        stream_json: bool,
        allowed_tools: Option<AllowedToolSet>,
        tool_registry: GlobalToolRegistry,
        progress_reporter: Option<InternalPromptProgressReporter>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Dispatch to the correct provider at construction time.
        // `ApiProviderClient` (exposed by the api crate as
        // `ProviderClient`) is an enum over Anthropic / xAI / OpenAI
        // variants, where xAI and OpenAI both use the OpenAI-compat
        // wire format under the hood. We consult
        // `detect_provider_kind(&resolved_model)` so model-name prefix
        // routing (`openai/`, `gpt-`, `grok`, `qwen/`) wins over
        // env-var presence.
        //
        // For Anthropic we build the client directly instead of going
        // through `ApiProviderClient::from_model_with_anthropic_auth`
        // so we can explicitly apply `api::read_base_url()` — that
        // reads `ANTHROPIC_BASE_URL` and is required for the local
        // mock-server test harness
        // (`crates/rusty-Himalaya-cli/tests/compact_output.rs`) to point
        // Himalaya at its fake Anthropic endpoint. We also attach a
        // session-scoped prompt cache on the Anthropic path; the
        // prompt cache is Anthropic-only so non-Anthropic variants
        // skip it.
        let resolved_model = api::resolve_model_alias(&model);
        let client = match detect_provider_kind(&resolved_model) {
            ProviderKind::Anthropic => {
                let auth = resolve_cli_auth_source()?;
                let inner = AnthropicClient::from_auth(auth)
                    .with_base_url(api::read_base_url())
                    .with_prompt_cache(PromptCache::new(session_id));
                ApiProviderClient::Anthropic(inner)
            }
            ProviderKind::Xai | ProviderKind::OpenAi => {
                // The api crate's `ProviderClient::from_model_with_anthropic_auth`
                // with `None` for the anthropic auth routes via
                // `detect_provider_kind` and builds an
                // `OpenAiCompatClient::from_env` with the matching
                // `OpenAiCompatConfig` (openai / xai / dashscope).
                // That reads the correct API-key env var and BASE_URL
                // override internally, so this one call covers OpenAI,
                // OpenRouter, xAI, DashScope, Ollama, and any other
                // OpenAI-compat endpoint users configure via
                // `OPENAI_BASE_URL` / `XAI_BASE_URL` / `DASHSCOPE_BASE_URL`.
                ApiProviderClient::from_model_with_anthropic_auth(&resolved_model, None)?
            }
        };
        Ok(Self {
            runtime: tokio::runtime::Runtime::new()?,
            client,
            session_id: session_id.to_string(),
            model,
            enable_tools,
            emit_output,
            stream_json,
            allowed_tools,
            tool_registry,
            progress_reporter,
            reasoning_effort: None,
        })
    }

    fn set_reasoning_effort(&mut self, effort: Option<String>) {
        self.reasoning_effort = effort;
    }
}

fn resolve_cli_auth_source() -> Result<AuthSource, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    Ok(resolve_cli_auth_source_for_cwd(&cwd, default_oauth_config)?)
}

fn resolve_cli_auth_source_for_cwd<F>(
    cwd: &Path,
    default_oauth: F,
) -> Result<AuthSource, api::ApiError>
where
    F: FnOnce() -> OAuthConfig,
{
    resolve_startup_auth_source(|| {
        Ok(Some(
            load_runtime_oauth_config_for(cwd)?.unwrap_or_else(default_oauth),
        ))
    })
}

fn load_runtime_oauth_config_for(cwd: &Path) -> Result<Option<OAuthConfig>, api::ApiError> {
    let config = ConfigLoader::default_for(cwd).load().map_err(|error| {
        api::ApiError::Auth(format!("failed to load runtime OAuth config: {error}"))
    })?;
    Ok(config.oauth().cloned())
}

impl AnthropicRuntimeClient {
    fn client_for_routed_model(
        &self,
        routed_model: &str,
    ) -> Result<ApiProviderClient, RuntimeError> {
        let resolved_current = api::resolve_model_alias(&self.model);
        let resolved_routed = api::resolve_model_alias(routed_model);
        if resolved_current == resolved_routed {
            return Ok(self.client.clone());
        }
        match detect_provider_kind(&resolved_routed) {
            ProviderKind::Anthropic => {
                let auth = resolve_cli_auth_source()
                    .map_err(|error| RuntimeError::new(error.to_string()))?;
                let inner = AnthropicClient::from_auth(auth)
                    .with_base_url(api::read_base_url())
                    .with_prompt_cache(PromptCache::new(&self.session_id));
                Ok(ApiProviderClient::Anthropic(inner))
            }
            ProviderKind::Xai | ProviderKind::OpenAi => {
                ApiProviderClient::from_model_with_anthropic_auth(&resolved_routed, None)
                    .map_err(|error| RuntimeError::new(error.to_string()))
            }
        }
    }
}

impl ApiClient for AnthropicRuntimeClient {
    #[allow(clippy::too_many_lines)]
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        if let Some(progress_reporter) = &self.progress_reporter {
            progress_reporter.mark_model_phase();
        }
        let is_post_tool = request_ends_with_tool_result(&request);
        let routed_model = request
            .model_route
            .as_ref()
            .map(|route| route.model.as_str())
            .unwrap_or(&self.model);
        let routed_client = self.client_for_routed_model(routed_model)?;
        let message_request = MessageRequest {
            model: routed_model.to_string(),
            max_tokens: max_tokens_for_model(routed_model),
            messages: convert_messages(&request.messages),
            system: (!request.system_prompt.is_empty()).then(|| request.system_prompt.join("\n\n")),
            tools: self
                .enable_tools
                .then(|| filter_tool_specs(&self.tool_registry, self.allowed_tools.as_ref())),
            tool_choice: self.enable_tools.then_some(ToolChoice::Auto),
            stream: true,
            reasoning_effort: self.reasoning_effort.clone(),
            ..Default::default()
        };

        self.runtime.block_on(async {
            // When resuming after tool execution, apply a stall timeout on the
            // first stream event.  If the model does not respond within the
            // deadline we drop the stalled connection and re-send the request as
            // a continuation nudge (one retry only).
            let max_attempts: usize = if is_post_tool { 2 } else { 1 };

            for attempt in 1..=max_attempts {
                let result = self
                    .consume_stream(
                        &routed_client,
                        &message_request,
                        is_post_tool && attempt == 1,
                    )
                    .await;
                match result {
                    Ok(events) => return Ok(events),
                    Err(error)
                        if error.to_string().contains("post-tool stall")
                            && attempt < max_attempts =>
                    {
                        // Stalled after tool completion — nudge the model by
                        // re-sending the same request.
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            }

            Err(RuntimeError::new("post-tool continuation nudge exhausted"))
        })
    }
}

impl AnthropicRuntimeClient {
    /// Consume a single streaming response, optionally applying a stall
    /// timeout on the first event for post-tool continuations.
    #[allow(clippy::too_many_lines)]
    async fn consume_stream(
        &self,
        client: &ApiProviderClient,
        message_request: &MessageRequest,
        apply_stall_timeout: bool,
    ) -> Result<Vec<AssistantEvent>, RuntimeError> {
        let mut stream = client
            .stream_message(message_request)
            .await
            .map_err(|error| {
                RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
            })?;
        let mut stdout = io::stdout();
        let mut sink = io::sink();
        let out: &mut dyn Write = if self.emit_output {
            &mut stdout
        } else {
            &mut sink
        };
        let renderer = TerminalRenderer::new();
        let mut markdown_stream = MarkdownStreamState::default();
        let mut events = Vec::new();
        let mut pending_tool: Option<(String, String, String)> = None;
        // (index, thinking_text, signature) for streaming thinking blocks
        let mut pending_thinking: Option<(u32, String, Option<String>)> = None;
        let mut block_has_thinking_summary = false;
        let mut saw_stop = false;
        let mut received_any_event = false;

        loop {
            let next = if apply_stall_timeout && !received_any_event {
                match tokio::time::timeout(POST_TOOL_STALL_TIMEOUT, stream.next_event()).await {
                    Ok(inner) => inner.map_err(|error| {
                        RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
                    })?,
                    Err(_elapsed) => {
                        return Err(RuntimeError::new(
                            "post-tool stall: model did not respond within timeout",
                        ));
                    }
                }
            } else {
                stream.next_event().await.map_err(|error| {
                    RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
                })?
            };

            let Some(event) = next else {
                break;
            };
            received_any_event = true;

            match event {
                ApiStreamEvent::MessageStart(start) => {
                    for block in start.message.content {
                        push_output_block(
                            block,
                            out,
                            &mut events,
                            &mut pending_tool,
                            true,
                            &mut block_has_thinking_summary,
                            self.stream_json,
                        )?;
                    }
                }
                ApiStreamEvent::ContentBlockStart(start) => {
                    match &start.content_block {
                        OutputContentBlock::Thinking { .. } => {
                            // Register for delta accumulation; summary rendered on first delta
                            pending_thinking = Some((start.index, String::new(), None));
                        }
                        OutputContentBlock::RedactedThinking { .. } => {
                            push_output_block(
                                start.content_block,
                                out,
                                &mut events,
                                &mut pending_tool,
                                true,
                                &mut block_has_thinking_summary,
                                self.stream_json,
                            )?;
                        }
                        _ => {
                            push_output_block(
                                start.content_block,
                                out,
                                &mut events,
                                &mut pending_tool,
                                true,
                                &mut block_has_thinking_summary,
                                self.stream_json,
                            )?;
                        }
                    }
                }
                ApiStreamEvent::ContentBlockDelta(delta) => match delta.delta {
                    ContentBlockDelta::TextDelta { text } => {
                        if !text.is_empty() {
                            if let Some(progress_reporter) = &self.progress_reporter {
                                progress_reporter.mark_text_phase(&text);
                            }
                            if self.stream_json {
                                print_stream_json_event(json!({"type":"text_delta","text":text}));
                            } else if let Some(rendered) = markdown_stream.push(&renderer, &text) {
                                write!(out, "{rendered}")
                                    .and_then(|()| out.flush())
                                    .map_err(|error| RuntimeError::new(error.to_string()))?;
                            }
                            events.push(AssistantEvent::TextDelta(text));
                        }
                    }
                    ContentBlockDelta::InputJsonDelta { partial_json } => {
                        if let Some((_, _, input)) = &mut pending_tool {
                            input.push_str(&partial_json);
                        }
                    }
                    ContentBlockDelta::ThinkingDelta { thinking } => {
                        if let Some((_, text, _)) = &mut pending_thinking {
                            text.push_str(&thinking);
                        }
                        if !block_has_thinking_summary {
                            render_thinking_block_summary(out, None, false)?;
                            block_has_thinking_summary = true;
                        }
                    }
                    ContentBlockDelta::SignatureDelta { signature } => {
                        if let Some((_, _, sig)) = &mut pending_thinking {
                            *sig = Some(signature);
                        }
                    }
                },
                ApiStreamEvent::ContentBlockStop(stop) => {
                    block_has_thinking_summary = false;
                    if let Some(rendered) = markdown_stream.flush(&renderer) {
                        write!(out, "{rendered}")
                            .and_then(|()| out.flush())
                            .map_err(|error| RuntimeError::new(error.to_string()))?;
                    }
                    // Commit accumulated thinking block
                    if let Some((idx, thinking, signature)) = pending_thinking.take() {
                        if idx == stop.index {
                            events.push(AssistantEvent::ReasoningStep(ReasoningStep::Analysis {
                                content: thinking.clone(),
                                confidence: None,
                                signature: signature.clone(),
                            }));
                            if self.stream_json {
                                let mut step = serde_json::json!({
                                    "step_type": "analysis",
                                    "content": thinking,
                                });
                                if let Some(sig) = signature {
                                    step["signature"] = serde_json::json!(sig);
                                }
                                print_stream_json_event(
                                    json!({"type":"reasoning_step","reasoning_step":step}),
                                );
                            }
                        }
                    }
                    if let Some((id, name, input)) = pending_tool.take() {
                        if let Some(progress_reporter) = &self.progress_reporter {
                            progress_reporter.mark_tool_phase(&name, &input);
                        }
                        if self.stream_json {
                            let input_val: serde_json::Value = serde_json::from_str(&input)
                                .unwrap_or(serde_json::Value::String(input.clone()));
                            print_stream_json_event(
                                json!({"type":"tool_use","id":id,"name":name,"input":input_val}),
                            );
                        } else {
                            // Display tool call now that input is fully accumulated
                            writeln!(out, "\n{}", format_tool_call_start(&name, &input))
                                .and_then(|()| out.flush())
                                .map_err(|error| RuntimeError::new(error.to_string()))?;
                        }
                        events.push(AssistantEvent::ToolUse { id, name, input });
                    }
                }
                ApiStreamEvent::MessageDelta(delta) => {
                    events.push(AssistantEvent::Usage(delta.usage.token_usage()));
                }
                ApiStreamEvent::MessageStop(_) => {
                    saw_stop = true;
                    if let Some(rendered) = markdown_stream.flush(&renderer) {
                        write!(out, "{rendered}")
                            .and_then(|()| out.flush())
                            .map_err(|error| RuntimeError::new(error.to_string()))?;
                    }
                    events.push(AssistantEvent::MessageStop);
                }
            }
        }

        push_prompt_cache_record(&self.client, &mut events);

        if !saw_stop && assistant_events_have_content(&events) {
            events.push(AssistantEvent::MessageStop);
        }

        if saw_stop && assistant_events_have_content(&events) {
            return Ok(events);
        }

        let response = self
            .client
            .send_message(&MessageRequest {
                stream: false,
                ..message_request.clone()
            })
            .await
            .map_err(|error| {
                RuntimeError::new(format_user_visible_api_error(&self.session_id, &error))
            })?;
        let mut events = response_to_events(response, out, self.stream_json)?;
        push_prompt_cache_record(&self.client, &mut events);
        if !assistant_events_have_content(&events) {
            return Err(RuntimeError::new(
                "provider returned an empty assistant response after streaming and non-streaming retry",
            ));
        }
        Ok(events)
    }
}

/// Returns `true` when the conversation ends with a tool-result message,
/// meaning the model is expected to continue after tool execution.
fn request_ends_with_tool_result(request: &ApiRequest) -> bool {
    request
        .messages
        .last()
        .is_some_and(|message| message.role == MessageRole::Tool)
}

fn format_user_visible_api_error(session_id: &str, error: &api::ApiError) -> String {
    if error.is_context_window_failure() {
        format_context_window_blocked_error(session_id, error)
    } else if error.is_generic_fatal_wrapper() {
        let mut qualifiers = vec![format!("session {session_id}")];
        if let Some(request_id) = error.request_id() {
            qualifiers.push(format!("trace {request_id}"));
        }
        format!(
            "{} ({}): {}",
            error.safe_failure_class(),
            qualifiers.join(", "),
            error
        )
    } else {
        error.to_string()
    }
}

fn format_context_window_blocked_error(session_id: &str, error: &api::ApiError) -> String {
    let mut lines = vec![
        "Context window blocked".to_string(),
        "  Failure class    context_window_blocked".to_string(),
        format!("  Session          {session_id}"),
    ];

    if let Some(request_id) = error.request_id() {
        lines.push(format!("  Trace            {request_id}"));
    }

    match error {
        api::ApiError::ContextWindowExceeded {
            model,
            estimated_input_tokens,
            requested_output_tokens,
            estimated_total_tokens,
            context_window_tokens,
        } => {
            lines.push(format!("  Model            {model}"));
            lines.push(format!(
                "  Input estimate   ~{estimated_input_tokens} tokens (heuristic)"
            ));
            lines.push(format!(
                "  Requested output {requested_output_tokens} tokens"
            ));
            lines.push(format!(
                "  Total estimate   ~{estimated_total_tokens} tokens (heuristic)"
            ));
            lines.push(format!("  Context window   {context_window_tokens} tokens"));
        }
        api::ApiError::Api { message, body, .. } => {
            let detail = message.as_deref().unwrap_or(body).trim();
            if !detail.is_empty() {
                lines.push(format!(
                    "  Detail           {}",
                    truncate_for_summary(detail, 120)
                ));
            }
        }
        api::ApiError::RetriesExhausted { last_error, .. } => {
            let detail = match last_error.as_ref() {
                api::ApiError::Api { message, body, .. } => message.as_deref().unwrap_or(body),
                other => return format_context_window_blocked_error(session_id, other),
            }
            .trim();
            if !detail.is_empty() {
                lines.push(format!(
                    "  Detail           {}",
                    truncate_for_summary(detail, 120)
                ));
            }
        }
        _ => {}
    }

    lines.push(String::new());
    lines.push("Recovery".to_string());
    lines.push("  Compact          /compact".to_string());
    lines.push(format!(
        "  Resume compact   Himalaya --resume {session_id} /compact"
    ));
    lines.push("  Fresh session    /clear --confirm".to_string());
    lines.push(
        "  Reduce scope     remove large pasted context/files or ask for a smaller slice"
            .to_string(),
    );
    lines.push("  Retry            rerun after compacting or reducing the request".to_string());

    lines.join("\n")
}

fn final_assistant_text(summary: &runtime::TurnSummary) -> String {
    summary
        .assistant_messages
        .last()
        .map(|message| {
            message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

fn collect_tool_uses(summary: &runtime::TurnSummary) -> Vec<serde_json::Value> {
    summary
        .assistant_messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, input } => Some(json!({
                "id": id,
                "name": name,
                "input": input,
            })),
            _ => None,
        })
        .collect()
}

fn collect_tool_results(summary: &runtime::TurnSummary) -> Vec<serde_json::Value> {
    summary
        .tool_results
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                tool_name,
                output,
                is_error,
            } => Some(json!({
                "tool_use_id": tool_use_id,
                "tool_name": tool_name,
                "output": output,
                "is_error": is_error,
            })),
            _ => None,
        })
        .collect()
}

fn collect_prompt_cache_events(summary: &runtime::TurnSummary) -> Vec<serde_json::Value> {
    summary
        .prompt_cache_events
        .iter()
        .map(|event| {
            json!({
                "unexpected": event.unexpected,
                "reason": event.reason,
                "previous_cache_read_input_tokens": event.previous_cache_read_input_tokens,
                "current_cache_read_input_tokens": event.current_cache_read_input_tokens,
                "token_drop": event.token_drop,
            })
        })
        .collect()
}

fn slash_command_completion_candidates_with_sessions(
    model: &str,
    active_session_id: Option<&str>,
    recent_session_ids: Vec<String>,
) -> Vec<String> {
    let mut completions = BTreeSet::new();

    for spec in slash_command_specs() {
        if is_stub_slash_command(spec.name) {
            continue;
        }
        completions.insert(format!("/{}", spec.name));
        for alias in spec.aliases {
            if !is_stub_slash_command(alias) {
                completions.insert(format!("/{alias}"));
            }
        }
    }

    for candidate in [
        "/bughunter ",
        "/clear --confirm",
        "/config ",
        "/config env",
        "/config hooks",
        "/config model",
        "/config plugins",
        "/mcp ",
        "/mcp list",
        "/mcp show ",
        "/export ",
        "/issue ",
        "/model ",
        "/model opus",
        "/model sonnet",
        "/model haiku",
        "/permissions ",
        "/permissions read-only",
        "/permissions workspace-write",
        "/permissions danger-full-access",
        "/plugin list",
        "/plugin install ",
        "/plugin enable ",
        "/plugin disable ",
        "/plugin uninstall ",
        "/plugin update ",
        "/plugins list",
        "/pr ",
        "/resume ",
        "/session list",
        "/session switch ",
        "/session fork ",
        "/teleport ",
        "/ultraplan ",
        "/agents help",
        "/mcp help",
        "/skills help",
    ] {
        completions.insert(candidate.to_string());
    }

    if !model.trim().is_empty() {
        completions.insert(format!("/model {}", resolve_model_alias(model)));
        completions.insert(format!("/model {model}"));
    }
    completions.insert("/model wizard".to_string());

    if let Some(active_session_id) = active_session_id.filter(|value| !value.trim().is_empty()) {
        completions.insert(format!("/resume {active_session_id}"));
        completions.insert(format!("/session switch {active_session_id}"));
    }

    for session_id in recent_session_ids
        .into_iter()
        .filter(|value| !value.trim().is_empty())
        .take(10)
    {
        completions.insert(format!("/resume {session_id}"));
        completions.insert(format!("/session switch {session_id}"));
    }

    completions.into_iter().collect()
}

fn format_tool_call_start(name: &str, input: &str) -> String {
    let parsed: serde_json::Value =
        serde_json::from_str(input).unwrap_or(serde_json::Value::String(input.to_string()));

    let detail = match name {
        "bash" | "Bash" => format_bash_call(&parsed),
        "read_file" | "Read" => {
            let path = extract_tool_path(&parsed);
            format!("\x1b[2m📄 Reading {path}…\x1b[0m")
        }
        "write_file" | "Write" => {
            let path = extract_tool_path(&parsed);
            let lines = parsed
                .get("content")
                .and_then(|value| value.as_str())
                .map_or(0, |content| content.lines().count());
            format!("\x1b[1;32m✏️ Writing {path}\x1b[0m \x1b[2m({lines} lines)\x1b[0m")
        }
        "edit_file" | "Edit" => {
            let path = extract_tool_path(&parsed);
            let old_value = parsed
                .get("old_string")
                .or_else(|| parsed.get("oldString"))
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            let new_value = parsed
                .get("new_string")
                .or_else(|| parsed.get("newString"))
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            format!(
                "\x1b[1;33m📝 Editing {path}\x1b[0m{}",
                format_patch_preview(old_value, new_value)
                    .map(|preview| format!("\n{preview}"))
                    .unwrap_or_default()
            )
        }
        "glob_search" | "Glob" => format_search_start("🔎 Glob", &parsed),
        "grep_search" | "Grep" => format_search_start("🔎 Grep", &parsed),
        "web_search" | "WebSearch" => parsed
            .get("query")
            .and_then(|value| value.as_str())
            .unwrap_or("?")
            .to_string(),
        _ => summarize_tool_payload(input),
    };

    let border = "─".repeat(name.len() + 8);
    format!(
        "\x1b[38;5;245m╭─ \x1b[1;36m{name}\x1b[0;38;5;245m ─╮\x1b[0m\n\x1b[38;5;245m│\x1b[0m {detail}\n\x1b[38;5;245m╰{border}╯\x1b[0m"
    )
}

fn format_tool_result(name: &str, output: &str, is_error: bool) -> String {
    let icon = if is_error {
        "\x1b[1;31m✗\x1b[0m"
    } else {
        "\x1b[1;32m✓\x1b[0m"
    };
    if is_error {
        let summary = truncate_for_summary(output.trim(), 160);
        return if summary.is_empty() {
            format!("{icon} \x1b[38;5;245m{name}\x1b[0m")
        } else {
            format!("{icon} \x1b[38;5;245m{name}\x1b[0m\n\x1b[38;5;203m{summary}\x1b[0m")
        };
    }

    let parsed: serde_json::Value =
        serde_json::from_str(output).unwrap_or(serde_json::Value::String(output.to_string()));
    match name {
        "bash" | "Bash" => format_bash_result(icon, &parsed),
        "read_file" | "Read" => format_read_result(icon, &parsed),
        "write_file" | "Write" => format_write_result(icon, &parsed),
        "edit_file" | "Edit" => format_edit_result(icon, &parsed),
        "glob_search" | "Glob" => format_glob_result(icon, &parsed),
        "grep_search" | "Grep" => format_grep_result(icon, &parsed),
        _ => format_generic_tool_result(icon, name, &parsed),
    }
}

/// Tone colors for runtime event lines (256-color ANSI), mirroring the webview tones.
fn runtime_event_tone_color(tone: &str) -> &'static str {
    match tone {
        "ok" => "38;5;43",    // teal/green
        "err" => "38;5;203",  // red
        "warn" => "38;5;179", // amber
        "run" => "38;5;75",   // blue
        _ => "38;5;245",      // grey/idle
    }
}

/// Classify a status/kind string into a visual tone, matching the webview's `toneFromStatus`.
fn runtime_status_tone(value: &str) -> &'static str {
    let v = value.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| v.contains(n));
    if has(&["fail", "error", "blocked", "escalat", "reject", "denied", "cancel"]) {
        "err"
    } else if has(&["complete", "success", "passed", "recovered", "done", "resolved", "assigned"]) {
        "ok"
    } else if has(&["running", "in_progress", "started", "retry", "pending", "resume", "scheduled"])
    {
        "run"
    } else if has(&["warn", "partial", "skip", "degraded"]) {
        "warn"
    } else {
        "idle"
    }
}

fn rt_str<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(serde_json::Value::as_str)
}

/// Format a runtime event as a compact, colorized terminal line for the interactive (rich) mode.
/// Returns `None` for events with no meaningful one-line representation.
fn format_runtime_event_line(kind: &str, event: &serde_json::Value) -> Option<String> {
    let tone_of = |s: &str| runtime_event_tone_color(s);
    match kind {
        "plan_execution_event" => {
            let node = rt_str(event, "node_id").unwrap_or("node");
            let status = rt_str(event, "status").unwrap_or("");
            let step = rt_str(event, "kind").unwrap_or("").replace('_', " ");
            let tone = runtime_status_tone(if status.is_empty() { &step } else { status });
            let mut line = format!(
                "\x1b[{}m◆ plan\x1b[0m \x1b[1m{}\x1b[0m \x1b[2m{}\x1b[0m",
                tone_of(tone),
                step.trim(),
                node
            );
            if !status.is_empty() {
                line.push_str(&format!(" \x1b[{}m{}\x1b[0m", tone_of(tone), status));
            }
            if let Some(reason) = rt_str(event, "blocking_reason") {
                line.push_str(&format!(" \x1b[38;5;203m({reason})\x1b[0m"));
            }
            Some(line)
        }
        "task_ledger_event" => {
            let label = rt_str(event, "event").unwrap_or("update").replace('_', " ");
            let status = rt_str(event, "status").unwrap_or("");
            let tone = runtime_status_tone(if status.is_empty() { &label } else { status });
            let task = rt_str(event, "task_id").unwrap_or("");
            Some(format!(
                "\x1b[{}m◆ task\x1b[0m \x1b[1m{}\x1b[0m\x1b[2m{}{}\x1b[0m",
                tone_of(tone),
                label.trim(),
                if task.is_empty() { "" } else { " · " },
                task
            ))
        }
        // RUNTIME_EVENT_LINE_REST
        "model_route_event" => {
            let phase = rt_str(event, "phase").unwrap_or("");
            let model = rt_str(event, "model").unwrap_or("model");
            let mut line = format!(
                "\x1b[{}m◆ route\x1b[0m \x1b[2m{}{}\x1b[0m\x1b[1;36m{}\x1b[0m",
                tone_of("run"),
                phase,
                if phase.is_empty() { "" } else { " → " },
                model
            );
            if let Some(conf) = event.get("confidence").and_then(serde_json::Value::as_f64) {
                let pct = (conf * 100.0).round().clamp(0.0, 100.0) as u32;
                line.push_str(&format!(" \x1b[2m{pct}%\x1b[0m"));
            }
            if let Some(fallback) = rt_str(event, "fallback_model") {
                line.push_str(&format!(" \x1b[2m(fallback {fallback})\x1b[0m"));
            }
            Some(line)
        }
        "recovery_event" => {
            let attempted = event.get("recovery_attempted")?;
            let scenario = rt_str(attempted, "scenario").unwrap_or("issue");
            let result = attempted.get("result");
            let (outcome, tone) = match result {
                Some(r) if r.get("recovered").is_some() => ("recovered", "ok"),
                Some(r) if r.get("partial_recovery").is_some() => ("partial recovery", "warn"),
                Some(r) if r.get("escalation_required").is_some() => ("escalation required", "err"),
                _ => ("attempted", "run"),
            };
            Some(format!(
                "\x1b[{}m◆ recovery\x1b[0m \x1b[1m{}\x1b[0m \x1b[2m· {}\x1b[0m",
                tone_of(tone),
                outcome,
                scenario
            ))
        }
        "recovery_action_event" => {
            let results = event.get("results").and_then(serde_json::Value::as_array);
            let count = results.map_or(0, Vec::len);
            let blocked = results.is_some_and(|rs| {
                rs.iter()
                    .any(|r| r.get("blocked").and_then(serde_json::Value::as_bool) == Some(true))
            });
            let tone = if blocked { "err" } else { "ok" };
            Some(format!(
                "\x1b[{}m◆ recovery action\x1b[0m \x1b[2m{} action(s)\x1b[0m",
                tone_of(tone),
                count
            ))
        }
        "team_execution_event" => {
            let role = rt_str(event, "role").unwrap_or("");
            let label = rt_str(event, "kind").unwrap_or("event").replace('_', " ");
            let tone = runtime_status_tone(&label);
            Some(format!(
                "\x1b[{}m◆ team\x1b[0m \x1b[1m{}\x1b[0m\x1b[2m{}{}\x1b[0m",
                tone_of(tone),
                label.trim(),
                if role.is_empty() { "" } else { " · " },
                role
            ))
        }
        "task_execution_event" => {
            let completed = event.get("completed").and_then(serde_json::Value::as_bool) == Some(true);
            let blocked = event.get("blocked").and_then(serde_json::Value::as_bool) == Some(true);
            let steps = event
                .get("steps")
                .and_then(serde_json::Value::as_array)
                .map_or(0, Vec::len);
            let (status, tone) = if blocked {
                ("blocked", "err")
            } else if completed {
                ("completed", "ok")
            } else {
                ("running", "run")
            };
            Some(format!(
                "\x1b[{}m◆ exec\x1b[0m \x1b[1m{}\x1b[0m \x1b[2m{} step(s)\x1b[0m",
                tone_of(tone),
                status,
                steps
            ))
        }
        _ => None,
    }
}


const DISPLAY_TRUNCATION_NOTICE: &str =
    "\x1b[2m… output truncated for display; full result preserved in session.\x1b[0m";
const READ_DISPLAY_MAX_LINES: usize = 80;
const READ_DISPLAY_MAX_CHARS: usize = 6_000;
const TOOL_OUTPUT_DISPLAY_MAX_LINES: usize = 60;
const TOOL_OUTPUT_DISPLAY_MAX_CHARS: usize = 4_000;

fn extract_tool_path(parsed: &serde_json::Value) -> String {
    parsed
        .get("file_path")
        .or_else(|| parsed.get("filePath"))
        .or_else(|| parsed.get("path"))
        .and_then(|value| value.as_str())
        .unwrap_or("?")
        .to_string()
}

fn format_search_start(label: &str, parsed: &serde_json::Value) -> String {
    let pattern = parsed
        .get("pattern")
        .and_then(|value| value.as_str())
        .unwrap_or("?");
    let scope = parsed
        .get("path")
        .and_then(|value| value.as_str())
        .unwrap_or(".");
    format!("{label} {pattern}\n\x1b[2min {scope}\x1b[0m")
}

fn format_patch_preview(old_value: &str, new_value: &str) -> Option<String> {
    if old_value.is_empty() && new_value.is_empty() {
        return None;
    }
    Some(format!(
        "\x1b[38;5;203m- {}\x1b[0m\n\x1b[38;5;70m+ {}\x1b[0m",
        truncate_for_summary(first_visible_line(old_value), 72),
        truncate_for_summary(first_visible_line(new_value), 72)
    ))
}

fn format_bash_call(parsed: &serde_json::Value) -> String {
    let command = parsed
        .get("command")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    if command.is_empty() {
        String::new()
    } else {
        format!(
            "\x1b[48;5;236;38;5;255m $ {} \x1b[0m",
            truncate_for_summary(command, 160)
        )
    }
}

fn first_visible_line(text: &str) -> &str {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(text)
}

fn format_bash_result(icon: &str, parsed: &serde_json::Value) -> String {
    use std::fmt::Write as _;

    let mut lines = vec![format!("{icon} \x1b[38;5;245mbash\x1b[0m")];
    if let Some(task_id) = parsed
        .get("backgroundTaskId")
        .and_then(|value| value.as_str())
    {
        write!(&mut lines[0], " backgrounded ({task_id})").expect("write to string");
    } else if let Some(status) = parsed
        .get("returnCodeInterpretation")
        .and_then(|value| value.as_str())
        .filter(|status| !status.is_empty())
    {
        write!(&mut lines[0], " {status}").expect("write to string");
    }

    if let Some(stdout) = parsed.get("stdout").and_then(|value| value.as_str()) {
        if !stdout.trim().is_empty() {
            lines.push(truncate_output_for_display(
                stdout,
                TOOL_OUTPUT_DISPLAY_MAX_LINES,
                TOOL_OUTPUT_DISPLAY_MAX_CHARS,
            ));
        }
    }
    if let Some(stderr) = parsed.get("stderr").and_then(|value| value.as_str()) {
        if !stderr.trim().is_empty() {
            lines.push(format!(
                "\x1b[38;5;203m{}\x1b[0m",
                truncate_output_for_display(
                    stderr,
                    TOOL_OUTPUT_DISPLAY_MAX_LINES,
                    TOOL_OUTPUT_DISPLAY_MAX_CHARS,
                )
            ));
        }
    }

    lines.join("\n\n")
}

fn format_read_result(icon: &str, parsed: &serde_json::Value) -> String {
    let file = parsed.get("file").unwrap_or(parsed);
    let path = extract_tool_path(file);
    let start_line = file
        .get("startLine")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1);
    let num_lines = file
        .get("numLines")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let total_lines = file
        .get("totalLines")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(num_lines);
    let content = file
        .get("content")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let end_line = start_line.saturating_add(num_lines.saturating_sub(1));

    format!(
        "{icon} \x1b[2m📄 Read {path} (lines {}-{} of {})\x1b[0m\n{}",
        start_line,
        end_line.max(start_line),
        total_lines,
        truncate_output_for_display(content, READ_DISPLAY_MAX_LINES, READ_DISPLAY_MAX_CHARS)
    )
}

fn format_write_result(icon: &str, parsed: &serde_json::Value) -> String {
    let path = extract_tool_path(parsed);
    let kind = parsed
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("write");
    let line_count = parsed
        .get("content")
        .and_then(|value| value.as_str())
        .map_or(0, |content| content.lines().count());
    format!(
        "{icon} \x1b[1;32m✏️ {} {path}\x1b[0m \x1b[2m({line_count} lines)\x1b[0m",
        if kind == "create" { "Wrote" } else { "Updated" },
    )
}

fn format_structured_patch_preview(parsed: &serde_json::Value) -> Option<String> {
    let hunks = parsed.get("structuredPatch")?.as_array()?;
    let mut preview = Vec::new();
    for hunk in hunks.iter().take(2) {
        let lines = hunk.get("lines")?.as_array()?;
        for line in lines.iter().filter_map(|value| value.as_str()).take(6) {
            match line.chars().next() {
                Some('+') => preview.push(format!("\x1b[38;5;70m{line}\x1b[0m")),
                Some('-') => preview.push(format!("\x1b[38;5;203m{line}\x1b[0m")),
                _ => preview.push(line.to_string()),
            }
        }
    }
    if preview.is_empty() {
        None
    } else {
        Some(preview.join("\n"))
    }
}

fn format_edit_result(icon: &str, parsed: &serde_json::Value) -> String {
    let path = extract_tool_path(parsed);
    let suffix = if parsed
        .get("replaceAll")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        " (replace all)"
    } else {
        ""
    };
    let preview = format_structured_patch_preview(parsed).or_else(|| {
        let old_value = parsed
            .get("oldString")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let new_value = parsed
            .get("newString")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        format_patch_preview(old_value, new_value)
    });

    match preview {
        Some(preview) => format!("{icon} \x1b[1;33m📝 Edited {path}{suffix}\x1b[0m\n{preview}"),
        None => format!("{icon} \x1b[1;33m📝 Edited {path}{suffix}\x1b[0m"),
    }
}

fn format_glob_result(icon: &str, parsed: &serde_json::Value) -> String {
    let num_files = parsed
        .get("numFiles")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let filenames = parsed
        .get("filenames")
        .and_then(|value| value.as_array())
        .map(|files| {
            files
                .iter()
                .filter_map(|value| value.as_str())
                .take(8)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    if filenames.is_empty() {
        format!("{icon} \x1b[38;5;245mglob_search\x1b[0m matched {num_files} files")
    } else {
        format!("{icon} \x1b[38;5;245mglob_search\x1b[0m matched {num_files} files\n{filenames}")
    }
}

fn format_grep_result(icon: &str, parsed: &serde_json::Value) -> String {
    let num_matches = parsed
        .get("numMatches")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let num_files = parsed
        .get("numFiles")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let content = parsed
        .get("content")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let filenames = parsed
        .get("filenames")
        .and_then(|value| value.as_array())
        .map(|files| {
            files
                .iter()
                .filter_map(|value| value.as_str())
                .take(8)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    let summary = format!(
        "{icon} \x1b[38;5;245mgrep_search\x1b[0m {num_matches} matches across {num_files} files"
    );
    if !content.trim().is_empty() {
        format!(
            "{summary}\n{}",
            truncate_output_for_display(
                content,
                TOOL_OUTPUT_DISPLAY_MAX_LINES,
                TOOL_OUTPUT_DISPLAY_MAX_CHARS,
            )
        )
    } else if !filenames.is_empty() {
        format!("{summary}\n{filenames}")
    } else {
        summary
    }
}

fn format_generic_tool_result(icon: &str, name: &str, parsed: &serde_json::Value) -> String {
    let rendered_output = match parsed {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Null => String::new(),
        serde_json::Value::Object(_) | serde_json::Value::Array(_) => {
            serde_json::to_string_pretty(parsed).unwrap_or_else(|_| parsed.to_string())
        }
        _ => parsed.to_string(),
    };
    let preview = truncate_output_for_display(
        &rendered_output,
        TOOL_OUTPUT_DISPLAY_MAX_LINES,
        TOOL_OUTPUT_DISPLAY_MAX_CHARS,
    );

    if preview.is_empty() {
        format!("{icon} \x1b[38;5;245m{name}\x1b[0m")
    } else if preview.contains('\n') {
        format!("{icon} \x1b[38;5;245m{name}\x1b[0m\n{preview}")
    } else {
        format!("{icon} \x1b[38;5;245m{name}:\x1b[0m {preview}")
    }
}

fn summarize_tool_payload(payload: &str) -> String {
    let compact = match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(value) => value.to_string(),
        Err(_) => payload.trim().to_string(),
    };
    truncate_for_summary(&compact, 96)
}

fn truncate_for_summary(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn truncate_output_for_display(content: &str, max_lines: usize, max_chars: usize) -> String {
    let original = content.trim_end_matches('\n');
    if original.is_empty() {
        return String::new();
    }

    let mut preview_lines = Vec::new();
    let mut used_chars = 0usize;
    let mut truncated = false;

    for (index, line) in original.lines().enumerate() {
        if index >= max_lines {
            truncated = true;
            break;
        }

        let newline_cost = usize::from(!preview_lines.is_empty());
        let available = max_chars.saturating_sub(used_chars + newline_cost);
        if available == 0 {
            truncated = true;
            break;
        }

        let line_chars = line.chars().count();
        if line_chars > available {
            preview_lines.push(line.chars().take(available).collect::<String>());
            truncated = true;
            break;
        }

        preview_lines.push(line.to_string());
        used_chars += newline_cost + line_chars;
    }

    let mut preview = preview_lines.join("\n");
    if truncated {
        if !preview.is_empty() {
            preview.push('\n');
        }
        preview.push_str(DISPLAY_TRUNCATION_NOTICE);
    }
    preview
}

fn render_thinking_block_summary(
    out: &mut (impl Write + ?Sized),
    char_count: Option<usize>,
    redacted: bool,
) -> Result<(), RuntimeError> {
    let summary = if redacted {
        "\n▶ Thinking block hidden by provider\n".to_string()
    } else if let Some(char_count) = char_count {
        format!("\n▶ Thinking ({char_count} chars hidden)\n")
    } else {
        "\n▶ Thinking hidden\n".to_string()
    };
    write!(out, "{summary}")
        .and_then(|()| out.flush())
        .map_err(|error| RuntimeError::new(error.to_string()))
}

fn push_output_block(
    block: OutputContentBlock,
    out: &mut (impl Write + ?Sized),
    events: &mut Vec<AssistantEvent>,
    pending_tool: &mut Option<(String, String, String)>,
    streaming_tool_input: bool,
    block_has_thinking_summary: &mut bool,
    stream_json: bool,
) -> Result<(), RuntimeError> {
    match block {
        OutputContentBlock::Text { text } => {
            if !text.is_empty() {
                if stream_json {
                    print_stream_json_event(json!({"type":"text_delta","text":text}));
                } else {
                    let rendered = TerminalRenderer::new().markdown_to_ansi(&text);
                    write!(out, "{rendered}")
                        .and_then(|()| out.flush())
                        .map_err(|error| RuntimeError::new(error.to_string()))?;
                }
                events.push(AssistantEvent::TextDelta(text));
            }
        }
        OutputContentBlock::ToolUse { id, name, input } => {
            // During streaming, the initial content_block_start has an empty input ({}).
            // The real input arrives via input_json_delta events. In
            // non-streaming responses, preserve a legitimate empty object.
            let initial_input = if streaming_tool_input
                && input.is_object()
                && input.as_object().is_some_and(serde_json::Map::is_empty)
            {
                String::new()
            } else {
                input.to_string()
            };
            *pending_tool = Some((id, name, initial_input));
        }
        OutputContentBlock::Thinking {
            thinking,
            signature,
        } => {
            render_thinking_block_summary(out, Some(thinking.chars().count()), false)?;
            *block_has_thinking_summary = true;
            // Store the thinking block so it can be round-tripped back to the API.
            events.push(AssistantEvent::ReasoningStep(ReasoningStep::Analysis {
                content: thinking.clone(),
                confidence: None,
                signature: signature.clone(),
            }));
            if stream_json {
                let mut step = serde_json::json!({
                    "step_type": "analysis",
                    "content": thinking,
                });
                if let Some(sig) = signature {
                    step["signature"] = serde_json::json!(sig);
                }
                print_stream_json_event(json!({"type":"reasoning_step","reasoning_step":step}));
            }
        }
        OutputContentBlock::RedactedThinking { data } => {
            render_thinking_block_summary(out, None, true)?;
            *block_has_thinking_summary = true;
            events.push(AssistantEvent::ReasoningStep(
                ReasoningStep::RedactedThinking {
                    data: data.to_string(),
                },
            ));
            if stream_json {
                print_stream_json_event(
                    json!({"type":"reasoning_step","reasoning_step":{"step_type":"redacted_thinking","data":data}}),
                );
            }
        }
    }
    Ok(())
}

fn assistant_events_have_content(events: &[AssistantEvent]) -> bool {
    events.iter().any(|event| {
        matches!(event, AssistantEvent::TextDelta(text) if !text.is_empty())
            || matches!(event, AssistantEvent::ToolUse { .. })
            || matches!(
                event,
                AssistantEvent::ReasoningStep(
                    ReasoningStep::Analysis { .. } | ReasoningStep::RedactedThinking { .. }
                )
            )
    })
}

fn response_to_events(
    response: MessageResponse,
    out: &mut (impl Write + ?Sized),
    stream_json: bool,
) -> Result<Vec<AssistantEvent>, RuntimeError> {
    let mut events = Vec::new();
    let mut pending_tool = None;

    for block in response.content {
        let mut block_has_thinking_summary = false;
        push_output_block(
            block,
            out,
            &mut events,
            &mut pending_tool,
            false,
            &mut block_has_thinking_summary,
            stream_json,
        )?;
        if let Some((id, name, input)) = pending_tool.take() {
            if stream_json {
                let input_val: serde_json::Value = serde_json::from_str(&input)
                    .unwrap_or(serde_json::Value::String(input.clone()));
                print_stream_json_event(
                    json!({"type":"tool_use","id":id,"name":name,"input":input_val}),
                );
            }
            events.push(AssistantEvent::ToolUse { id, name, input });
        }
    }

    events.push(AssistantEvent::Usage(response.usage.token_usage()));
    events.push(AssistantEvent::MessageStop);
    Ok(events)
}

fn push_prompt_cache_record(client: &ApiProviderClient, events: &mut Vec<AssistantEvent>) {
    // `ApiProviderClient::take_last_prompt_cache_record` is a pass-through
    // to the Anthropic variant and returns `None` for OpenAI-compat /
    // xAI variants, which do not have a prompt cache. So this helper
    // remains a no-op on non-Anthropic providers without any extra
    // branching here.
    if let Some(record) = client.take_last_prompt_cache_record() {
        if let Some(event) = prompt_cache_record_to_runtime_event(record) {
            events.push(AssistantEvent::PromptCache(event));
        }
    }
}

fn prompt_cache_record_to_runtime_event(
    record: api::PromptCacheRecord,
) -> Option<PromptCacheEvent> {
    let cache_break = record.cache_break?;
    Some(PromptCacheEvent {
        unexpected: cache_break.unexpected,
        reason: cache_break.reason,
        previous_cache_read_input_tokens: cache_break.previous_cache_read_input_tokens,
        current_cache_read_input_tokens: cache_break.current_cache_read_input_tokens,
        token_drop: cache_break.token_drop,
    })
}

struct CliToolExecutor {
    renderer: TerminalRenderer,
    emit_output: bool,
    stream_json: bool,
    allowed_tools: Option<AllowedToolSet>,
    tool_registry: GlobalToolRegistry,
    mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
}

impl CliToolExecutor {
    fn new(
        allowed_tools: Option<AllowedToolSet>,
        emit_output: bool,
        stream_json: bool,
        tool_registry: GlobalToolRegistry,
        mcp_state: Option<Arc<Mutex<RuntimeMcpState>>>,
    ) -> Self {
        Self {
            renderer: TerminalRenderer::new(),
            emit_output,
            stream_json,
            allowed_tools,
            tool_registry,
            mcp_state,
        }
    }

    fn execute_search_tool(&self, value: serde_json::Value) -> Result<String, ToolError> {
        let input: ToolSearchRequest = serde_json::from_value(value)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        let (pending_mcp_servers, mcp_degraded) =
            self.mcp_state.as_ref().map_or((None, None), |state| {
                let state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (state.pending_servers(), state.degraded_report())
            });
        serde_json::to_string_pretty(&self.tool_registry.search(
            &input.query,
            input.max_results.unwrap_or(5),
            pending_mcp_servers,
            mcp_degraded,
        ))
        .map_err(|error| ToolError::new(error.to_string()))
    }

    fn execute_runtime_tool(
        &self,
        tool_name: &str,
        value: serde_json::Value,
    ) -> Result<String, ToolError> {
        let Some(mcp_state) = &self.mcp_state else {
            return Err(ToolError::new(format!(
                "runtime tool `{tool_name}` is unavailable without configured MCP servers"
            )));
        };
        let mut mcp_state = mcp_state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        match tool_name {
            "MCPTool" => {
                let input: McpToolRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                let qualified_name = input
                    .qualified_name
                    .or(input.tool)
                    .ok_or_else(|| ToolError::new("missing required field `qualifiedName`"))?;
                mcp_state.call_tool(&qualified_name, input.arguments)
            }
            "ListMcpResourcesTool" => {
                let input: ListMcpResourcesRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                match input.server {
                    Some(server_name) => mcp_state.list_resources_for_server(&server_name),
                    None => mcp_state.list_resources_for_all_servers(),
                }
            }
            "ReadMcpResourceTool" => {
                let input: ReadMcpResourceRequest = serde_json::from_value(value)
                    .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
                mcp_state.read_resource(&input.server, &input.uri)
            }
            _ => mcp_state.call_tool(tool_name, Some(value)),
        }
    }
}

impl ToolExecutor for CliToolExecutor {
    fn available_tools(&self) -> Vec<runtime::Tool> {
        filter_tool_specs(&self.tool_registry, self.allowed_tools.as_ref())
            .into_iter()
            .map(|definition| {
                tool_from_profile(
                    &definition.name,
                    definition.description.as_deref(),
                    Some(&definition.input_schema),
                )
            })
            .collect()
    }

    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        if self
            .allowed_tools
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(tool_name))
        {
            return Err(ToolError::new(format!(
                "tool `{tool_name}` is not enabled by the current --allowedTools setting"
            )));
        }
        let value = serde_json::from_str(input)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;
        let result = if tool_name == "ToolSearch" {
            self.execute_search_tool(value)
        } else if self.tool_registry.has_runtime_tool(tool_name) {
            self.tool_registry
                .enforce_tool_permission(tool_name, &value)
                .map_err(ToolError::new)?;
            self.execute_runtime_tool(tool_name, value)
        } else {
            self.tool_registry
                .execute(tool_name, &value)
                .map_err(ToolError::new)
        };
        match result {
            Ok(output) => {
                if self.stream_json {
                    print_stream_json_event(
                        json!({"type":"tool_result","name":tool_name,"output":output,"is_error":false}),
                    );
                } else if self.emit_output {
                    let markdown = format_tool_result(tool_name, &output, false);
                    self.renderer
                        .stream_markdown(&markdown, &mut io::stdout())
                        .map_err(|error| ToolError::new(error.to_string()))?;
                }
                Ok(output)
            }
            Err(error) => {
                if self.stream_json {
                    print_stream_json_tool_error(tool_name, &error.to_string());
                } else if self.emit_output {
                    let markdown = format_tool_result(tool_name, &error.to_string(), true);
                    self.renderer
                        .stream_markdown(&markdown, &mut io::stdout())
                        .map_err(|stream_error| ToolError::new(stream_error.to_string()))?;
                }
                Err(error)
            }
        }
    }
}

fn permission_policy(
    mode: PermissionMode,
    feature_config: &runtime::RuntimeFeatureConfig,
    tool_registry: &GlobalToolRegistry,
) -> Result<PermissionPolicy, String> {
    Ok(tool_registry.permission_specs(None)?.into_iter().fold(
        PermissionPolicy::new(mode).with_permission_rules(feature_config.permission_rules()),
        |policy, (name, required_permission)| {
            policy.with_tool_requirement(name, required_permission)
        },
    ))
}

/// Expand `@path` tokens in `input` that resolve to existing files.
///
/// Text-extractable files are inlined into the returned prompt string.
/// Image files (when the model supports vision) are returned as separate
/// [`ContentBlock::Image`] blocks to be injected before the turn.
/// Sensitive attachment checks shared with VS Code (attachmentPaths.ts). Both
/// lists must stay in sync to keep the security story consistent across the
/// CLI and the extension.
fn is_attachment_blocked(path: &std::path::Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let blocked_extensions: &[&str] = &[".pem", ".key", ".p12", ".pfx", ".kdbx"];
    if blocked_extensions
        .iter()
        .any(|blocked| ext.ends_with(blocked.trim_start_matches('.')))
    {
        return true;
    }
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let blocked_snippets: &[&str] = &["id_rsa", "id_ed25519", "credentials", "secret", "token"];
    blocked_snippets
        .iter()
        .any(|snippet| filename.contains(snippet))
}

/// Preprocess `file:`, `附件:`, and `文件:` prefix lines (VS Code attachment
/// syntax) into `@path` tokens so the unified CLI recognizer works with
/// prompts written in both environments. Each such line is replaced with a
/// `@path` reference that `expand_at_file_syntax` then expands.
fn expand_file_prefix_lines(input: &str) -> String {
    let prefixes: &[&str] = &["file:", "文件:", "附件:"];
    let mut out = String::with_capacity(input.len());
    for line in input.lines() {
        let trimmed = line.trim();
        let mut matched = false;
        for prefix in prefixes {
            if let Some(path) = trimmed
                .strip_prefix(prefix)
                .map(|rest| rest.trim())
                .filter(|rest| !rest.is_empty())
            {
                // Strip surrounding quotes if present.
                let clean = path
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .or_else(|| path.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                    .unwrap_or(path);
                out.push_str(&format!("@{clean}\n"));
                matched = true;
                break;
            }
        }
        if !matched {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}

fn attachment_may_require_vision(path: &std::path::Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        ext.as_str(),
        "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp"
    )
}

fn model_wants_image_blocks(model: &str) -> bool {
    let base_url = std::env::var("OPENAI_BASE_URL")
        .ok()
        .filter(|s| !s.is_empty());
    api::model_supports_vision_probed(model, base_url.as_deref())
}

fn expand_at_file_syntax(input: &str, model: &str) -> Result<(String, Vec<ContentBlock>), String> {
    use file_extract::{extract_file, FileContent};

    // The vision probe may perform network I/O and can wake a local model. Only
    // run it lazily when an actual raster image attachment is being expanded.
    let mut want_image_cache: Option<bool> = None;
    let mut image_blocks = Vec::new();
    let mut result = String::with_capacity(input.len());
    let mut chars = input.char_indices().peekable();

    while let Some((i, ch)) = chars.next() {
        if ch != '@' {
            result.push(ch);
            continue;
        }
        // Collect the path token (up to whitespace or end of string)
        let start = i + 1;
        let end = input[start..]
            .find(|c: char| c.is_whitespace())
            .map(|n| start + n)
            .unwrap_or(input.len());
        let path_str = &input[start..end];
        let path = std::path::Path::new(path_str);

        if path_str.is_empty() {
            result.push(ch);
            continue;
        }
        if !path.exists() {
            eprintln!(
                "Attachment warning: file not found — {}  (use --file <path> to attach from another directory)",
                path.display()
            );
            result.push(ch);
            continue;
        }
        // Block sensitive files (mirrors VS Code attachmentPaths.ts blocked list).
        if is_attachment_blocked(path) {
            eprintln!(
                "Attachment blocked: {} is an excluded file type (credentials, keys, tokens)",
                path.display()
            );
            result.push(ch);
            continue;
        }

        // Advance the iterator past the path characters
        let skip = end - start;
        for _ in 0..skip {
            chars.next();
        }

        let want_image = if attachment_may_require_vision(path) {
            *want_image_cache.get_or_insert_with(|| model_wants_image_blocks(model))
        } else {
            false
        };

        match extract_file(path, want_image) {
            Ok(FileContent::Text(text)) => {
                warn_for_extracted_attachment_text(path, &text);
                let label = path_str;
                result.push_str(&format!("[File: {label}]\n{text}\n"));
            }
            Ok(FileContent::Image {
                base64_data,
                media_type,
            }) => {
                image_blocks.push(ContentBlock::Image {
                    data: base64_data,
                    media_type: media_type.as_mime().to_string(),
                });
            }
            Err(e) => return Err(format!("@{path_str}: {e}")),
        }
    }

    Ok((result.trim().to_string(), image_blocks))
}

/// Extract attachment names from the expanded prompt (`[File: ...]` markers)
/// and image blocks, so the CLI can print a summary before the turn begins.
fn attachment_summary(expanded_text: &str, image_blocks: &[ContentBlock]) -> String {
    let mut files: Vec<String> = Vec::new();
    // Text attachments are inlined as `[File: path]\n...`
    let mut remaining = expanded_text;
    while let Some(idx) = remaining.find("[File: ") {
        remaining = &remaining[idx + 7..];
        if let Some(end) = remaining.find(']') {
            files.push(remaining[..end].to_string());
        }
    }
    // Image blocks carry no inline marker, just a ContentBlock::Image.
    for block in image_blocks {
        if let ContentBlock::Image { media_type, .. } = block {
            files.push(format!("(image, {media_type})"));
        }
    }
    if files.is_empty() {
        return String::new();
    }
    format!("\n  Attached {} file(s): {}", files.len(), files.join(", "))
}

/// Load files from `paths` and convert them to [`ContentBlock`]s.
///
/// Uses [`api::model_supports_vision_probed`] to decide whether images should be
/// sent as base64 vision blocks or downgraded to text placeholders (for
/// Ollama and other models without vision support).
fn load_files_as_content_blocks(
    paths: &[PathBuf],
    model: &str,
) -> Result<Vec<ContentBlock>, String> {
    use file_extract::{extract_file, FileContent};

    let want_image = if paths
        .iter()
        .any(|path| !is_attachment_blocked(path) && attachment_may_require_vision(path))
    {
        model_wants_image_blocks(model)
    } else {
        false
    };
    let mut blocks = Vec::new();
    for path in paths {
        if is_attachment_blocked(path) {
            eprintln!(
                "Attachment blocked: {} is an excluded file type",
                path.display()
            );
            continue;
        }
        match extract_file(path, want_image) {
            Ok(FileContent::Text(text)) => {
                warn_for_extracted_attachment_text(path, &text);
                let label = path.display().to_string();
                blocks.push(ContentBlock::Text {
                    text: format!("[File: {label}]\n{text}"),
                });
            }
            Ok(FileContent::Image {
                base64_data,
                media_type,
            }) => {
                blocks.push(ContentBlock::Image {
                    data: base64_data,
                    media_type: media_type.as_mime().to_string(),
                });
            }
            Err(e) => return Err(format!("failed to load {}: {e}", path.display())),
        }
    }
    Ok(blocks)
}

const LARGE_TEXT_ATTACHMENT_WARNING_CHARS: usize = 120_000;

fn warn_for_extracted_attachment_text(path: &Path, text: &str) {
    for warning in extracted_attachment_warnings(path, text) {
        eprintln!("{warning}");
    }
}

fn extracted_attachment_warnings(path: &Path, text: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    if text.contains(file_extract::PDF_NO_EXTRACTABLE_TEXT) {
        warnings.push(format!(
            "Attachment warning: no extractable text found in {}",
            path.display()
        ));
    } else if text.contains(file_extract::PDF_EXTRACTION_WARNING_PREFIX) {
        warnings.push(format!(
            "Attachment warning: partial or low-confidence PDF text extraction for {}",
            path.display()
        ));
    }

    let extracted_chars = text.chars().count();
    if extracted_chars >= LARGE_TEXT_ATTACHMENT_WARNING_CHARS {
        warnings.push(format!(
            "Attachment warning: extracted {extracted_chars} characters from {}; small-context local models may not read the whole attachment in one turn",
            path.display()
        ));
    }
    warnings
}

fn convert_messages(messages: &[ConversationMessage]) -> Vec<InputMessage> {
    messages
        .iter()
        .filter_map(|message| {
            let role = match message.role {
                MessageRole::System | MessageRole::User | MessageRole::Tool => "user",
                MessageRole::Assistant => "assistant",
            };
            let content = message
                .blocks
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text } => InputContentBlock::Text { text: text.clone() },
                    ContentBlock::ToolUse { id, name, input } => InputContentBlock::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: serde_json::from_str(input)
                            .unwrap_or_else(|_| serde_json::json!({ "raw": input })),
                    },
                    ContentBlock::ToolResult {
                        tool_use_id,
                        output,
                        is_error,
                        ..
                    } => InputContentBlock::ToolResult {
                        tool_use_id: tool_use_id.clone(),
                        content: vec![ToolResultContentBlock::Text {
                            text: output.clone(),
                        }],
                        is_error: *is_error,
                    },
                    ContentBlock::Image { data, media_type } => InputContentBlock::Image {
                        data: data.clone(),
                        media_type: media_type.clone(),
                    },
                    ContentBlock::Thinking {
                        thinking,
                        signature,
                    } => InputContentBlock::Thinking {
                        thinking: thinking.clone(),
                        signature: signature.clone(),
                    },
                    ContentBlock::RedactedThinking { data } => {
                        InputContentBlock::RedactedThinking {
                            data: serde_json::from_str(data)
                                .unwrap_or_else(|_| serde_json::Value::String(data.clone())),
                        }
                    }
                })
                .collect::<Vec<_>>();
            (!content.is_empty()).then(|| InputMessage {
                role: role.to_string(),
                content,
            })
        })
        .collect()
}

#[allow(clippy::too_many_lines)]
fn print_help_to(out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "Himalaya v{VERSION}")?;
    writeln!(out)?;
    writeln!(out, "Usage:")?;
    writeln!(
        out,
        "  Himalaya [--model MODEL] [--allowedTools TOOL[,TOOL...]]"
    )?;
    writeln!(out, "      Start the interactive REPL")?;
    writeln!(
        out,
        "  Himalaya [--model MODEL] [--output-format text|json|stream-json] prompt TEXT"
    )?;
    writeln!(out, "      Send one prompt and exit")?;
    writeln!(
        out,
        "  Himalaya [--model MODEL] [--output-format text|json|stream-json] TEXT"
    )?;
    writeln!(out, "      Shorthand non-interactive prompt mode")?;
    writeln!(
        out,
        "  Himalaya --resume [SESSION.jsonl|session-id|latest] [/status] [/compact] [...]"
    )?;
    writeln!(
        out,
        "      Inspect or maintain a saved session without entering the REPL"
    )?;
    writeln!(out, "  Himalaya help")?;
    writeln!(out, "      Alias for --help")?;
    writeln!(out, "  Himalaya version")?;
    writeln!(out, "      Alias for --version")?;
    writeln!(out, "  Himalaya status")?;
    writeln!(
        out,
        "      Show the current local workspace status snapshot"
    )?;
    writeln!(out, "  Himalaya sandbox")?;
    writeln!(out, "      Show the current sandbox isolation snapshot")?;
    writeln!(out, "  Himalaya doctor")?;
    writeln!(
        out,
        "      Diagnose local auth, config, workspace, and sandbox health"
    )?;
    writeln!(
        out,
        "  Himalaya tasks [list|show <task-id>|status <task-id>|report <task-id>|review <task-id>|packet create <packet.json>|packet run <packet.json>|packet status <task-id>|scheduler tick|scheduler queue|scheduler explain <task-id>|scheduler run [--once|--max-ticks N]|scheduler status|daemon start [--once|--max-ticks N]|daemon status|daemon stop|daemon logs [--limit N]|daemon report [--limit N] [--max-ticks N]|daemon evaluate [--limit N] [--max-ticks N]|daemon replay [--limit N] [--max-ticks N]|resume <task-id> [prompt]|execute <task-id>|verify <task-id>|recover <task-id>|cancel <task-id>]"
    )?;
    writeln!(
        out,
        "      Inspect, resume, execute, verify, recover, schedule, daemonize, diagnose, or cancel durable long-running tasks"
    )?;
    writeln!(
        out,
        "  Himalaya workers [list|create|spawn [--cwd PATH] [--trusted-root PATH] [--isolate-worktree] [--worktree-root PATH] -- COMMAND...|probe <worker-id>|observe|ready|resolve-trust|prompt|complete|restart|terminate|cleanup [--stale]|supervise]"
    )?;
    writeln!(
        out,
        "      Supervise local worker lifecycle state and durable scheduler ticks"
    )?;
    writeln!(
        out,
        "  Himalaya routes [feedback summary|summary|list|optimize|replay|propose|apply <proposal-id> [--dry-run]|rollback <proposal-id>]"
    )?;
    writeln!(
        out,
        "      Summarize feedback, evaluate routing, and manage safe MoE policy proposals"
    )?;
    writeln!(
        out,
        "  Himalaya policy [review [--limit N] [--max-ticks N] [--no-record]|ledger [--limit N]|replay [--limit N]|plan [--limit N] [--max-ticks N]|apply [--dry-run] [--domain routing|scheduler|memory|recovery] [--proposal-id ID]|rollback [--domain routing] [--proposal-id ID]]"
    )?;
    writeln!(
        out,
        "      Review, plan, apply, rollback, and audit governed autonomous policy state"
    )?;
    writeln!(
        out,
        "  Himalaya benchmark [list|show <task-id>|run [--record]]"
    )?;
    writeln!(
        out,
        "      Run the built-in complex coding task benchmark suite"
    )?;
    writeln!(out, "  Himalaya maturity-matrix")?;
    writeln!(
        out,
        "      Summarize built-in tool and slash command maturity"
    )?;
    writeln!(out, "  Himalaya plan TASK")?;
    writeln!(
        out,
        "      Generate a local task decomposition without calling the API"
    )?;
    writeln!(out, "  Himalaya bootstrap-plan")?;
    writeln!(out, "  Himalaya agents")?;
    writeln!(out, "  Himalaya mcp")?;
    writeln!(out, "  Himalaya skills")?;
    writeln!(
        out,
        "  Himalaya system-prompt [--cwd PATH] [--date YYYY-MM-DD]"
    )?;
    writeln!(out, "  Himalaya login")?;
    writeln!(out, "  Himalaya logout")?;
    writeln!(out, "  Himalaya init")?;
    writeln!(
        out,
        "  Himalaya export [PATH] [--session SESSION] [--output PATH]"
    )?;
    writeln!(
        out,
        "      Dump the latest (or named) session as markdown; writes to PATH or stdout"
    )?;
    writeln!(out)?;
    writeln!(out, "Flags:")?;
    writeln!(
        out,
        "  --model MODEL              Override the active model"
    )?;
    writeln!(
        out,
        "  --output-format FORMAT     Non-interactive output format: text, json, or stream-json"
    )?;
    writeln!(
        out,
        "  --compact                  Strip tool call details; print only the final assistant text (text mode only; useful for piping)"
    )?;
    writeln!(
        out,
        "  --permission-mode MODE     Set default, plan, acceptEdits, auto, or bypassPermissions"
    )?;
    writeln!(
        out,
        "  --dangerously-skip-permissions  Skip all permission checks"
    )?;
    writeln!(out, "  --allowedTools TOOLS       Restrict enabled tools (repeatable; comma-separated aliases supported)")?;
    writeln!(
        out,
        "  --version, -V              Print version and build information locally"
    )?;
    writeln!(out)?;
    writeln!(out, "Interactive slash commands:")?;
    writeln!(out, "{}", render_slash_command_help_filtered())?;
    writeln!(out)?;
    let resume_commands = resume_supported_slash_commands()
        .into_iter()
        .map(|spec| match spec.argument_hint {
            Some(argument_hint) => format!("/{} {}", spec.name, argument_hint),
            None => format!("/{}", spec.name),
        })
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(out, "Resume-safe commands: {resume_commands}")?;
    writeln!(out)?;
    writeln!(out, "Session shortcuts:")?;
    writeln!(
        out,
        "  REPL turns auto-save to .Himalaya/sessions/<session-id>.{PRIMARY_SESSION_EXTENSION}"
    )?;
    writeln!(
        out,
        "  Use `{LATEST_SESSION_REFERENCE}` with --resume, /resume, or /session switch to target the newest saved session"
    )?;
    writeln!(
        out,
        "  Use /session list in the REPL to browse managed sessions"
    )?;
    writeln!(out, "Examples:")?;
    writeln!(
        out,
        "  Himalaya --model Himalaya-opus \"summarize this repo\""
    )?;
    writeln!(
        out,
        "  Himalaya --output-format json prompt \"explain src/main.rs\""
    )?;
    writeln!(out, "  Himalaya --compact \"summarize Cargo.toml\" | wc -l")?;
    writeln!(
        out,
        "  Himalaya --allowedTools read,glob \"summarize Cargo.toml\""
    )?;
    writeln!(out, "  Himalaya --resume {LATEST_SESSION_REFERENCE}")?;
    writeln!(
        out,
        "  Himalaya --resume {LATEST_SESSION_REFERENCE} /status /diff /export notes.txt"
    )?;
    writeln!(out, "  Himalaya agents")?;
    writeln!(out, "  Himalaya mcp show my-server")?;
    writeln!(out, "  Himalaya /skills")?;
    writeln!(out, "  Himalaya doctor")?;
    writeln!(out, "  Himalaya tasks daemon status")?;
    writeln!(
        out,
        "  Himalaya tasks daemon report --limit 20 --max-ticks 3"
    )?;
    writeln!(out, "  Himalaya tasks daemon evaluate --limit 20")?;
    writeln!(out, "  Himalaya tasks daemon replay --limit 20")?;
    writeln!(
        out,
        "  Himalaya policy apply --dry-run --domain routing --proposal-id <id>"
    )?;
    writeln!(out, "  Himalaya login")?;
    writeln!(out, "  Himalaya init")?;
    writeln!(out, "  Himalaya export")?;
    writeln!(out, "  Himalaya export conversation.md")?;
    Ok(())
}

fn print_help(output_format: CliOutputFormat) -> Result<(), Box<dyn std::error::Error>> {
    let mut buffer = Vec::new();
    print_help_to(&mut buffer)?;
    let message = String::from_utf8(buffer)?;
    match output_format {
        CliOutputFormat::Text => print!("{message}"),
        CliOutputFormat::Json | CliOutputFormat::StreamJson => println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "kind": "help",
                "message": message,
            }))?
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        attachment_may_require_vision, build_plan_output, build_runtime_plugin_state_with_loader,
        build_runtime_with_plugin_state, collect_session_prompt_history,
        create_managed_session_handle, describe_tool_progress, extracted_attachment_warnings,
        filter_tool_specs, format_bughunter_report, format_commit_preflight_report,
        format_commit_skipped_report, format_compact_report, format_connected_line,
        format_cost_report, format_history_timestamp, format_internal_prompt_progress_line,
        format_issue_report, format_model_report, format_model_switch_report,
        format_permissions_report, format_permissions_switch_report, format_pr_report,
        format_resume_report, format_status_report, format_runtime_event_line, format_tool_call_start, format_tool_result,
        format_ultraplan_report, format_unknown_slash_command,
        format_unknown_slash_command_message, format_user_visible_api_error,
        load_files_as_content_blocks, maturity_matrix_value, merge_prompt_with_stdin,
        normalize_permission_mode, parse_args, parse_benchmark_cli_command, parse_export_args,
        parse_git_status_branch, parse_git_status_metadata_for, parse_git_workspace_summary,
        parse_history_count, parse_policy_cli_command, parse_route_cli_command,
        parse_task_cli_command, parse_worker_cli_command, permission_policy, print_help_to,
        push_output_block, render_config_report, render_diff_report, render_diff_report_for,
        render_governed_policy_apply_text, render_maturity_matrix_text, render_memory_report,
        render_policy_apply_plan_text, render_policy_replay_text, render_prompt_history_report,
        render_repl_help, render_resume_usage, render_session_markdown, resolve_model_alias,
        resolve_model_alias_with_config, resolve_repl_model, resolve_session_reference,
        response_to_events, resume_supported_slash_commands, run_resume_command, short_tool_id,
        slash_command_completion_candidates_with_sessions, slash_command_status, status_context,
        stream_json_event, summarize_tool_payload_for_markdown, validate_no_args,
        write_mcp_server_fixture, BenchmarkCliCommand, CliAction, CliOutputFormat, CliToolExecutor,
        CronCliCommand, GitWorkspaceSummary, InternalPromptProgressEvent,
        InternalPromptProgressState, LiveCli, LocalHelpTopic, PolicyCliCommand, PromptHistoryEntry,
        RouteCliCommand, SlashCommand, SlashCommandStatus, StatusUsage, TaskCliCommand,
        TaskDaemonCliCommand, TaskPacketCliCommand, TaskSchedulerCliCommand, WorkerCliCommand,
        DEFAULT_MODEL, LARGE_TEXT_ATTACHMENT_WARNING_CHARS, LATEST_SESSION_REFERENCE,
        STREAM_PROTOCOL_VERSION,
    };
    use crate::autonomous_cli::{
        render_autonomous_daemon_report_text, render_autonomous_health_checkpoint_text,
        render_autonomous_integration_text, render_autonomous_policy_summary_text,
        render_autonomous_preflight_blocked_text, render_autonomous_replay_text,
    };
    use api::{ApiError, MessageResponse, OutputContentBlock, Usage};
    use plugins::{
        PluginManager, PluginManagerConfig, PluginTool, PluginToolDefinition, PluginToolPermission,
    };
    use runtime::{
        load_oauth_credentials, save_oauth_credentials, AssistantEvent, ConfigLoader, ContentBlock,
        ConversationMessage, MessageRole, OAuthConfig, PermissionMode, PermissionOutcome, Session,
        ToolExecutor,
    };
    use serde_json::json;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::{Mutex, MutexGuard, OnceLock};
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tools::GlobalToolRegistry;

    #[test]
    fn stream_json_event_sets_protocol_version() {
        let event = stream_json_event(json!({
            "type": "text_delta",
            "text": "hi",
        }));

        assert_eq!(event["type"], "text_delta");
        assert_eq!(event["text"], "hi");
        assert_eq!(
            event["protocol_version"],
            serde_json::Value::from(STREAM_PROTOCOL_VERSION)
        );
    }

    #[test]
    fn stream_json_event_overwrites_protocol_version() {
        let event = stream_json_event(json!({
            "type": "done",
            "protocol_version": 999,
        }));

        assert_eq!(event["type"], "done");
        assert_eq!(
            event["protocol_version"],
            serde_json::Value::from(STREAM_PROTOCOL_VERSION)
        );
    }

    #[test]
    fn policy_apply_plan_text_includes_adapter_summary() {
        let text = render_policy_apply_plan_text(&json!({
            "type": "policy_apply_plan",
            "recorded": false,
            "plan": {
                "status": "planned",
                "blockers": [],
                "adapters": [{
                    "domain": "routing",
                    "name": "routing_policy",
                    "supports_persistent_apply": true,
                    "supports_dry_run": true,
                    "supports_rollback": true,
                    "planned_only": false
                }],
                "actions": [{
                    "domain": "routing",
                    "proposal_id": "route-proposal-1",
                    "status": "planned",
                    "executable": true
                }]
            }
        }));

        assert!(text.contains("Adapters 1"));
        assert!(text.contains("routing/routing_policy"));
        assert!(text.contains("persistent=true"));
        assert!(text.contains("Actions:"));
    }

    #[test]
    fn policy_apply_text_includes_adapter_report_kind() {
        let text = render_governed_policy_apply_text(&json!({
            "type": "policy_apply",
            "apply": {
                "status": "dry_run_passed",
                "dry_run": true,
                "applied": false,
                "blockers": [],
                "receipt": {
                    "id": "policy-apply-1-receipt",
                    "adapter": "routing_policy"
                },
                "adapter_report": {
                    "kind": "routing_apply",
                    "report": {}
                },
                "structured_blockers": []
            }
        }));

        assert!(text.contains("Adapter  routing_policy"));
        assert!(text.contains("Adapter report routing_apply"));
    }

    #[test]
    fn policy_replay_text_includes_distribution_summary() {
        let text = render_policy_replay_text(&json!({
            "type": "policy_replay",
            "replay": {
                "summary": {
                    "lifecycle_count": 2,
                    "event_count": 4,
                    "anomaly_count": 1,
                    "malformed_lines": 0,
                    "domain_counts": {
                        "routing": 1,
                        "memory": 1
                    },
                    "action_counts": {
                        "apply_routing_policy_overlay": 2,
                        "dry_run_memory_policy_coverage": 2
                    },
                    "anomaly_kind_counts": {
                        "duplicate_status": 1
                    }
                },
                "lifecycles": []
            }
        }));

        assert!(text.contains("Domains"));
        assert!(text.contains("routing=1"));
        assert!(text.contains("Actions"));
        assert!(text.contains("dry_run_memory_policy_coverage=2"));
        assert!(text.contains("Anomaly kinds duplicate_status=1"));
    }

    #[test]
    fn autonomous_integration_text_surfaces_cross_domain_health() {
        let text = render_autonomous_integration_text(&json!({
            "integration": {
                "status": "degraded",
                "summary": {
                    "task_count": 2,
                    "runnable_task_count": 1,
                    "blocked_task_count": 1,
                    "scheduler_status": "running",
                    "scheduler_tick_count": 3,
                    "worker_count": 1,
                    "active_worker_count": 1,
                    "blocked_worker_count": 0,
                    "task_memory_entries": 2,
                    "route_feedback_entries": 2,
                    "policy_lifecycle_count": 1,
                    "policy_anomaly_count": 0
                },
                "invariants": [{
                    "name": "memory_entries_reference_tasks",
                    "status": "passed"
                }, {
                    "name": "policy_replay_has_no_anomalies",
                    "status": "warning"
                }],
                "replay": {
                    "stages": [{
                        "name": "task_lifecycle",
                        "status": "passed"
                    }, {
                        "name": "routing_policy_replay",
                        "status": "warning"
                    }]
                },
                "recommendations": [
                    "Complete missing golden replay stages."
                ]
            },
            "health": {
                "status": "degraded",
                "headline": "Autonomous loop is usable but missing complete evidence.",
                "next_action": "Complete missing golden replay stages.",
                "safe_to_iterate": true,
                "safe_to_apply_policy": false,
                "blockers": [],
                "warnings": ["replay routing_policy_replay needs evidence"],
                "highlights": ["tasks=2 runnable=1 blocked=1"]
            }
        }));

        assert!(text.contains("Autonomous integration"));
        assert!(text.contains("Status            degraded"));
        assert!(text.contains(
            "Summary           Autonomous loop is usable but missing complete evidence."
        ));
        assert!(text.contains("Next action       Complete missing golden replay stages."));
        assert!(text.contains("Safety            iterate=true apply_policy=false"));
        assert!(text.contains("2 total / 1 runnable / 1 blocked"));
        assert!(text.contains("routing_policy_replay: warning"));
        assert!(text.contains("Complete missing golden replay stages."));
        assert!(text.contains("Guidance:"));
        assert!(text
            .contains("Prefer report, evaluate, and replay until missing evidence is collected."));
        assert!(text.contains("Keep policy apply in dry-run mode until health is healthy."));
    }

    #[test]
    fn autonomous_preflight_blocked_text_surfaces_recovery_action() {
        let text = render_autonomous_preflight_blocked_text(&json!({
            "type": "autonomous_preflight_blocked",
            "operation": "policy apply",
            "status": "blocked",
            "next_action": "Run policy apply --dry-run and replay before persistent apply.",
            "safe_to_iterate": true,
            "safe_to_apply_policy": false,
            "health": {
                "blockers": [
                    "policy replay has unresolved anomalies"
                ]
            },
            "recommendations": [
                "Review policy ledger anomalies before applying routing changes."
            ]
        }));

        assert!(text.contains("Autonomous preflight blocked"));
        assert!(text.contains("Operation         policy apply"));
        assert!(text.contains(
            "Next action       Run policy apply --dry-run and replay before persistent apply."
        ));
        assert!(text.contains("Safety            iterate=true apply_policy=false"));
        assert!(text.contains("Blocker           policy replay has unresolved anomalies"));
        assert!(text.contains("Review policy ledger anomalies before applying routing changes."));
    }

    #[test]
    fn autonomous_health_checkpoint_text_surfaces_next_operator_step() {
        let text = render_autonomous_health_checkpoint_text(&json!({
            "status": "degraded",
            "headline": "Autonomous loop is usable but missing complete evidence.",
            "next_action": "Run daemon evaluate and replay before another apply.",
            "safe_to_iterate": true,
            "safe_to_apply_policy": false,
            "warnings": [
                "routing policy replay needs evidence"
            ],
            "blockers": []
        }));

        assert!(text.contains("Health checkpoint"));
        assert!(text.contains("Status            degraded"));
        assert!(
            text.contains("Next action       Run daemon evaluate and replay before another apply.")
        );
        assert!(text.contains("Safety            iterate=true apply_policy=false"));
        assert!(text.contains("Warning           routing policy replay needs evidence"));
        assert!(text.contains("Guidance:"));
        assert!(text.contains("Keep policy apply in dry-run mode until health is healthy."));
    }

    #[test]
    fn autonomous_replay_text_surfaces_review_guidance_for_policy_drift() {
        let text = render_autonomous_replay_text(&json!({
            "replay": {
                "considered_runs": 2,
                "changed_decisions": 1,
                "policy_recommendation": {
                    "action": "request_review",
                    "recommended_max_ticks": 1
                },
                "decisions": [{
                    "run_id": "run-1",
                    "observed_status": "blocked",
                    "replay_action": "request_review",
                    "changed": true
                }],
                "recommendations": []
            }
        }));

        assert!(text.contains("Next action       Review changed decisions before policy apply"));
        assert!(text.contains("! run-1: observed blocked, replay request_review"));
    }

    #[test]
    fn autonomous_daemon_report_text_keeps_policy_summary_readable() {
        let review = runtime::AutonomousPolicyReview {
            summary: runtime::AutonomousRunHistorySummary {
                runs_path: PathBuf::from("runs.jsonl"),
                considered_runs: 3,
                malformed_lines: 0,
                latest_run_id: Some("run-3".to_string()),
                status_counts: runtime::AutonomousRunStatusCounts {
                    idle: 1,
                    running: 1,
                    blocked: 1,
                },
                idle_rate: 0.33,
                running_rate: 0.33,
                blocked_rate: 0.34,
                average_ticks: 2.0,
                average_ticks_to_idle: Some(1.0),
                average_ticks_to_blocked: Some(3.0),
                consecutive_blocked_runs: 1,
                repeated_blocked_actions: Vec::new(),
                repeated_blocked_risks: Vec::new(),
                repeated_blocked_reasons: Vec::new(),
                repeated_task_types: Vec::new(),
            },
            recommendation: runtime::AutonomousPolicyRecommendation {
                requested_max_ticks: 4,
                recommended_max_ticks: 2,
                permission_mode: "read-only".to_string(),
                conservative_permission_mode: "read-only".to_string(),
                action: runtime::AutonomousPolicyAction::ReduceTicks,
                review_required: false,
                cool_down: false,
                reasons: vec!["blocked rate is elevated".to_string()],
            },
        };

        let text = render_autonomous_daemon_report_text(&review, Path::new("runs.jsonl"));

        assert!(text.contains("Daemon report"));
        assert!(text.contains("Runs              3"));
        assert!(text.contains("Policy            reduce_ticks"));
        assert!(text.contains("Max ticks         4 -> 2"));
        assert!(text.contains("Reason            blocked rate is elevated"));

        let summary = render_autonomous_policy_summary_text(&review);
        assert_eq!(
            summary,
            "  Policy            reduce_ticks: max_ticks 4 -> 2"
        );
    }

    fn registry_with_plugin_tool() -> GlobalToolRegistry {
        GlobalToolRegistry::with_plugin_tools(vec![PluginTool::new(
            "plugin-demo@external",
            "plugin-demo",
            PluginToolDefinition {
                name: "plugin_echo".to_string(),
                description: Some("Echo plugin payload".to_string()),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "message": { "type": "string" }
                    },
                    "required": ["message"],
                    "additionalProperties": false
                }),
            },
            "echo".to_string(),
            Vec::new(),
            PluginToolPermission::WorkspaceWrite,
            None,
        )])
        .expect("plugin tool registry should build")
    }

    #[test]
    fn opaque_provider_wrapper_surfaces_failure_class_session_and_trace() {
        let error = ApiError::Api {
            status: "500".parse().expect("status"),
            error_type: Some("api_error".to_string()),
            message: Some(
                "Something went wrong while processing your request. Please try again, or use /new to start a fresh session."
                    .to_string(),
            ),
            request_id: Some("req_jobdori_789".to_string()),
            body: String::new(),
            retryable: true,
        };

        let rendered = format_user_visible_api_error("session-issue-22", &error);
        assert!(rendered.contains("provider_internal"));
        assert!(rendered.contains("session session-issue-22"));
        assert!(rendered.contains("trace req_jobdori_789"));
    }

    #[test]
    fn retry_exhaustion_uses_retry_failure_class_for_generic_provider_wrapper() {
        let error = ApiError::RetriesExhausted {
            attempts: 3,
            last_error: Box::new(ApiError::Api {
                status: "502".parse().expect("status"),
                error_type: Some("api_error".to_string()),
                message: Some(
                    "Something went wrong while processing your request. Please try again, or use /new to start a fresh session."
                        .to_string(),
                ),
                request_id: Some("req_jobdori_790".to_string()),
                body: String::new(),
                retryable: true,
            }),
        };

        let rendered = format_user_visible_api_error("session-issue-22", &error);
        assert!(rendered.contains("provider_retry_exhausted"), "{rendered}");
        assert!(rendered.contains("session session-issue-22"));
        assert!(rendered.contains("trace req_jobdori_790"));
    }

    #[test]
    fn context_window_preflight_errors_render_recovery_steps() {
        let error = ApiError::ContextWindowExceeded {
            model: "Himalaya-sonnet-4-6".to_string(),
            estimated_input_tokens: 182_000,
            requested_output_tokens: 64_000,
            estimated_total_tokens: 246_000,
            context_window_tokens: 200_000,
        };

        let rendered = format_user_visible_api_error("session-issue-32", &error);
        assert!(rendered.contains("Context window blocked"), "{rendered}");
        assert!(rendered.contains("context_window_blocked"), "{rendered}");
        assert!(
            rendered.contains("Session          session-issue-32"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Model            Himalaya-sonnet-4-6"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Input estimate   ~182000 tokens (heuristic)"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Total estimate   ~246000 tokens (heuristic)"),
            "{rendered}"
        );
        assert!(rendered.contains("Compact          /compact"), "{rendered}");
        assert!(
            rendered.contains("Resume compact   Himalaya --resume session-issue-32 /compact"),
            "{rendered}"
        );
        assert!(
            rendered.contains("Fresh session    /clear --confirm"),
            "{rendered}"
        );
        assert!(rendered.contains("Reduce scope"), "{rendered}");
        assert!(rendered.contains("Retry            rerun"), "{rendered}");
    }

    #[test]
    fn provider_context_window_errors_are_reframed_with_same_guidance() {
        let error = ApiError::Api {
            status: "400".parse().expect("status"),
            error_type: Some("invalid_request_error".to_string()),
            message: Some(
                "This model's maximum context length is 200000 tokens, but your request used 230000 tokens."
                    .to_string(),
            ),
            request_id: Some("req_ctx_456".to_string()),
            body: String::new(),
            retryable: false,
        };

        let rendered = format_user_visible_api_error("session-issue-32", &error);
        assert!(rendered.contains("context_window_blocked"), "{rendered}");
        assert!(
            rendered.contains("Trace            req_ctx_456"),
            "{rendered}"
        );
        assert!(
            rendered
                .contains("Detail           This model's maximum context length is 200000 tokens"),
            "{rendered}"
        );
        assert!(rendered.contains("Compact          /compact"), "{rendered}");
        assert!(
            rendered.contains("Fresh session    /clear --confirm"),
            "{rendered}"
        );
    }

    #[test]
    fn retry_wrapped_context_window_errors_keep_recovery_guidance() {
        let error = ApiError::RetriesExhausted {
            attempts: 2,
            last_error: Box::new(ApiError::Api {
                status: "413".parse().expect("status"),
                error_type: Some("invalid_request_error".to_string()),
                message: Some("Request is too large for this model's context window.".to_string()),
                request_id: Some("req_ctx_retry_789".to_string()),
                body: String::new(),
                retryable: false,
            }),
        };

        let rendered = format_user_visible_api_error("session-issue-32", &error);
        assert!(rendered.contains("Context window blocked"), "{rendered}");
        assert!(rendered.contains("context_window_blocked"), "{rendered}");
        assert!(
            rendered.contains("Trace            req_ctx_retry_789"),
            "{rendered}"
        );
        assert!(
            rendered
                .contains("Detail           Request is too large for this model's context window."),
            "{rendered}"
        );
        assert!(rendered.contains("Compact          /compact"), "{rendered}");
        assert!(
            rendered.contains("Resume compact   Himalaya --resume session-issue-32 /compact"),
            "{rendered}"
        );
    }

    fn temp_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};

        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time should be after epoch")
            .as_nanos();
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("rusty-Himalaya-cli-{nanos}-{unique}"))
    }

    fn git(args: &[&str], cwd: &Path) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .expect("git command should run");
        assert!(
            status.success(),
            "git command failed: git {}",
            args.join(" ")
        );
    }

    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn sample_oauth_config(token_url: String) -> OAuthConfig {
        OAuthConfig {
            client_id: "runtime-client".to_string(),
            authorize_url: "https://console.test/oauth/authorize".to_string(),
            token_url,
            callback_port: Some(4545),
            manual_redirect_url: Some("https://console.test/oauth/callback".to_string()),
            scopes: vec!["org:create_api_key".to_string(), "user:profile".to_string()],
        }
    }

    fn spawn_token_server(response_body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let address = listener.local_addr().expect("local addr");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept connection");
            let mut buffer = [0_u8; 4096];
            let _ = stream.read(&mut buffer).expect("read request");
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        });
        format!("http://{address}/oauth/token")
    }

    fn with_current_dir<T>(cwd: &Path, f: impl FnOnce() -> T) -> T {
        let _guard = cwd_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = std::env::current_dir().expect("cwd should load");
        std::env::set_current_dir(cwd).expect("cwd should change");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        std::env::set_current_dir(previous).expect("cwd should restore");
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    fn write_plugin_fixture(root: &Path, name: &str, include_hooks: bool, include_lifecycle: bool) {
        fs::create_dir_all(root.join(".Himalaya-plugin")).expect("manifest dir");
        if include_hooks {
            fs::create_dir_all(root.join("hooks")).expect("hooks dir");
            fs::write(
                root.join("hooks").join("pre.sh"),
                "#!/bin/sh\nprintf 'plugin pre hook'\n",
            )
            .expect("write hook");
        }
        if include_lifecycle {
            fs::create_dir_all(root.join("lifecycle")).expect("lifecycle dir");
            fs::write(
                root.join("lifecycle").join("init.sh"),
                "#!/bin/sh\nprintf 'init\\n' >> lifecycle.log\n",
            )
            .expect("write init lifecycle");
            fs::write(
                root.join("lifecycle").join("shutdown.sh"),
                "#!/bin/sh\nprintf 'shutdown\\n' >> lifecycle.log\n",
            )
            .expect("write shutdown lifecycle");
        }

        let hooks = if include_hooks {
            ",\n  \"hooks\": {\n    \"PreToolUse\": [\"./hooks/pre.sh\"]\n  }"
        } else {
            ""
        };
        let lifecycle = if include_lifecycle {
            ",\n  \"lifecycle\": {\n    \"Init\": [\"./lifecycle/init.sh\"],\n    \"Shutdown\": [\"./lifecycle/shutdown.sh\"]\n  }"
        } else {
            ""
        };
        fs::write(
            root.join(".Himalaya-plugin").join("plugin.json"),
            format!(
                "{{\n  \"name\": \"{name}\",\n  \"version\": \"1.0.0\",\n  \"description\": \"runtime plugin fixture\"{hooks}{lifecycle}\n}}"
            ),
        )
        .expect("write plugin manifest");
    }
    fn default_permission_mode_for_tests() -> PermissionMode {
        super::default_permission_mode()
    }

    #[test]
    fn defaults_to_repl_when_no_args() {
        let _guard = env_lock();
        std::env::remove_var("RUSTY_Himalaya_PERMISSION_MODE");
        assert_eq!(
            parse_args(&[]).expect("args should parse"),
            CliAction::Repl {
                model: DEFAULT_MODEL.to_string(),
                allowed_tools: None,
                permission_mode: default_permission_mode_for_tests(),
                allow_broad_cwd: false,
                // Bare launch now auto-resumes the latest workspace session for
                // conversational continuity (falls back to fresh when none).
                resume_target: Some(PathBuf::from(LATEST_SESSION_REFERENCE)),
            }
        );
    }

    #[test]
    fn new_flag_forces_a_fresh_repl_session() {
        let _guard = env_lock();
        std::env::remove_var("RUSTY_Himalaya_PERMISSION_MODE");
        assert_eq!(
            parse_args(&["--new".to_string()]).expect("args should parse"),
            CliAction::Repl {
                model: DEFAULT_MODEL.to_string(),
                allowed_tools: None,
                permission_mode: default_permission_mode_for_tests(),
                allow_broad_cwd: false,
                resume_target: None,
            }
        );
    }

    #[test]
    fn default_permission_mode_uses_project_config_when_env_is_unset() {
        let _guard = env_lock();
        let root = temp_dir();
        let cwd = root.join("project");
        let config_home = root.join("config-home");
        std::fs::create_dir_all(cwd.join(".Himalaya")).expect("project config dir should exist");
        std::fs::create_dir_all(&config_home).expect("config home should exist");
        std::fs::write(
            cwd.join(".Himalaya").join("settings.json"),
            r#"{"permissionMode":"acceptEdits"}"#,
        )
        .expect("project config should write");

        let original_config_home = std::env::var("Himalaya_CONFIG_HOME").ok();
        let original_permission_mode = std::env::var("RUSTY_Himalaya_PERMISSION_MODE").ok();
        std::env::set_var("Himalaya_CONFIG_HOME", &config_home);
        std::env::remove_var("RUSTY_Himalaya_PERMISSION_MODE");

        let resolved = with_current_dir(&cwd, super::default_permission_mode);

        match original_config_home {
            Some(value) => std::env::set_var("Himalaya_CONFIG_HOME", value),
            None => std::env::remove_var("Himalaya_CONFIG_HOME"),
        }
        match original_permission_mode {
            Some(value) => std::env::set_var("RUSTY_Himalaya_PERMISSION_MODE", value),
            None => std::env::remove_var("RUSTY_Himalaya_PERMISSION_MODE"),
        }
        std::fs::remove_dir_all(root).expect("temp config root should clean up");

        assert_eq!(resolved, PermissionMode::WorkspaceWrite);
    }

    #[test]
    fn env_permission_mode_overrides_project_config_default() {
        let _guard = env_lock();
        let root = temp_dir();
        let cwd = root.join("project");
        let config_home = root.join("config-home");
        std::fs::create_dir_all(cwd.join(".Himalaya")).expect("project config dir should exist");
        std::fs::create_dir_all(&config_home).expect("config home should exist");
        std::fs::write(
            cwd.join(".Himalaya").join("settings.json"),
            r#"{"permissionMode":"acceptEdits"}"#,
        )
        .expect("project config should write");

        let original_config_home = std::env::var("Himalaya_CONFIG_HOME").ok();
        let original_permission_mode = std::env::var("RUSTY_Himalaya_PERMISSION_MODE").ok();
        std::env::set_var("Himalaya_CONFIG_HOME", &config_home);
        std::env::set_var("RUSTY_Himalaya_PERMISSION_MODE", "read-only");

        let resolved = with_current_dir(&cwd, super::default_permission_mode);

        match original_config_home {
            Some(value) => std::env::set_var("Himalaya_CONFIG_HOME", value),
            None => std::env::remove_var("Himalaya_CONFIG_HOME"),
        }
        match original_permission_mode {
            Some(value) => std::env::set_var("RUSTY_Himalaya_PERMISSION_MODE", value),
            None => std::env::remove_var("RUSTY_Himalaya_PERMISSION_MODE"),
        }
        std::fs::remove_dir_all(root).expect("temp config root should clean up");

        assert_eq!(resolved, PermissionMode::ReadOnly);
    }

    #[test]
    fn load_runtime_oauth_config_for_returns_none_without_project_config() {
        let _guard = env_lock();
        let root = temp_dir();
        std::fs::create_dir_all(&root).expect("workspace should exist");

        let oauth = super::load_runtime_oauth_config_for(&root)
            .expect("loading config should succeed when files are absent");

        std::fs::remove_dir_all(root).expect("temp workspace should clean up");

        assert_eq!(oauth, None);
    }

    #[test]
    fn resolve_cli_auth_source_uses_default_oauth_when_runtime_config_is_missing() {
        let _guard = env_lock();
        let workspace = temp_dir();
        let config_home = temp_dir();
        std::fs::create_dir_all(&workspace).expect("workspace should exist");
        std::fs::create_dir_all(&config_home).expect("config home should exist");

        let original_config_home = std::env::var("Himalaya_CONFIG_HOME").ok();
        let original_api_key = std::env::var("ANTHROPIC_API_KEY").ok();
        let original_auth_token = std::env::var("ANTHROPIC_AUTH_TOKEN").ok();
        std::env::set_var("Himalaya_CONFIG_HOME", &config_home);
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::env::remove_var("ANTHROPIC_AUTH_TOKEN");

        save_oauth_credentials(&runtime::OAuthTokenSet {
            access_token: "expired-access-token".to_string(),
            refresh_token: Some("refresh-token".to_string()),
            expires_at: Some(0),
            scopes: vec!["org:create_api_key".to_string(), "user:profile".to_string()],
        })
        .expect("save expired oauth credentials");

        let token_url = spawn_token_server(
            r#"{"access_token":"refreshed-access-token","refresh_token":"refreshed-refresh-token","expires_at":4102444800,"scopes":["org:create_api_key","user:profile"]}"#,
        );

        let auth =
            super::resolve_cli_auth_source_for_cwd(&workspace, || sample_oauth_config(token_url))
                .expect("expired saved oauth should refresh via default config");

        let stored = load_oauth_credentials()
            .expect("load stored credentials")
            .expect("stored credentials should exist");

        match original_config_home {
            Some(value) => std::env::set_var("Himalaya_CONFIG_HOME", value),
            None => std::env::remove_var("Himalaya_CONFIG_HOME"),
        }
        match original_api_key {
            Some(value) => std::env::set_var("ANTHROPIC_API_KEY", value),
            None => std::env::remove_var("ANTHROPIC_API_KEY"),
        }
        match original_auth_token {
            Some(value) => std::env::set_var("ANTHROPIC_AUTH_TOKEN", value),
            None => std::env::remove_var("ANTHROPIC_AUTH_TOKEN"),
        }
        std::fs::remove_dir_all(workspace).expect("temp workspace should clean up");
        std::fs::remove_dir_all(config_home).expect("temp config home should clean up");

        assert_eq!(auth.bearer_token(), Some("refreshed-access-token"));
        assert_eq!(stored.access_token, "refreshed-access-token");
        assert_eq!(
            stored.refresh_token.as_deref(),
            Some("refreshed-refresh-token")
        );
    }

    #[test]
    fn parses_prompt_subcommand() {
        let _guard = env_lock();
        std::env::remove_var("RUSTY_Himalaya_PERMISSION_MODE");
        let args = vec![
            "prompt".to_string(),
            "hello".to_string(),
            "world".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::Prompt {
                prompt: "hello world".to_string(),
                model: DEFAULT_MODEL.to_string(),
                output_format: CliOutputFormat::Text,
                allowed_tools: None,
                permission_mode: default_permission_mode_for_tests(),
                compact: false,
                base_commit: None,
                reasoning_effort: None,
                allow_broad_cwd: false,
                file_paths: Vec::new(),
            }
        );
    }

    #[test]
    fn parses_prompt_subcommand_with_file_flag_preserves_attachments() {
        let _guard = env_lock();
        std::env::remove_var("RUSTY_Himalaya_PERMISSION_MODE");
        let args = vec![
            "--file".to_string(),
            "/tmp/himalaya-attachment.txt".to_string(),
            "prompt".to_string(),
            "hello".to_string(),
        ];

        let parsed = parse_args(&args).expect("args should parse");

        assert_eq!(
            parsed,
            CliAction::Prompt {
                prompt: "hello".to_string(),
                model: DEFAULT_MODEL.to_string(),
                output_format: CliOutputFormat::Text,
                allowed_tools: None,
                permission_mode: default_permission_mode_for_tests(),
                compact: false,
                base_commit: None,
                reasoning_effort: None,
                allow_broad_cwd: false,
                file_paths: vec![PathBuf::from("/tmp/himalaya-attachment.txt")],
            }
        );
    }

    #[test]
    fn merge_prompt_with_stdin_returns_prompt_unchanged_when_no_pipe() {
        // given
        let prompt = "Review this";

        // when
        let merged = merge_prompt_with_stdin(prompt, None);

        // then
        assert_eq!(merged, "Review this");
    }

    #[test]
    fn merge_prompt_with_stdin_ignores_whitespace_only_pipe() {
        // given
        let prompt = "Review this";
        let piped = "   \n\t\n  ";

        // when
        let merged = merge_prompt_with_stdin(prompt, Some(piped));

        // then
        assert_eq!(merged, "Review this");
    }

    #[test]
    fn merge_prompt_with_stdin_appends_piped_content_as_context() {
        // given
        let prompt = "Review this";
        let piped = "fn main() { println!(\"hi\"); }\n";

        // when
        let merged = merge_prompt_with_stdin(prompt, Some(piped));

        // then
        assert_eq!(merged, "Review this\n\nfn main() { println!(\"hi\"); }");
    }

    #[test]
    fn merge_prompt_with_stdin_trims_surrounding_whitespace_on_pipe() {
        // given
        let prompt = "Summarize";
        let piped = "\n\n  some notes  \n\n";

        // when
        let merged = merge_prompt_with_stdin(prompt, Some(piped));

        // then
        assert_eq!(merged, "Summarize\n\nsome notes");
    }

    #[test]
    fn merge_prompt_with_stdin_returns_pipe_when_prompt_is_empty() {
        // given
        let prompt = "";
        let piped = "standalone body";

        // when
        let merged = merge_prompt_with_stdin(prompt, Some(piped));

        // then
        assert_eq!(merged, "standalone body");
    }

    #[test]
    fn parses_bare_prompt_and_json_output_flag() {
        let _guard = env_lock();
        std::env::remove_var("RUSTY_Himalaya_PERMISSION_MODE");
        let args = vec![
            "--output-format=json".to_string(),
            "--model".to_string(),
            "Himalaya-opus".to_string(),
            "explain".to_string(),
            "this".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::Prompt {
                prompt: "explain this".to_string(),
                model: "Himalaya-opus".to_string(),
                output_format: CliOutputFormat::Json,
                allowed_tools: None,
                permission_mode: default_permission_mode_for_tests(),
                compact: false,
                base_commit: None,
                reasoning_effort: None,
                allow_broad_cwd: false,
                file_paths: Vec::new(),
            }
        );
    }

    #[test]
    fn parses_compact_flag_for_prompt_mode() {
        // given a bare prompt invocation that includes the --compact flag
        let _guard = env_lock();
        std::env::remove_var("RUSTY_Himalaya_PERMISSION_MODE");
        let args = vec![
            "--compact".to_string(),
            "summarize".to_string(),
            "this".to_string(),
        ];

        // when parse_args interprets the flag
        let parsed = parse_args(&args).expect("args should parse");

        // then compact mode is propagated and other defaults stay unchanged
        assert_eq!(
            parsed,
            CliAction::Prompt {
                prompt: "summarize this".to_string(),
                model: DEFAULT_MODEL.to_string(),
                output_format: CliOutputFormat::Text,
                allowed_tools: None,
                permission_mode: default_permission_mode_for_tests(),
                compact: true,
                base_commit: None,
                reasoning_effort: None,
                allow_broad_cwd: false,
                file_paths: Vec::new(),
            }
        );
    }

    #[test]
    fn prompt_subcommand_defaults_compact_to_false() {
        // given a `prompt` subcommand invocation without --compact
        let _guard = env_lock();
        std::env::remove_var("RUSTY_Himalaya_PERMISSION_MODE");
        let args = vec!["prompt".to_string(), "hello".to_string()];

        // when parse_args runs
        let parsed = parse_args(&args).expect("args should parse");

        // then compact stays false (opt-in flag)
        match parsed {
            CliAction::Prompt { compact, .. } => assert!(!compact),
            other => panic!("expected Prompt action, got {other:?}"),
        }
    }

    #[test]
    fn prompt_mode_file_loading_turns_text_attachments_into_content_blocks() {
        let _guard = env_lock();
        let root = temp_dir();
        fs::create_dir_all(&root).expect("temp dir should be created");

        let file_path = root.join("attachment.txt");
        fs::write(&file_path, "Alpha line\nBeta line\n").expect("attachment should be written");

        let blocks = load_files_as_content_blocks(&[file_path.clone()], DEFAULT_MODEL)
            .expect("text attachment should load");

        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::Text { text } => {
                assert!(
                    text.starts_with(&format!("[File: {}]\n", file_path.display())),
                    "{text}"
                );
                assert!(text.contains("Alpha line"), "{text}");
                assert!(text.contains("Beta line"), "{text}");
            }
            other => panic!("expected text block for attachment, got {other:?}"),
        }

        fs::remove_dir_all(&root).expect("temp dir should clean up");
    }

    #[test]
    fn attachment_vision_probe_gate_only_matches_raster_images() {
        assert!(attachment_may_require_vision(Path::new("diagram.png")));
        assert!(attachment_may_require_vision(Path::new("photo.JPEG")));
        assert!(attachment_may_require_vision(Path::new("scan.webp")));

        assert!(!attachment_may_require_vision(Path::new("paper.pdf")));
        assert!(!attachment_may_require_vision(Path::new("notes.md")));
        assert!(!attachment_may_require_vision(Path::new("vector.svg")));
        assert!(!attachment_may_require_vision(Path::new("clip.mp4")));
    }

    #[test]
    fn extracted_attachment_warnings_cover_pdf_failure_modes() {
        let missing_text = extracted_attachment_warnings(
            Path::new("/tmp/scanned.pdf"),
            file_extract::PDF_NO_EXTRACTABLE_TEXT,
        );
        assert!(
            missing_text[0].contains("no extractable text found"),
            "{missing_text:?}"
        );

        let partial_text = extracted_attachment_warnings(
            Path::new("/tmp/partial.pdf"),
            &format!(
                "{} text was detected on only 1/20 pages]\nbody",
                file_extract::PDF_EXTRACTION_WARNING_PREFIX
            ),
        );
        assert!(
            partial_text[0].contains("partial or low-confidence PDF text extraction"),
            "{partial_text:?}"
        );

        let large_text = extracted_attachment_warnings(
            Path::new("/tmp/thesis.pdf"),
            &"x".repeat(LARGE_TEXT_ATTACHMENT_WARNING_CHARS),
        );
        assert!(
            large_text[0].contains("small-context local models may not read the whole attachment"),
            "{large_text:?}"
        );
    }

    #[test]
    fn resolves_model_aliases_in_args() {
        let _guard = env_lock();
        std::env::remove_var("RUSTY_Himalaya_PERMISSION_MODE");
        let args = vec![
            "--model".to_string(),
            "opus".to_string(),
            "explain".to_string(),
            "this".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::Prompt {
                prompt: "explain this".to_string(),
                model: "Himalaya-opus-4-6".to_string(),
                output_format: CliOutputFormat::Text,
                allowed_tools: None,
                permission_mode: default_permission_mode_for_tests(),
                compact: false,
                base_commit: None,
                reasoning_effort: None,
                allow_broad_cwd: false,
                file_paths: Vec::new(),
            }
        );
    }

    #[test]
    fn resolves_known_model_aliases() {
        assert_eq!(resolve_model_alias("opus"), "Himalaya-opus-4-6");
        assert_eq!(resolve_model_alias("sonnet"), "Himalaya-sonnet-4-6");
        assert_eq!(resolve_model_alias("haiku"), "Himalaya-haiku-4-5-20251213");
        assert_eq!(resolve_model_alias("Himalaya-opus"), "Himalaya-opus");
    }

    #[test]
    fn user_defined_aliases_resolve_before_provider_dispatch() {
        // given
        let _guard = env_lock();
        let root = temp_dir();
        let cwd = root.join("project");
        let config_home = root.join("config-home");
        std::fs::create_dir_all(cwd.join(".Himalaya")).expect("project config dir should exist");
        std::fs::create_dir_all(&config_home).expect("config home should exist");
        std::fs::write(
            cwd.join(".Himalaya").join("settings.json"),
            r#"{"aliases":{"fast":"Himalaya-haiku-4-5-20251213","smart":"opus","cheap":"grok-3-mini"}}"#,
        )
        .expect("project config should write");

        let original_config_home = std::env::var("Himalaya_CONFIG_HOME").ok();
        std::env::set_var("Himalaya_CONFIG_HOME", &config_home);

        // when
        let direct = with_current_dir(&cwd, || resolve_model_alias_with_config("fast"));
        let chained = with_current_dir(&cwd, || resolve_model_alias_with_config("smart"));
        let cross_provider = with_current_dir(&cwd, || resolve_model_alias_with_config("cheap"));
        let unknown = with_current_dir(&cwd, || resolve_model_alias_with_config("unknown-model"));
        let builtin = with_current_dir(&cwd, || resolve_model_alias_with_config("haiku"));

        match original_config_home {
            Some(value) => std::env::set_var("Himalaya_CONFIG_HOME", value),
            None => std::env::remove_var("Himalaya_CONFIG_HOME"),
        }
        std::fs::remove_dir_all(root).expect("temp config root should clean up");

        // then
        assert_eq!(direct, "Himalaya-haiku-4-5-20251213");
        assert_eq!(chained, "Himalaya-opus-4-6");
        assert_eq!(cross_provider, "grok-3-mini");
        assert_eq!(unknown, "unknown-model");
        assert_eq!(builtin, "Himalaya-haiku-4-5-20251213");
    }

    #[test]
    fn parses_version_flags_without_initializing_prompt_mode() {
        assert_eq!(
            parse_args(&["--version".to_string()]).expect("args should parse"),
            CliAction::Version {
                output_format: CliOutputFormat::Text,
            }
        );
        assert_eq!(
            parse_args(&["-V".to_string()]).expect("args should parse"),
            CliAction::Version {
                output_format: CliOutputFormat::Text,
            }
        );
    }

    #[test]
    fn parses_repl_resume_target() {
        let args = vec![
            "--repl".to_string(),
            "--resume".to_string(),
            "session-123".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::Repl {
                model: DEFAULT_MODEL.to_string(),
                allowed_tools: None,
                permission_mode: default_permission_mode_for_tests(),
                allow_broad_cwd: false,
                resume_target: Some(PathBuf::from("session-123")),
            }
        );
    }
    #[test]
    fn parses_permission_mode_flag() {
        let args = vec!["--permission-mode=read-only".to_string()];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::Repl {
                model: DEFAULT_MODEL.to_string(),
                allowed_tools: None,
                permission_mode: PermissionMode::ReadOnly,
                allow_broad_cwd: false,
                resume_target: Some(PathBuf::from(LATEST_SESSION_REFERENCE)),
            }
        );
    }

    #[test]
    fn dangerously_skip_permissions_flag_forces_danger_full_access_in_repl() {
        let _guard = env_lock();
        std::env::set_var("RUSTY_Himalaya_PERMISSION_MODE", "read-only");
        let args = vec!["--dangerously-skip-permissions".to_string()];
        let parsed = parse_args(&args).expect("args should parse");
        std::env::remove_var("RUSTY_Himalaya_PERMISSION_MODE");

        assert_eq!(
            parsed,
            CliAction::Repl {
                model: DEFAULT_MODEL.to_string(),
                allowed_tools: None,
                permission_mode: PermissionMode::DangerFullAccess,
                allow_broad_cwd: false,
                resume_target: Some(PathBuf::from(LATEST_SESSION_REFERENCE)),
            }
        );
    }

    #[test]
    fn dangerously_skip_permissions_flag_applies_to_prompt_subcommand() {
        let _guard = env_lock();
        std::env::set_var("RUSTY_Himalaya_PERMISSION_MODE", "read-only");
        let args = vec![
            "--dangerously-skip-permissions".to_string(),
            "prompt".to_string(),
            "do".to_string(),
            "the".to_string(),
            "thing".to_string(),
        ];
        let parsed = parse_args(&args).expect("args should parse");
        std::env::remove_var("RUSTY_Himalaya_PERMISSION_MODE");

        assert_eq!(
            parsed,
            CliAction::Prompt {
                prompt: "do the thing".to_string(),
                model: DEFAULT_MODEL.to_string(),
                output_format: CliOutputFormat::Text,
                allowed_tools: None,
                permission_mode: PermissionMode::DangerFullAccess,
                compact: false,
                base_commit: None,
                reasoning_effort: None,
                allow_broad_cwd: false,
                file_paths: Vec::new(),
            }
        );
    }

    #[test]
    fn parses_allowed_tools_flags_with_aliases_and_lists() {
        let _guard = env_lock();
        std::env::remove_var("RUSTY_Himalaya_PERMISSION_MODE");
        let args = vec![
            "--allowedTools".to_string(),
            "read,glob".to_string(),
            "--allowed-tools=write_file".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::Repl {
                model: DEFAULT_MODEL.to_string(),
                allowed_tools: Some(
                    ["glob_search", "read_file", "write_file"]
                        .into_iter()
                        .map(str::to_string)
                        .collect()
                ),
                permission_mode: default_permission_mode_for_tests(),
                allow_broad_cwd: false,
                resume_target: Some(PathBuf::from(LATEST_SESSION_REFERENCE)),
            }
        );
    }

    #[test]
    fn rejects_unknown_allowed_tools() {
        let error = parse_args(&["--allowedTools".to_string(), "teleport".to_string()])
            .expect_err("tool should be rejected");
        assert!(error.contains("unsupported tool in --allowedTools: teleport"));
    }

    #[test]
    fn parses_system_prompt_options() {
        let args = vec![
            "system-prompt".to_string(),
            "--cwd".to_string(),
            "/tmp/project".to_string(),
            "--date".to_string(),
            "2026-04-01".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::PrintSystemPrompt {
                cwd: PathBuf::from("/tmp/project"),
                date: "2026-04-01".to_string(),
                output_format: CliOutputFormat::Text,
            }
        );
    }

    #[test]
    fn parses_login_and_logout_subcommands() {
        assert_eq!(
            parse_args(&["login".to_string()]).expect("login should parse"),
            CliAction::Login {
                output_format: CliOutputFormat::Text,
            }
        );
        assert_eq!(
            parse_args(&["logout".to_string()]).expect("logout should parse"),
            CliAction::Logout {
                output_format: CliOutputFormat::Text,
            }
        );
        assert_eq!(
            parse_args(&["doctor".to_string()]).expect("doctor should parse"),
            CliAction::Doctor {
                output_format: CliOutputFormat::Text,
            }
        );
        assert_eq!(
            parse_args(&["state".to_string()]).expect("state should parse"),
            CliAction::State {
                output_format: CliOutputFormat::Text,
            }
        );
        assert_eq!(
            parse_args(&[
                "state".to_string(),
                "--output-format".to_string(),
                "json".to_string()
            ])
            .expect("state --output-format json should parse"),
            CliAction::State {
                output_format: CliOutputFormat::Json,
            }
        );
        assert_eq!(
            parse_task_cli_command(&[
                "list".to_string(),
                "--status".to_string(),
                "blocked".to_string()
            ])
            .expect("tasks list should parse"),
            TaskCliCommand::List {
                status: Some(runtime::TaskStatus::Blocked),
            }
        );
        assert_eq!(
            parse_args(&[
                "tasks".to_string(),
                "show".to_string(),
                "task-1".to_string()
            ])
            .expect("tasks show should parse"),
            CliAction::Tasks {
                command: TaskCliCommand::Show {
                    task_id: "task-1".to_string(),
                },
                output_format: CliOutputFormat::Text,
                model: DEFAULT_MODEL.to_string(),
                allowed_tools: None,
                permission_mode: crate::default_permission_mode(),
                compact: false,
                reasoning_effort: None,
            }
        );
        assert_eq!(
            parse_task_cli_command(&["status".to_string(), "task-1".to_string()])
                .expect("tasks status should parse"),
            TaskCliCommand::Status {
                task_id: "task-1".to_string(),
            }
        );
        assert_eq!(
            parse_task_cli_command(&["report".to_string(), "task-1".to_string()])
                .expect("tasks report should parse"),
            TaskCliCommand::Report {
                task_id: "task-1".to_string(),
            }
        );
        assert_eq!(
            parse_task_cli_command(&["review".to_string(), "task-1".to_string()])
                .expect("tasks review should parse"),
            TaskCliCommand::Review {
                task_id: "task-1".to_string(),
            }
        );
        assert_eq!(
            parse_task_cli_command(&[
                "resume".to_string(),
                "task-1".to_string(),
                "--from-node".to_string(),
                "node-1".to_string(),
                "continue".to_string(),
            ])
            .expect("tasks resume --from-node should parse"),
            TaskCliCommand::Resume {
                task_id: "task-1".to_string(),
                from_node: Some("node-1".to_string()),
                prompt: Some("continue".to_string()),
            }
        );
        assert_eq!(
            parse_task_cli_command(&[
                "retry".to_string(),
                "task-1".to_string(),
                "--node".to_string(),
                "node-1".to_string(),
            ])
            .expect("tasks retry should parse"),
            TaskCliCommand::Retry {
                task_id: "task-1".to_string(),
                node_id: "node-1".to_string(),
            }
        );
        assert_eq!(
            parse_task_cli_command(&[
                "verify".to_string(),
                "task-1".to_string(),
                "--node".to_string(),
                "node-1".to_string(),
                "cargo".to_string(),
                "test".to_string(),
            ])
            .expect("tasks verify should parse"),
            TaskCliCommand::Verify {
                task_id: "task-1".to_string(),
                node_id: "node-1".to_string(),
                command: "cargo test".to_string(),
            }
        );
        assert_eq!(
            parse_task_cli_command(&[
                "execute".to_string(),
                "task-1".to_string(),
                "--from-node".to_string(),
                "node-1".to_string(),
            ])
            .expect("tasks execute --from-node should parse"),
            TaskCliCommand::Execute {
                task_id: "task-1".to_string(),
                from_node: Some("node-1".to_string()),
            }
        );
        assert_eq!(
            parse_task_cli_command(&["recover".to_string(), "task-1".to_string()])
                .expect("tasks recover should parse"),
            TaskCliCommand::Recover {
                task_id: "task-1".to_string(),
            }
        );
        assert_eq!(
            parse_task_cli_command(&["verify".to_string(), "task-1".to_string()])
                .expect("tasks verify task should parse"),
            TaskCliCommand::VerifyTask {
                task_id: "task-1".to_string(),
            }
        );
        assert_eq!(
            parse_task_cli_command(&[
                "compact".to_string(),
                "task-1".to_string(),
                "--keep-last".to_string(),
                "42".to_string(),
            ])
            .expect("tasks compact should parse"),
            TaskCliCommand::Compact {
                task_id: "task-1".to_string(),
                keep_last: 42,
            }
        );
        assert_eq!(
            parse_task_cli_command(&[
                "packet".to_string(),
                "create".to_string(),
                "packet.json".to_string(),
            ])
            .expect("tasks packet create should parse"),
            TaskCliCommand::Packet {
                command: TaskPacketCliCommand::Create {
                    path: PathBuf::from("packet.json"),
                },
            }
        );
        assert_eq!(
            parse_task_cli_command(&[
                "packet".to_string(),
                "run".to_string(),
                "packet.json".to_string(),
            ])
            .expect("tasks packet run should parse"),
            TaskCliCommand::Packet {
                command: TaskPacketCliCommand::Run {
                    path: PathBuf::from("packet.json"),
                },
            }
        );
        assert_eq!(
            parse_task_cli_command(&[
                "packet".to_string(),
                "status".to_string(),
                "task-1".to_string(),
            ])
            .expect("tasks packet status should parse"),
            TaskCliCommand::Packet {
                command: TaskPacketCliCommand::Status {
                    task_id: "task-1".to_string(),
                },
            }
        );
        assert_eq!(
            parse_args(&[
                "tasks".to_string(),
                "packet".to_string(),
                "status".to_string(),
                "task-1".to_string(),
            ])
            .expect("tasks packet status should parse"),
            CliAction::Tasks {
                command: TaskCliCommand::Packet {
                    command: TaskPacketCliCommand::Status {
                        task_id: "task-1".to_string(),
                    },
                },
                output_format: CliOutputFormat::Text,
                model: DEFAULT_MODEL.to_string(),
                allowed_tools: None,
                permission_mode: crate::default_permission_mode(),
                compact: false,
                reasoning_effort: None,
            }
        );
        assert_eq!(
            parse_task_cli_command(&["scheduler".to_string(), "tick".to_string()])
                .expect("tasks scheduler tick should parse"),
            TaskCliCommand::Scheduler {
                command: TaskSchedulerCliCommand::Tick,
            }
        );
        assert_eq!(
            parse_task_cli_command(&["scheduler".to_string(), "queue".to_string()])
                .expect("tasks scheduler queue should parse"),
            TaskCliCommand::Scheduler {
                command: TaskSchedulerCliCommand::Queue,
            }
        );
        assert_eq!(
            parse_task_cli_command(&[
                "scheduler".to_string(),
                "explain".to_string(),
                "task-1".to_string(),
            ])
            .expect("tasks scheduler explain should parse"),
            TaskCliCommand::Scheduler {
                command: TaskSchedulerCliCommand::Explain {
                    task_id: "task-1".to_string(),
                },
            }
        );
        assert_eq!(
            parse_task_cli_command(&[
                "daemon".to_string(),
                "start".to_string(),
                "--max-ticks".to_string(),
                "3".to_string(),
            ])
            .expect("tasks daemon start should parse"),
            TaskCliCommand::Daemon {
                command: TaskDaemonCliCommand::Start { max_ticks: 3 },
            }
        );
        assert_eq!(
            parse_task_cli_command(&[
                "daemon".to_string(),
                "logs".to_string(),
                "--limit=5".to_string(),
            ])
            .expect("tasks daemon logs should parse"),
            TaskCliCommand::Daemon {
                command: TaskDaemonCliCommand::Logs { limit: 5 },
            }
        );
        assert_eq!(
            parse_task_cli_command(&[
                "daemon".to_string(),
                "report".to_string(),
                "--limit=7".to_string(),
                "--max-ticks".to_string(),
                "3".to_string(),
            ])
            .expect("tasks daemon report should parse"),
            TaskCliCommand::Daemon {
                command: TaskDaemonCliCommand::Report {
                    limit: 7,
                    max_ticks: 3
                },
            }
        );
        assert_eq!(
            parse_task_cli_command(&[
                "daemon".to_string(),
                "evaluate".to_string(),
                "--limit".to_string(),
                "8".to_string(),
                "--max-ticks=4".to_string(),
            ])
            .expect("tasks daemon evaluate should parse"),
            TaskCliCommand::Daemon {
                command: TaskDaemonCliCommand::Evaluate {
                    limit: 8,
                    max_ticks: 4
                },
            }
        );
        assert_eq!(
            parse_task_cli_command(&[
                "daemon".to_string(),
                "replay".to_string(),
                "--limit=9".to_string(),
            ])
            .expect("tasks daemon replay should parse"),
            TaskCliCommand::Daemon {
                command: TaskDaemonCliCommand::Replay {
                    limit: 9,
                    max_ticks: 1
                },
            }
        );
        assert_eq!(
            parse_task_cli_command(&["daemon".to_string(), "stop".to_string()])
                .expect("tasks daemon stop should parse"),
            TaskCliCommand::Daemon {
                command: TaskDaemonCliCommand::Stop,
            }
        );
        assert_eq!(
            parse_benchmark_cli_command(&[
                "autonomous".to_string(),
                "--record".to_string(),
                "--limit=6".to_string(),
                "--max-ticks".to_string(),
                "2".to_string(),
                "--optimize-routes".to_string(),
            ])
            .expect("benchmark autonomous should parse"),
            BenchmarkCliCommand::Autonomous {
                record: true,
                limit: 6,
                max_ticks: 2,
                optimize_routes: true,
            }
        );
        assert_eq!(
            parse_route_cli_command(&["feedback".to_string(), "summary".to_string()])
                .expect("routes feedback summary should parse"),
            RouteCliCommand::FeedbackSummary
        );
        assert_eq!(
            parse_route_cli_command(&[
                "optimize".to_string(),
                "--min-samples=3".to_string(),
                "--threshold-percent".to_string(),
                "60".to_string(),
            ])
            .expect("routes optimize should parse"),
            RouteCliCommand::Optimize {
                min_samples: 3,
                threshold_percent: 60,
            }
        );
        assert_eq!(
            parse_route_cli_command(&[
                "replay".to_string(),
                "--min-samples".to_string(),
                "4".to_string(),
            ])
            .expect("routes replay should parse"),
            RouteCliCommand::Replay {
                min_samples: 4,
                threshold_percent: 50,
            }
        );
        assert_eq!(
            parse_route_cli_command(&["list".to_string()]).expect("routes list should parse"),
            RouteCliCommand::List
        );
        assert_eq!(
            parse_route_cli_command(
                &["propose".to_string(), "--threshold-percent=70".to_string(),]
            )
            .expect("routes propose should parse"),
            RouteCliCommand::Propose {
                min_samples: 2,
                threshold_percent: 70,
            }
        );
        assert_eq!(
            parse_route_cli_command(&[
                "apply".to_string(),
                "route-proposal-1".to_string(),
                "--dry-run".to_string(),
            ])
            .expect("routes apply should parse"),
            RouteCliCommand::Apply {
                proposal_id: "route-proposal-1".to_string(),
                dry_run: true,
            }
        );
        assert_eq!(
            parse_route_cli_command(&["rollback".to_string(), "route-proposal-1".to_string(),])
                .expect("routes rollback should parse"),
            RouteCliCommand::Rollback {
                proposal_id: "route-proposal-1".to_string(),
            }
        );
        assert_eq!(
            parse_args(&["routes".to_string(), "summary".to_string()])
                .expect("routes summary should parse"),
            CliAction::Routes {
                command: RouteCliCommand::FeedbackSummary,
                output_format: CliOutputFormat::Text,
                model: DEFAULT_MODEL.to_string(),
            }
        );
        assert_eq!(
            parse_policy_cli_command(&["review".to_string()]).expect("policy review should parse"),
            PolicyCliCommand::Review {
                limit: 20,
                max_ticks: 1,
                record: true,
            }
        );
        assert_eq!(
            parse_policy_cli_command(&[
                "review".to_string(),
                "--limit=7".to_string(),
                "--max-ticks".to_string(),
                "3".to_string(),
                "--no-record".to_string(),
            ])
            .expect("policy review args should parse"),
            PolicyCliCommand::Review {
                limit: 7,
                max_ticks: 3,
                record: false,
            }
        );
        assert_eq!(
            parse_policy_cli_command(&[
                "ledger".to_string(),
                "--limit".to_string(),
                "5".to_string()
            ])
            .expect("policy ledger should parse"),
            PolicyCliCommand::Ledger { limit: 5 }
        );
        assert_eq!(
            parse_policy_cli_command(&["replay".to_string(), "--limit=50".to_string()])
                .expect("policy replay should parse"),
            PolicyCliCommand::Replay { limit: 50 }
        );
        assert_eq!(
            parse_policy_cli_command(&[
                "plan".to_string(),
                "--limit".to_string(),
                "4".to_string(),
                "--max-ticks=2".to_string()
            ])
            .expect("policy plan should parse"),
            PolicyCliCommand::Plan {
                limit: 4,
                max_ticks: 2,
            }
        );
        assert_eq!(
            parse_policy_cli_command(&[
                "apply".to_string(),
                "--dry-run".to_string(),
                "--domain".to_string(),
                "routing".to_string(),
                "--proposal-id=route-policy-1".to_string()
            ])
            .expect("policy apply should parse"),
            PolicyCliCommand::Apply {
                limit: 20,
                max_ticks: 1,
                domain: Some(runtime::PolicyDomain::Routing),
                proposal_id: Some("route-policy-1".to_string()),
                dry_run: true,
            }
        );
        assert_eq!(
            parse_policy_cli_command(&[
                "rollback".to_string(),
                "--domain=routing".to_string(),
                "--proposal-id".to_string(),
                "route-policy-1".to_string()
            ])
            .expect("policy rollback should parse"),
            PolicyCliCommand::Rollback {
                limit: 20,
                max_ticks: 1,
                domain: Some(runtime::PolicyDomain::Routing),
                proposal_id: Some("route-policy-1".to_string()),
            }
        );
        assert_eq!(
            parse_args(&[
                "policy".to_string(),
                "review".to_string(),
                "--no-record".to_string()
            ])
            .expect("policy review should parse"),
            CliAction::Policy {
                command: PolicyCliCommand::Review {
                    limit: 20,
                    max_ticks: 1,
                    record: false,
                },
                output_format: CliOutputFormat::Text,
                permission_mode: crate::default_permission_mode(),
            }
        );
        assert_eq!(
            parse_worker_cli_command(&[
                "spawn".to_string(),
                "--cwd".to_string(),
                "/tmp/work".to_string(),
                "--trusted-root=/tmp/work".to_string(),
                "--isolate-worktree".to_string(),
                "--worktree-root".to_string(),
                "/tmp/isolated".to_string(),
                "--".to_string(),
                "sh".to_string(),
                "-c".to_string(),
                "exit 0".to_string(),
            ])
            .expect("workers spawn should parse"),
            WorkerCliCommand::Spawn {
                cwd: Some(PathBuf::from("/tmp/work")),
                trusted_roots: vec!["/tmp/work".to_string()],
                isolate_worktree: true,
                worktree_root: Some(PathBuf::from("/tmp/isolated")),
                command: vec!["sh".to_string(), "-c".to_string(), "exit 0".to_string()],
            }
        );
        assert_eq!(
            parse_worker_cli_command(&["probe".to_string(), "worker-1".to_string()])
                .expect("workers probe should parse"),
            WorkerCliCommand::Probe {
                worker_id: "worker-1".to_string(),
            }
        );
        assert_eq!(
            parse_args(&[
                "workers".to_string(),
                "spawn".to_string(),
                "--".to_string(),
                "tool".to_string(),
                "--help".to_string(),
            ])
            .expect("workers spawn command flags should be forwarded"),
            CliAction::Workers {
                command: WorkerCliCommand::Spawn {
                    cwd: None,
                    trusted_roots: Vec::new(),
                    isolate_worktree: false,
                    worktree_root: None,
                    command: vec!["tool".to_string(), "--help".to_string()],
                },
                output_format: CliOutputFormat::Text,
            }
        );
        assert_eq!(
            parse_worker_cli_command(&["cleanup".to_string(), "--stale".to_string()])
                .expect("workers cleanup --stale should parse"),
            WorkerCliCommand::Cleanup {
                include_stale: true,
            }
        );
        assert!(parse_worker_cli_command(&["spawn".to_string()]).is_err());
        assert!(parse_worker_cli_command(&["probe".to_string()]).is_err());
        assert!(
            parse_task_cli_command(&["scheduler".to_string(), "unknown".to_string(),]).is_err()
        );
        assert!(parse_task_cli_command(&["packet".to_string()]).is_err());
        assert!(parse_task_cli_command(&[
            "packet".to_string(),
            "create".to_string(),
            "a.json".to_string(),
            "extra".to_string(),
        ])
        .is_err());
        assert_eq!(
            parse_args(&["maturity-matrix".to_string()]).expect("maturity matrix should parse"),
            CliAction::MaturityMatrix {
                output_format: CliOutputFormat::Text,
            }
        );
        assert_eq!(
            parse_args(&["plan".to_string(), "fix".to_string(), "tests".to_string()])
                .expect("plan should parse"),
            CliAction::Plan {
                prompt: "fix tests".to_string(),
                output_format: CliOutputFormat::Text,
                permission_mode: crate::default_permission_mode(),
            }
        );
        assert_eq!(
            parse_args(&[
                "/plan".to_string(),
                "add".to_string(),
                "task".to_string(),
                "board".to_string(),
            ])
            .expect("direct slash plan should parse"),
            CliAction::Plan {
                prompt: "add task board".to_string(),
                output_format: CliOutputFormat::Text,
                permission_mode: crate::default_permission_mode(),
            }
        );
        assert!(
            slash_command_status("plan") == SlashCommandStatus::Implemented,
            "/plan should be implemented, not hidden as a stub"
        );
        let plan_value = build_plan_output("fix tests and update docs", PermissionMode::ReadOnly)
            .expect("plan output should build");
        assert_eq!(plan_value["type"], "plan");
        assert!(plan_value["plan"]["steps"]
            .as_array()
            .is_some_and(|steps| steps.len() >= 2));
        let matrix_value = maturity_matrix_value();
        assert_eq!(matrix_value["type"], "maturity_matrix");
        assert!(matrix_value["tool_count"].as_u64().unwrap_or_default() > 0);
        assert_eq!(matrix_value["release_readiness"]["status"], "converging");
        assert!(matrix_value["release_readiness"]["stable_capabilities"]
            .as_array()
            .is_some_and(
                |capabilities| capabilities.iter().any(|capability| capability
                    .as_str()
                    .is_some_and(|value| value.contains("health view")))
            ));
        assert!(matrix_value["release_readiness"]["stable_capabilities"]
            .as_array()
            .is_some_and(
                |capabilities| capabilities.iter().any(|capability| capability
                    .as_str()
                    .is_some_and(|value| value.contains("preflight gates")))
            ));
        assert!(matrix_value["release_readiness"]["operator_workflow"]
            .as_array()
            .is_some_and(|steps| steps.iter().any(|step| step
                .as_str()
                .is_some_and(|value| value.contains("autonomous_preflight_blocked")))));
        let matrix_text = render_maturity_matrix_text(&matrix_value);
        assert!(matrix_text.contains("Release readiness:"));
        assert!(matrix_text.contains("Operator workflow"));
        assert!(matrix_text.contains("tasks daemon start is preflight-blocked"));
        assert!(matrix_text.contains("stream-json schema remains backward compatible"));
        assert!(
            matrix_value["implemented_slash_command_count"]
                .as_u64()
                .unwrap_or_default()
                > 0
        );
        assert!(matrix_value["slash_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command["name"] == "plan" && command["implemented"] == true));
        assert_eq!(
            parse_args(&["agents".to_string()]).expect("agents should parse"),
            CliAction::Agents {
                args: None,
                output_format: CliOutputFormat::Text
            }
        );
        assert_eq!(
            parse_args(&["mcp".to_string()]).expect("mcp should parse"),
            CliAction::Mcp {
                args: None,
                output_format: CliOutputFormat::Text,
            }
        );
        assert_eq!(
            parse_args(&["skills".to_string()]).expect("skills should parse"),
            CliAction::Skills {
                args: None,
                output_format: CliOutputFormat::Text,
            }
        );
        assert_eq!(
            parse_args(&[
                "skills".to_string(),
                "help".to_string(),
                "overview".to_string()
            ])
            .expect("skills help overview should invoke"),
            CliAction::Prompt {
                prompt: "$help overview".to_string(),
                model: DEFAULT_MODEL.to_string(),
                output_format: CliOutputFormat::Text,
                allowed_tools: None,
                permission_mode: crate::default_permission_mode(),
                compact: false,
                base_commit: None,
                reasoning_effort: None,
                allow_broad_cwd: false,
                file_paths: Vec::new(),
            }
        );
        assert_eq!(
            parse_args(&["agents".to_string(), "--help".to_string()])
                .expect("agents help should parse"),
            CliAction::Agents {
                args: Some("--help".to_string()),
                output_format: CliOutputFormat::Text,
            }
        );
    }

    #[test]
    fn local_command_help_flags_stay_on_the_local_parser_path() {
        assert_eq!(
            parse_args(&["status".to_string(), "--help".to_string()])
                .expect("status help should parse"),
            CliAction::HelpTopic(LocalHelpTopic::Status)
        );
        assert_eq!(
            parse_args(&["sandbox".to_string(), "-h".to_string()])
                .expect("sandbox help should parse"),
            CliAction::HelpTopic(LocalHelpTopic::Sandbox)
        );
        assert_eq!(
            parse_args(&["doctor".to_string(), "--help".to_string()])
                .expect("doctor help should parse"),
            CliAction::HelpTopic(LocalHelpTopic::Doctor)
        );
    }

    #[test]
    fn parses_single_word_command_aliases_without_falling_back_to_prompt_mode() {
        let _guard = env_lock();
        std::env::remove_var("RUSTY_Himalaya_PERMISSION_MODE");
        assert_eq!(
            parse_args(&["help".to_string()]).expect("help should parse"),
            CliAction::Help {
                output_format: CliOutputFormat::Text,
            }
        );
        assert_eq!(
            parse_args(&["version".to_string()]).expect("version should parse"),
            CliAction::Version {
                output_format: CliOutputFormat::Text,
            }
        );
        assert_eq!(
            parse_args(&["status".to_string()]).expect("status should parse"),
            CliAction::Status {
                model: DEFAULT_MODEL.to_string(),
                permission_mode: default_permission_mode_for_tests(),
                output_format: CliOutputFormat::Text,
            }
        );
        assert_eq!(
            parse_args(&["sandbox".to_string()]).expect("sandbox should parse"),
            CliAction::Sandbox {
                output_format: CliOutputFormat::Text,
            }
        );
    }

    #[test]
    fn parses_bare_export_subcommand_targeting_latest_session() {
        // given
        let _guard = env_lock();
        std::env::remove_var("RUSTY_Himalaya_PERMISSION_MODE");
        let args = vec!["export".to_string()];

        // when
        let parsed = parse_args(&args).expect("bare export should parse");

        // then
        assert_eq!(
            parsed,
            CliAction::Export {
                session_reference: LATEST_SESSION_REFERENCE.to_string(),
                output_path: None,
                output_format: CliOutputFormat::Text,
            }
        );
    }

    #[test]
    fn parses_export_subcommand_with_positional_output_path() {
        // given
        let args = vec!["export".to_string(), "conversation.md".to_string()];

        // when
        let parsed = parse_args(&args).expect("export with path should parse");

        // then
        assert_eq!(
            parsed,
            CliAction::Export {
                session_reference: LATEST_SESSION_REFERENCE.to_string(),
                output_path: Some(PathBuf::from("conversation.md")),
                output_format: CliOutputFormat::Text,
            }
        );
    }

    #[test]
    fn parses_export_subcommand_with_session_and_output_flags() {
        // given
        let args = vec![
            "export".to_string(),
            "--session".to_string(),
            "session-alpha".to_string(),
            "--output".to_string(),
            "/tmp/share.md".to_string(),
        ];

        // when
        let parsed = parse_args(&args).expect("export flags should parse");

        // then
        assert_eq!(
            parsed,
            CliAction::Export {
                session_reference: "session-alpha".to_string(),
                output_path: Some(PathBuf::from("/tmp/share.md")),
                output_format: CliOutputFormat::Text,
            }
        );
    }

    #[test]
    fn parses_export_subcommand_with_inline_flag_values() {
        // given
        let args = vec![
            "export".to_string(),
            "--session=session-beta".to_string(),
            "--output=/tmp/beta.md".to_string(),
        ];

        // when
        let parsed = parse_args(&args).expect("export inline flags should parse");

        // then
        assert_eq!(
            parsed,
            CliAction::Export {
                session_reference: "session-beta".to_string(),
                output_path: Some(PathBuf::from("/tmp/beta.md")),
                output_format: CliOutputFormat::Text,
            }
        );
    }

    #[test]
    fn parses_export_subcommand_with_json_output_format() {
        // given
        let args = vec![
            "--output-format=json".to_string(),
            "export".to_string(),
            "/tmp/notes.md".to_string(),
        ];

        // when
        let parsed = parse_args(&args).expect("json export should parse");

        // then
        assert_eq!(
            parsed,
            CliAction::Export {
                session_reference: LATEST_SESSION_REFERENCE.to_string(),
                output_path: Some(PathBuf::from("/tmp/notes.md")),
                output_format: CliOutputFormat::Json,
            }
        );
    }

    #[test]
    fn rejects_unknown_export_options_with_helpful_message() {
        // given
        let args = vec!["export".to_string(), "--bogus".to_string()];

        // when
        let error = parse_args(&args).expect_err("unknown export option should fail");

        // then
        assert!(error.contains("unknown export option: --bogus"));
    }

    #[test]
    fn rejects_export_with_extra_positional_after_path() {
        // given
        let args = vec![
            "export".to_string(),
            "first.md".to_string(),
            "second.md".to_string(),
        ];

        // when
        let error = parse_args(&args).expect_err("multiple positionals should fail");

        // then
        assert!(error.contains("unexpected export argument: second.md"));
    }

    #[test]
    fn parse_export_args_helper_defaults_to_latest_reference_and_no_output() {
        // given
        let args: Vec<String> = vec![];

        // when
        let parsed = parse_export_args(&args, CliOutputFormat::Text)
            .expect("empty export args should parse");

        // then
        assert_eq!(
            parsed,
            CliAction::Export {
                session_reference: LATEST_SESSION_REFERENCE.to_string(),
                output_path: None,
                output_format: CliOutputFormat::Text,
            }
        );
    }

    #[test]
    fn render_session_markdown_includes_header_and_summarized_tool_calls() {
        // given
        let mut session = Session::new();
        session.session_id = "session-export-test".to_string();
        session.messages = vec![
            ConversationMessage::user_text("How do I list files?"),
            ConversationMessage::assistant(vec![
                ContentBlock::Text {
                    text: "I'll run a tool.".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "toolu_abcdefghijklmnop".to_string(),
                    name: "bash".to_string(),
                    input: r#"{"command":"ls -la"}"#.to_string(),
                },
            ]),
            ConversationMessage {
                role: MessageRole::Tool,
                blocks: vec![ContentBlock::ToolResult {
                    tool_use_id: "toolu_abcdefghijklmnop".to_string(),
                    tool_name: "bash".to_string(),
                    output: "total 8\ndrwxr-xr-x  2 user staff   64 Apr  7 12:00 .".to_string(),
                    is_error: false,
                }],
                usage: None,
            },
        ];

        // when
        let markdown = render_session_markdown(
            &session,
            "session-export-test",
            std::path::Path::new("/tmp/sessions/session-export-test.jsonl"),
        );

        // then
        assert!(markdown.starts_with("# Conversation Export"));
        assert!(markdown.contains("- **Session**: `session-export-test`"));
        assert!(markdown.contains("- **Messages**: 3"));
        assert!(markdown.contains("## 1. User"));
        assert!(markdown.contains("How do I list files?"));
        assert!(markdown.contains("## 2. Assistant"));
        assert!(markdown.contains("**Tool call** `bash`"));
        assert!(markdown.contains("toolu_abcdef…"));
        assert!(markdown.contains("ls -la"));
        assert!(markdown.contains("## 3. Tool"));
        assert!(markdown.contains("**Tool result** `bash`"));
        assert!(markdown.contains("ok"));
        assert!(markdown.contains("total 8"));
    }

    #[test]
    fn render_session_markdown_marks_tool_errors_and_skips_empty_summaries() {
        // given
        let mut session = Session::new();
        session.session_id = "errs".to_string();
        session.messages = vec![ConversationMessage {
            role: MessageRole::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "short".to_string(),
                tool_name: "read_file".to_string(),
                output: "   ".to_string(),
                is_error: true,
            }],
            usage: None,
        }];

        // when
        let markdown =
            render_session_markdown(&session, "errs", std::path::Path::new("errs.jsonl"));

        // then
        assert!(markdown.contains("**Tool result** `read_file` _(id `short`, error)_"));
        // an empty summary should not produce a stray blockquote line
        assert!(!markdown.contains("> \n"));
    }

    #[test]
    fn summarize_tool_payload_for_markdown_compacts_json_and_truncates_overflow() {
        // given
        let json_payload = r#"{
            "command":   "ls -la",
            "cwd": "/tmp"
        }"#;
        let long_payload = "a".repeat(600);

        // when
        let compacted = summarize_tool_payload_for_markdown(json_payload);
        let truncated = summarize_tool_payload_for_markdown(&long_payload);

        // then
        assert_eq!(compacted, r#"{"command":"ls -la","cwd":"/tmp"}"#);
        assert!(truncated.ends_with('…'));
        assert!(truncated.chars().count() <= 281);
    }

    #[test]
    fn short_tool_id_truncates_long_identifiers_with_ellipsis() {
        // given
        let long = "toolu_01ABCDEFGHIJKLMN";
        let short = "tool_1";

        // when
        let trimmed_long = short_tool_id(long);
        let trimmed_short = short_tool_id(short);

        // then
        assert_eq!(trimmed_long, "toolu_01ABCD…");
        assert_eq!(trimmed_short, "tool_1");
    }

    #[test]
    fn parses_json_output_for_mcp_and_skills_commands() {
        assert_eq!(
            parse_args(&["--output-format=json".to_string(), "mcp".to_string()])
                .expect("json mcp should parse"),
            CliAction::Mcp {
                args: None,
                output_format: CliOutputFormat::Json,
            }
        );
        assert_eq!(
            parse_args(&[
                "--output-format=json".to_string(),
                "/skills".to_string(),
                "help".to_string(),
            ])
            .expect("json /skills help should parse"),
            CliAction::Skills {
                args: Some("help".to_string()),
                output_format: CliOutputFormat::Json,
            }
        );
    }

    #[test]
    fn single_word_slash_command_names_return_guidance_instead_of_hitting_prompt_mode() {
        let error = parse_args(&["cost".to_string()]).expect_err("cost should return guidance");
        assert!(error.contains("slash command"));
        assert!(error.contains("/cost"));
    }

    #[test]
    fn multi_word_prompt_still_uses_shorthand_prompt_mode() {
        let _guard = env_lock();
        std::env::remove_var("RUSTY_Himalaya_PERMISSION_MODE");
        // Input is ["help", "me", "debug"] so the joined prompt shorthand
        // must be "help me debug". A previous batch accidentally rewrote
        // the expected string to "$help overview" (copy-paste slip).
        assert_eq!(
            parse_args(&["help".to_string(), "me".to_string(), "debug".to_string()])
                .expect("prompt shorthand should still work"),
            CliAction::Prompt {
                prompt: "help me debug".to_string(),
                model: DEFAULT_MODEL.to_string(),
                output_format: CliOutputFormat::Text,
                allowed_tools: None,
                permission_mode: crate::default_permission_mode(),
                compact: false,
                base_commit: None,
                reasoning_effort: None,
                allow_broad_cwd: false,
                file_paths: Vec::new(),
            }
        );
    }

    #[test]
    fn parses_direct_agents_mcp_and_skills_slash_commands() {
        assert_eq!(
            parse_args(&["/agents".to_string()]).expect("/agents should parse"),
            CliAction::Agents {
                args: None,
                output_format: CliOutputFormat::Text
            }
        );
        assert_eq!(
            parse_args(&["/mcp".to_string(), "show".to_string(), "demo".to_string()])
                .expect("/mcp show demo should parse"),
            CliAction::Mcp {
                args: Some("show demo".to_string()),
                output_format: CliOutputFormat::Text,
            }
        );
        assert_eq!(
            parse_args(&["/skills".to_string()]).expect("/skills should parse"),
            CliAction::Skills {
                args: None,
                output_format: CliOutputFormat::Text,
            }
        );
        assert_eq!(
            parse_args(&["/skill".to_string()]).expect("/skill should parse"),
            CliAction::Skills {
                args: None,
                output_format: CliOutputFormat::Text,
            }
        );
        assert_eq!(
            parse_args(&["/skills".to_string(), "help".to_string()])
                .expect("/skills help should parse"),
            CliAction::Skills {
                args: Some("help".to_string()),
                output_format: CliOutputFormat::Text,
            }
        );
        assert_eq!(
            parse_args(&["/skill".to_string(), "list".to_string()])
                .expect("/skill list should parse"),
            CliAction::Skills {
                args: Some("list".to_string()),
                output_format: CliOutputFormat::Text,
            }
        );
        assert_eq!(
            parse_args(&[
                "/skills".to_string(),
                "help".to_string(),
                "overview".to_string()
            ])
            .expect("/skills help overview should invoke"),
            CliAction::Prompt {
                prompt: "$help overview".to_string(),
                model: DEFAULT_MODEL.to_string(),
                output_format: CliOutputFormat::Text,
                allowed_tools: None,
                permission_mode: crate::default_permission_mode(),
                compact: false,
                base_commit: None,
                reasoning_effort: None,
                allow_broad_cwd: false,
                file_paths: Vec::new(),
            }
        );
        assert_eq!(
            parse_args(&[
                "/skills".to_string(),
                "install".to_string(),
                "./fixtures/help-skill".to_string(),
            ])
            .expect("/skills install should parse"),
            CliAction::Skills {
                args: Some("install ./fixtures/help-skill".to_string()),
                output_format: CliOutputFormat::Text,
            }
        );
        assert_eq!(
            parse_args(&["/skills".to_string(), "/test".to_string()])
                .expect("/skills /test should normalize to a single skill prompt prefix"),
            CliAction::Prompt {
                prompt: "$test".to_string(),
                model: DEFAULT_MODEL.to_string(),
                output_format: CliOutputFormat::Text,
                allowed_tools: None,
                permission_mode: crate::default_permission_mode(),
                compact: false,
                base_commit: None,
                reasoning_effort: None,
                allow_broad_cwd: false,
                file_paths: Vec::new(),
            }
        );
        assert_eq!(
            parse_args(&[
                "/tasks".to_string(),
                "show".to_string(),
                "task-1".to_string()
            ])
            .expect("/tasks show should parse"),
            CliAction::Tasks {
                command: TaskCliCommand::Show {
                    task_id: "task-1".to_string(),
                },
                output_format: CliOutputFormat::Text,
                model: DEFAULT_MODEL.to_string(),
                allowed_tools: None,
                permission_mode: crate::default_permission_mode(),
                compact: false,
                reasoning_effort: None,
            }
        );
        assert_eq!(
            parse_args(&[
                "/cron".to_string(),
                "add".to_string(),
                "*/15".to_string(),
                "*".to_string(),
                "*".to_string(),
                "*".to_string(),
                "*".to_string(),
                "check".to_string(),
                "health".to_string(),
            ])
            .expect("/cron add should parse"),
            CliAction::Cron {
                command: CronCliCommand::Add {
                    schedule: "*/15 * * * *".to_string(),
                    prompt: "check health".to_string(),
                    description: None,
                },
                output_format: CliOutputFormat::Text,
                permission_mode: PermissionMode::ReadOnly,
            }
        );

        assert_eq!(
            parse_args(&[
                "--permission-mode".to_string(),
                "workspace-write".to_string(),
                "cron".to_string(),
                "run".to_string(),
                "--max-fires=2".to_string(),
                "--max-ticks".to_string(),
                "3".to_string(),
            ])
            .expect("cron run should parse"),
            CliAction::Cron {
                command: CronCliCommand::Run {
                    max_fires: 2,
                    max_ticks: 3,
                },
                output_format: CliOutputFormat::Text,
                permission_mode: PermissionMode::WorkspaceWrite,
            }
        );

        let error = parse_args(&["/status".to_string()])
            .expect_err("/status should remain REPL-only when invoked directly");
        assert!(error.contains("interactive-only"));
        assert!(error.contains("Himalaya --resume SESSION.jsonl /status"));
    }

    #[test]
    fn direct_slash_commands_surface_shared_validation_errors() {
        let compact_error = parse_args(&["/compact".to_string(), "now".to_string()])
            .expect_err("invalid /compact shape should be rejected");
        assert!(compact_error.contains("Unexpected arguments for /compact."));
        assert!(compact_error.contains("Usage            /compact"));

        let plugins_error = parse_args(&[
            "/plugins".to_string(),
            "list".to_string(),
            "extra".to_string(),
        ])
        .expect_err("invalid /plugins list shape should be rejected");
        assert!(plugins_error.contains("Usage: /plugin list"));
        assert!(plugins_error.contains("Aliases          /plugins, /marketplace"));
    }

    #[test]
    fn formats_unknown_slash_command_with_suggestions() {
        let report = format_unknown_slash_command_message("statsu");
        assert!(report.contains("unknown slash command: /statsu"));
        assert!(report.contains("Did you mean"));
        assert!(report.contains("Use /help"));
    }

    #[test]
    fn runtime_event_line_renders_plan_execution() {
        let event = serde_json::json!({
            "seq": 1, "task_id": "t1", "node_id": "node-7",
            "kind": "node_started", "status": "running"
        });
        let line = format_runtime_event_line("plan_execution_event", &event).expect("line");
        assert!(line.contains("plan"));
        assert!(line.contains("node started"));
        assert!(line.contains("node-7"));
        assert!(line.contains("running"));
        // running → blue tone
        assert!(line.contains("38;5;75"));
    }

    #[test]
    fn runtime_event_line_renders_model_route_with_confidence() {
        let event = serde_json::json!({
            "phase": "coding", "model": "sonnet", "reason": "r", "confidence": 0.83
        });
        let line = format_runtime_event_line("model_route_event", &event).expect("line");
        assert!(line.contains("route"));
        assert!(line.contains("coding"));
        assert!(line.contains("sonnet"));
        assert!(line.contains("83%"));
    }

    #[test]
    fn runtime_event_line_recovery_outcome_tone() {
        let recovered = serde_json::json!({
            "recovery_attempted": { "scenario": "compile_failure", "result": { "recovered": { "steps_taken": 1 } } }
        });
        let line = format_runtime_event_line("recovery_event", &recovered).expect("line");
        assert!(line.contains("recovered"));
        assert!(line.contains("compile_failure"));
        assert!(line.contains("38;5;43")); // ok tone

        let escalated = serde_json::json!({
            "recovery_attempted": { "scenario": "x", "result": { "escalation_required": true } }
        });
        let line = format_runtime_event_line("recovery_event", &escalated).expect("line");
        assert!(line.contains("escalation required"));
        assert!(line.contains("38;5;203")); // err tone
    }

    #[test]
    fn runtime_event_line_skips_unknown_and_noisy_kinds() {
        assert!(format_runtime_event_line("decisioning_event", &serde_json::json!({})).is_none());
        assert!(format_runtime_event_line("some_future_kind", &serde_json::json!({})).is_none());
    }

    #[test]
    fn runtime_status_tone_classifies_states() {
        assert_eq!(super::runtime_status_tone("node_failed"), "err");
        assert_eq!(super::runtime_status_tone("completed"), "ok");
        assert_eq!(super::runtime_status_tone("running"), "run");
        assert_eq!(super::runtime_status_tone("partial_recovery"), "warn");
        assert_eq!(super::runtime_status_tone("mystery"), "idle");
    }

    #[test]
    fn formats_namespaced_omc_slash_command_with_contract_guidance() {
        let report = format_unknown_slash_command_message("oh-my-Himalayacode:hud");
        assert!(report.contains("unknown slash command: /oh-my-Himalayacode:hud"));
        assert!(report.contains("Himalaya Code/OMC plugin command"));
        assert!(report.contains("plugin slash commands"));
        assert!(report.contains("statusline"));
        assert!(report.contains("session hooks"));
    }

    #[test]
    fn parses_resume_flag_with_slash_command() {
        let args = vec![
            "--resume".to_string(),
            "session.jsonl".to_string(),
            "/compact".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::ResumeSession {
                session_path: PathBuf::from("session.jsonl"),
                commands: vec!["/compact".to_string()],
                output_format: CliOutputFormat::Text,
            }
        );
    }

    #[test]
    fn parses_resume_flag_without_path_as_latest_session() {
        assert_eq!(
            parse_args(&["--resume".to_string()]).expect("args should parse"),
            CliAction::ResumeSession {
                session_path: PathBuf::from("latest"),
                commands: vec![],
                output_format: CliOutputFormat::Text,
            }
        );
        assert_eq!(
            parse_args(&["--resume".to_string(), "/status".to_string()])
                .expect("resume shortcut should parse"),
            CliAction::ResumeSession {
                session_path: PathBuf::from("latest"),
                commands: vec!["/status".to_string()],
                output_format: CliOutputFormat::Text,
            }
        );
    }

    #[test]
    fn parses_resume_flag_with_prompt_subcommand() {
        let args = vec![
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--resume".to_string(),
            "session.jsonl".to_string(),
            "prompt".to_string(),
            "hello".to_string(),
            "again".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("resumed prompt should parse"),
            CliAction::ResumePrompt {
                session_path: PathBuf::from("session.jsonl"),
                prompt: "hello again".to_string(),
                model: DEFAULT_MODEL.to_string(),
                output_format: CliOutputFormat::StreamJson,
                allowed_tools: None,
                permission_mode: crate::default_permission_mode(),
                compact: false,
                base_commit: None,
                reasoning_effort: None,
                allow_broad_cwd: false,
                file_paths: Vec::new(),
            }
        );
    }
    #[test]
    fn parses_resume_flag_with_multiple_slash_commands() {
        let args = vec![
            "--resume".to_string(),
            "session.jsonl".to_string(),
            "/status".to_string(),
            "/compact".to_string(),
            "/cost".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::ResumeSession {
                session_path: PathBuf::from("session.jsonl"),
                commands: vec![
                    "/status".to_string(),
                    "/compact".to_string(),
                    "/cost".to_string(),
                ],
                output_format: CliOutputFormat::Text,
            }
        );
    }

    #[test]
    fn rejects_unknown_options_with_helpful_guidance() {
        let error = parse_args(&["--resum".to_string()]).expect_err("unknown option should fail");
        assert!(error.contains("unknown option: --resum"));
        assert!(error.contains("Did you mean --resume?"));
        assert!(error.contains("Himalaya --help"));
    }

    #[test]
    fn parses_resume_flag_with_slash_command_arguments() {
        let args = vec![
            "--resume".to_string(),
            "session.jsonl".to_string(),
            "/export".to_string(),
            "notes.txt".to_string(),
            "/clear".to_string(),
            "--confirm".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::ResumeSession {
                session_path: PathBuf::from("session.jsonl"),
                commands: vec![
                    "/export notes.txt".to_string(),
                    "/clear --confirm".to_string(),
                ],
                output_format: CliOutputFormat::Text,
            }
        );
    }

    #[test]
    fn parses_resume_flag_with_absolute_export_path() {
        let args = vec![
            "--resume".to_string(),
            "session.jsonl".to_string(),
            "/export".to_string(),
            "/tmp/notes.txt".to_string(),
            "/status".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::ResumeSession {
                session_path: PathBuf::from("session.jsonl"),
                commands: vec!["/export /tmp/notes.txt".to_string(), "/status".to_string()],
                output_format: CliOutputFormat::Text,
            }
        );
    }

    #[test]
    fn filtered_tool_specs_respect_allowlist() {
        let allowed = ["read_file", "grep_search"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let filtered = filter_tool_specs(&GlobalToolRegistry::builtin(), Some(&allowed));
        let names = filtered
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["read_file", "grep_search"]);
    }

    #[test]
    fn filtered_tool_specs_include_plugin_tools() {
        let filtered = filter_tool_specs(&registry_with_plugin_tool(), None);
        let names = filtered
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();
        assert!(names.contains(&"bash".to_string()));
        assert!(names.contains(&"plugin_echo".to_string()));
    }

    #[test]
    fn permission_policy_uses_plugin_tool_permissions() {
        let feature_config = runtime::RuntimeFeatureConfig::default();
        let policy = permission_policy(
            PermissionMode::ReadOnly,
            &feature_config,
            &registry_with_plugin_tool(),
        )
        .expect("permission policy should build");
        let required = policy.required_mode_for("plugin_echo");
        assert_eq!(required, PermissionMode::WorkspaceWrite);
    }

    #[test]
    fn permission_policy_uses_mcp_runtime_tool_permissions() {
        let feature_config = runtime::RuntimeFeatureConfig::default();
        let registry = GlobalToolRegistry::builtin()
            .with_runtime_tools(vec![
                tools::RuntimeToolDefinition {
                    name: "mcp__alpha__echo".to_string(),
                    description: Some("Read-only MCP echo".to_string()),
                    input_schema: json!({ "type": "object", "additionalProperties": true }),
                    required_permission: PermissionMode::ReadOnly,
                },
                tools::RuntimeToolDefinition {
                    name: "mcp__alpha__write".to_string(),
                    description: Some("Workspace-write MCP tool".to_string()),
                    input_schema: json!({ "type": "object", "additionalProperties": true }),
                    required_permission: PermissionMode::WorkspaceWrite,
                },
                tools::RuntimeToolDefinition {
                    name: "MCPTool".to_string(),
                    description: Some("Generic MCP wrapper".to_string()),
                    input_schema: json!({ "type": "object", "additionalProperties": true }),
                    required_permission: PermissionMode::DangerFullAccess,
                },
            ])
            .expect("runtime MCP tools should register");
        let policy = permission_policy(PermissionMode::ReadOnly, &feature_config, &registry)
            .expect("permission policy should build");

        assert_eq!(
            policy.authorize("mcp__alpha__echo", r#"{"text":"hi"}"#, None),
            PermissionOutcome::Allow
        );
        assert!(matches!(
            policy.authorize("mcp__alpha__write", r#"{"path":"out.txt"}"#, None),
            PermissionOutcome::Deny { reason } if reason.contains("workspace-write") && reason.contains("read-only")
        ));
        assert!(matches!(
            policy.authorize("MCPTool", r#"{"qualifiedName":"mcp__alpha__write"}"#, None),
            PermissionOutcome::Deny { reason } if reason.contains("danger-full-access") && reason.contains("read-only")
        ));
    }

    #[test]
    fn shared_help_uses_resume_annotation_copy() {
        let help = commands::render_slash_command_help();
        assert!(help.contains("Slash commands"));
        assert!(help.contains("works with --resume SESSION.jsonl"));
    }

    #[test]
    fn repl_help_includes_shared_commands_and_exit() {
        let help = render_repl_help();
        assert!(help.contains("REPL"));
        assert!(help.contains("/help"));
        assert!(help.contains("Complete commands, modes, and recent sessions"));
        assert!(help.contains("/status"));
        assert!(help.contains("/sandbox"));
        assert!(help.contains("/model [model]"));
        assert!(help.contains("/permissions [default|plan|acceptEdits|auto|bypassPermissions]"));
        assert!(help.contains("/clear [--confirm]"));
        assert!(help.contains("/cost"));
        assert!(help.contains("/resume <session-path>"));
        assert!(help.contains("/config [env|hooks|model|plugins]"));
        assert!(help.contains("/mcp [list|show <server>|help]"));
        assert!(help.contains("/memory"));
        assert!(help.contains("/init"));
        assert!(help.contains("/diff"));
        assert!(help.contains("/version"));
        assert!(help.contains("/export [file]"));
        // Batch 5 added `/session delete`; match on the stable core rather than
        // the trailing bracket so future additions don't re-break this.
        assert!(help.contains("/session [list|switch <session-id>|fork [branch-name]"));
        assert!(help.contains(
            "/plugin [list|install <path>|enable <name>|disable <name>|uninstall <id>|update <id>]"
        ));
        assert!(help.contains("aliases: /plugins, /marketplace"));
        assert!(help.contains("/agents"));
        assert!(help.contains("/skills"));
        assert!(help.contains("/exit"));
        assert!(help.contains("Auto-save            .Himalaya/sessions/<session-id>.jsonl"));
        assert!(help.contains("Resume latest        /resume latest"));
    }

    #[test]
    fn completion_candidates_include_workflow_shortcuts_and_dynamic_sessions() {
        let completions = slash_command_completion_candidates_with_sessions(
            "sonnet",
            Some("session-current"),
            vec!["session-old".to_string()],
        );

        assert!(completions.contains(&"/model Himalaya-sonnet-4-6".to_string()));
        assert!(completions.contains(&"/permissions workspace-write".to_string()));
        assert!(completions.contains(&"/session list".to_string()));
        assert!(completions.contains(&"/session switch session-current".to_string()));
        assert!(completions.contains(&"/resume session-old".to_string()));
        assert!(completions.contains(&"/mcp list".to_string()));
        assert!(completions.contains(&"/ultraplan ".to_string()));
    }

    #[test]
    fn startup_banner_mentions_workflow_completions() {
        let _guard = env_lock();
        // Inject dummy credentials so LiveCli can construct without real Anthropic key
        std::env::set_var("ANTHROPIC_API_KEY", "test-dummy-key-for-banner-test");
        let root = temp_dir();
        fs::create_dir_all(&root).expect("root dir");

        let banner = with_current_dir(&root, || {
            LiveCli::new(
                "Himalaya-sonnet-4-6".to_string(),
                true,
                None,
                default_permission_mode_for_tests(),
            )
            .expect("cli should initialize")
            .startup_banner()
        });

        assert!(banner.contains("Tab"));
        assert!(banner.contains("workflow completions"));

        fs::remove_dir_all(root).expect("cleanup temp dir");
        std::env::remove_var("ANTHROPIC_API_KEY");
    }

    #[test]
    fn format_connected_line_renders_anthropic_provider_for_Himalaya_model() {
        let model = "Himalaya-sonnet-4-6";

        let line = format_connected_line(model);

        assert_eq!(line, "Connected: Himalaya-sonnet-4-6 via anthropic");
    }

    #[test]
    fn format_connected_line_renders_xai_provider_for_grok_model() {
        let model = "grok-3";

        let line = format_connected_line(model);

        assert_eq!(line, "Connected: grok-3 via xai");
    }

    #[test]
    fn resolve_repl_model_returns_user_supplied_model_unchanged_when_explicit() {
        let user_model = "Himalaya-sonnet-4-6".to_string();

        let resolved = resolve_repl_model(user_model);

        assert_eq!(resolved, "Himalaya-sonnet-4-6");
    }

    #[test]
    fn resolve_repl_model_falls_back_to_anthropic_model_env_when_default() {
        let _guard = env_lock();
        let root = temp_dir();
        fs::create_dir_all(&root).expect("root dir");
        let config_home = root.join("config");
        fs::create_dir_all(&config_home).expect("config home dir");
        std::env::set_var("Himalaya_CONFIG_HOME", &config_home);
        std::env::remove_var("ANTHROPIC_MODEL");
        std::env::set_var("ANTHROPIC_MODEL", "sonnet");

        let resolved = with_current_dir(&root, || resolve_repl_model(DEFAULT_MODEL.to_string()));

        assert_eq!(resolved, "Himalaya-sonnet-4-6");

        std::env::remove_var("ANTHROPIC_MODEL");
        std::env::remove_var("Himalaya_CONFIG_HOME");
        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn resolve_repl_model_returns_default_when_env_unset_and_no_config() {
        let _guard = env_lock();
        let root = temp_dir();
        fs::create_dir_all(&root).expect("root dir");
        let config_home = root.join("config");
        fs::create_dir_all(&config_home).expect("config home dir");
        std::env::set_var("Himalaya_CONFIG_HOME", &config_home);
        std::env::remove_var("ANTHROPIC_MODEL");

        let resolved = with_current_dir(&root, || resolve_repl_model(DEFAULT_MODEL.to_string()));

        assert_eq!(resolved, DEFAULT_MODEL);

        std::env::remove_var("Himalaya_CONFIG_HOME");
        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn resume_supported_command_list_matches_expected_surface() {
        let names = resume_supported_slash_commands()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        // After Phase 3 cleanup: stubs are excluded from resume-safe by the
        // commands crate. Verify the minimum still holds.
        assert!(
            names.len() >= 27,
            "expected at least 27 resume-supported commands, got {}",
            names.len()
        );
        // Verify key resume commands still exist
        assert!(names.contains(&"help"));
        assert!(names.contains(&"status"));
        assert!(names.contains(&"compact"));
    }

    #[test]
    fn resume_report_uses_sectioned_layout() {
        let report = format_resume_report("session.jsonl", 14, 6);
        assert!(report.contains("Session resumed"));
        assert!(report.contains("Session file     session.jsonl"));
        assert!(report.contains("Messages         14"));
        assert!(report.contains("Turns            6"));
    }

    #[test]
    fn compact_report_uses_structured_output() {
        let compacted = format_compact_report(8, 5, false);
        assert!(compacted.contains("Compact"));
        assert!(compacted.contains("Result           compacted"));
        assert!(compacted.contains("Messages removed 8"));
        let skipped = format_compact_report(0, 3, true);
        assert!(skipped.contains("Result           skipped"));
    }

    #[test]
    fn cost_report_uses_sectioned_layout() {
        let report = format_cost_report(runtime::TokenUsage {
            input_tokens: 20,
            output_tokens: 8,
            cache_creation_input_tokens: 3,
            cache_read_input_tokens: 1,
        });
        assert!(report.contains("Cost"));
        assert!(report.contains("Input tokens     20"));
        assert!(report.contains("Output tokens    8"));
        assert!(report.contains("Cache create     3"));
        assert!(report.contains("Cache read       1"));
        assert!(report.contains("Total tokens     32"));
    }

    #[test]
    fn permissions_report_uses_sectioned_layout() {
        let report = format_permissions_report("workspace-write");
        assert!(report.contains("Permissions"));
        assert!(report.contains("Active mode      workspace-write"));
        assert!(report.contains("Modes"));
        assert!(report.contains("read-only"));
        assert!(report.contains("workspace-write"));
        assert!(report.contains("danger-full-access"));
        assert!(report.contains("● current"));
        assert!(report.contains("○ available"));
    }

    #[test]
    fn permissions_switch_report_is_structured() {
        let report = format_permissions_switch_report("read-only", "workspace-write");
        assert!(report.contains("Permissions updated"));
        assert!(report.contains("Result           mode switched"));
        assert!(report.contains("Previous mode    read-only"));
        assert!(report.contains("Active mode      workspace-write"));
        assert!(report.contains("Applies to       subsequent tool calls"));
    }

    #[test]
    fn init_help_mentions_direct_subcommand() {
        let mut help = Vec::new();
        print_help_to(&mut help).expect("help should render");
        let help = String::from_utf8(help).expect("help should be utf8");
        assert!(help.contains("Himalaya help"));
        assert!(help.contains("[--output-format text|json|stream-json] prompt TEXT"));
        assert!(help.contains("[--output-format text|json|stream-json] TEXT"));
        assert!(help.contains("Non-interactive output format: text, json, or stream-json"));
        assert!(help.contains("Himalaya version"));
        assert!(help.contains("Himalaya status"));
        assert!(help.contains("Himalaya sandbox"));
        assert!(help.contains("Himalaya init"));
        assert!(help.contains("Himalaya agents"));
        assert!(help.contains("Himalaya mcp"));
        assert!(help.contains("Himalaya skills"));
        assert!(help.contains("Himalaya /skills"));
    }

    #[test]
    fn model_report_uses_sectioned_layout() {
        let report = format_model_report("Himalaya-sonnet", 12, 4);
        assert!(report.contains("Model"));
        assert!(report.contains("Current model    Himalaya-sonnet"));
        assert!(report.contains("Session messages 12"));
        assert!(report.contains("Switch models with /model <name>"));
        assert!(report.contains("Built-in aliases"));
        assert!(report.contains("Add custom aliases in settings.json"));
    }

    #[test]
    fn model_switch_report_preserves_context_summary() {
        let report = format_model_switch_report("Himalaya-sonnet", "Himalaya-opus", 9);
        assert!(report.contains("Model updated"));
        assert!(report.contains("Previous         Himalaya-sonnet"));
        assert!(report.contains("Current          Himalaya-opus"));
        assert!(report.contains("Preserved msgs   9"));
    }

    #[test]
    fn status_line_reports_model_and_token_totals() {
        let status = format_status_report(
            "Himalaya-sonnet",
            StatusUsage {
                message_count: 7,
                turns: 3,
                latest: runtime::TokenUsage {
                    input_tokens: 5,
                    output_tokens: 4,
                    cache_creation_input_tokens: 1,
                    cache_read_input_tokens: 0,
                },
                cumulative: runtime::TokenUsage {
                    input_tokens: 20,
                    output_tokens: 8,
                    cache_creation_input_tokens: 2,
                    cache_read_input_tokens: 1,
                },
                estimated_tokens: 128,
            },
            "workspace-write",
            &super::StatusContext {
                cwd: PathBuf::from("/tmp/project"),
                session_path: Some(PathBuf::from("session.jsonl")),
                loaded_config_files: 2,
                discovered_config_files: 3,
                memory_file_count: 4,
                project_root: Some(PathBuf::from("/tmp")),
                git_branch: Some("main".to_string()),
                git_summary: GitWorkspaceSummary {
                    changed_files: 3,
                    staged_files: 1,
                    unstaged_files: 1,
                    untracked_files: 1,
                    conflicted_files: 0,
                },
                sandbox_status: runtime::SandboxStatus::default(),
            },
        );
        assert!(status.contains("Status"));
        assert!(status.contains("Model            Himalaya-sonnet"));
        assert!(status.contains("Permission mode  workspace-write"));
        assert!(status.contains("Messages         7"));
        assert!(status.contains("Latest total     10"));
        assert!(status.contains("Cumulative total 31"));
        assert!(status.contains("Cwd              /tmp/project"));
        assert!(status.contains("Project root     /tmp"));
        assert!(status.contains("Git branch       main"));
        assert!(
            status.contains("Git state        dirty · 3 files · 1 staged, 1 unstaged, 1 untracked")
        );
        assert!(status.contains("Changed files    3"));
        assert!(status.contains("Staged           1"));
        assert!(status.contains("Unstaged         1"));
        assert!(status.contains("Untracked        1"));
        assert!(status.contains("Session          session.jsonl"));
        assert!(status.contains("Config files     loaded 2/3"));
        assert!(status.contains("Memory files     4"));
        assert!(status.contains("Suggested flow   /status → /diff → /commit"));
    }

    #[test]
    fn commit_reports_surface_workspace_context() {
        let summary = GitWorkspaceSummary {
            changed_files: 2,
            staged_files: 1,
            unstaged_files: 1,
            untracked_files: 0,
            conflicted_files: 0,
        };

        let preflight = format_commit_preflight_report(Some("feature/ux"), summary);
        assert!(preflight.contains("Result           ready"));
        assert!(preflight.contains("Branch           feature/ux"));
        assert!(preflight.contains("Workspace        dirty · 2 files · 1 staged, 1 unstaged"));
        assert!(preflight
            .contains("Action           create a git commit from the current workspace changes"));
    }

    #[test]
    fn commit_skipped_report_points_to_next_steps() {
        let report = format_commit_skipped_report();
        assert!(report.contains("Reason           no workspace changes"));
        assert!(report
            .contains("Action           create a git commit from the current workspace changes"));
        assert!(report.contains("/status to inspect context"));
        assert!(report.contains("/diff to inspect repo changes"));
    }

    #[test]
    fn runtime_slash_reports_describe_command_behavior() {
        let bughunter = format_bughunter_report(Some("runtime"));
        assert!(bughunter.contains("Scope            runtime"));
        assert!(bughunter.contains("inspect the selected code for likely bugs"));

        let ultraplan = format_ultraplan_report(Some("ship the release"));
        assert!(ultraplan.contains("Task             ship the release"));
        assert!(ultraplan.contains("break work into a multi-step execution plan"));

        let pr = format_pr_report("feature/ux", Some("ready for review"));
        assert!(pr.contains("Branch           feature/ux"));
        assert!(pr.contains("draft or create a pull request"));

        let issue = format_issue_report(Some("flaky test"));
        assert!(issue.contains("Context          flaky test"));
        assert!(issue.contains("draft or create a GitHub issue"));
    }

    #[test]
    fn no_arg_commands_reject_unexpected_arguments() {
        assert!(validate_no_args("/commit", None).is_ok());

        let error = validate_no_args("/commit", Some("now"))
            .expect_err("unexpected arguments should fail")
            .to_string();
        assert!(error.contains("/commit does not accept arguments"));
        assert!(error.contains("Received: now"));
    }

    #[test]
    fn config_report_supports_section_views() {
        let report = render_config_report(Some("env")).expect("config report should render");
        assert!(report.contains("Merged section: env"));
        let plugins_report =
            render_config_report(Some("plugins")).expect("plugins config report should render");
        assert!(plugins_report.contains("Merged section: plugins"));
    }

    #[test]
    fn memory_report_uses_sectioned_layout() {
        let report = render_memory_report().expect("memory report should render");
        assert!(report.contains("Memory"));
        assert!(report.contains("Working directory"));
        assert!(report.contains("Instruction files"));
        assert!(report.contains("Discovered files"));
    }

    #[test]
    fn config_report_uses_sectioned_layout() {
        let report = render_config_report(None).expect("config report should render");
        assert!(report.contains("Config"));
        assert!(report.contains("Discovered files"));
        assert!(report.contains("Merged JSON"));
    }

    #[test]
    fn parses_git_status_metadata() {
        let _guard = env_lock();
        let temp_root = temp_dir();
        fs::create_dir_all(&temp_root).expect("root dir");
        let (project_root, branch) = parse_git_status_metadata_for(
            &temp_root,
            Some(
                "## rcc/cli...origin/rcc/cli
 M src/main.rs",
            ),
        );
        assert_eq!(branch.as_deref(), Some("rcc/cli"));
        assert!(project_root.is_none());
        fs::remove_dir_all(temp_root).expect("cleanup temp dir");
    }

    #[test]
    fn parses_detached_head_from_status_snapshot() {
        let _guard = env_lock();
        assert_eq!(
            parse_git_status_branch(Some(
                "## HEAD (no branch)
 M src/main.rs"
            )),
            Some("detached HEAD".to_string())
        );
    }

    #[test]
    fn parses_git_workspace_summary_counts() {
        let summary = parse_git_workspace_summary(Some(
            "## feature/ux
M  src/main.rs
 M README.md
?? notes.md
UU conflicted.rs",
        ));

        assert_eq!(
            summary,
            GitWorkspaceSummary {
                changed_files: 4,
                staged_files: 2,
                unstaged_files: 2,
                untracked_files: 1,
                conflicted_files: 1,
            }
        );
        assert_eq!(
            summary.headline(),
            "dirty · 4 files · 2 staged, 2 unstaged, 1 untracked, 1 conflicted"
        );
    }

    #[test]
    fn render_diff_report_shows_clean_tree_for_committed_repo() {
        let _guard = env_lock();
        let root = temp_dir();
        fs::create_dir_all(&root).expect("root dir");
        git(&["init", "--quiet"], &root);
        git(&["config", "user.email", "tests@example.com"], &root);
        git(&["config", "user.name", "Rusty Himalaya Tests"], &root);
        fs::write(root.join("tracked.txt"), "hello\n").expect("write file");
        git(&["add", "tracked.txt"], &root);
        git(&["commit", "-m", "init", "--quiet"], &root);

        let report = render_diff_report_for(&root).expect("diff report should render");
        assert!(report.contains("clean working tree"));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn render_diff_report_includes_staged_and_unstaged_sections() {
        let _guard = env_lock();
        let root = temp_dir();
        fs::create_dir_all(&root).expect("root dir");
        git(&["init", "--quiet"], &root);
        git(&["config", "user.email", "tests@example.com"], &root);
        git(&["config", "user.name", "Rusty Himalaya Tests"], &root);
        fs::write(root.join("tracked.txt"), "hello\n").expect("write file");
        git(&["add", "tracked.txt"], &root);
        git(&["commit", "-m", "init", "--quiet"], &root);

        fs::write(root.join("tracked.txt"), "hello\nstaged\n").expect("update file");
        git(&["add", "tracked.txt"], &root);
        fs::write(root.join("tracked.txt"), "hello\nstaged\nunstaged\n")
            .expect("update file twice");

        let report = render_diff_report_for(&root).expect("diff report should render");
        assert!(report.contains("Staged changes:"));
        assert!(report.contains("Unstaged changes:"));
        assert!(report.contains("tracked.txt"));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn render_diff_report_omits_ignored_files() {
        let _guard = env_lock();
        let root = temp_dir();
        fs::create_dir_all(&root).expect("root dir");
        git(&["init", "--quiet"], &root);
        git(&["config", "user.email", "tests@example.com"], &root);
        git(&["config", "user.name", "Rusty Himalaya Tests"], &root);
        fs::write(root.join(".gitignore"), ".omx/\nignored.txt\n").expect("write gitignore");
        fs::write(root.join("tracked.txt"), "hello\n").expect("write tracked");
        git(&["add", ".gitignore", "tracked.txt"], &root);
        git(&["commit", "-m", "init", "--quiet"], &root);
        fs::create_dir_all(root.join(".omx")).expect("write omx dir");
        fs::write(root.join(".omx").join("state.json"), "{}").expect("write ignored omx");
        fs::write(root.join("ignored.txt"), "secret\n").expect("write ignored file");
        fs::write(root.join("tracked.txt"), "hello\nworld\n").expect("write tracked change");

        let report = render_diff_report_for(&root).expect("diff report should render");
        assert!(report.contains("tracked.txt"));
        assert!(!report.contains("+++ b/ignored.txt"));
        assert!(!report.contains("+++ b/.omx/state.json"));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn resume_diff_command_renders_report_for_saved_session() {
        let _guard = env_lock();
        let root = temp_dir();
        fs::create_dir_all(&root).expect("root dir");
        git(&["init", "--quiet"], &root);
        git(&["config", "user.email", "tests@example.com"], &root);
        git(&["config", "user.name", "Rusty Himalaya Tests"], &root);
        fs::write(root.join("tracked.txt"), "hello\n").expect("write tracked");
        git(&["add", "tracked.txt"], &root);
        git(&["commit", "-m", "init", "--quiet"], &root);
        fs::write(root.join("tracked.txt"), "hello\nworld\n").expect("modify tracked");
        let session_path = root.join("session.json");
        Session::new()
            .save_to_path(&session_path)
            .expect("session should save");

        let session = Session::load_from_path(&session_path).expect("session should load");
        let outcome = with_current_dir(&root, || {
            run_resume_command(&session_path, &session, &SlashCommand::Diff)
                .expect("resume diff should work")
        });
        let message = outcome.message.expect("diff message should exist");
        assert!(message.contains("Unstaged changes:"));
        assert!(message.contains("tracked.txt"));

        fs::remove_dir_all(root).expect("cleanup temp dir");
    }

    #[test]
    fn status_context_reads_real_workspace_metadata() {
        let context = status_context(None).expect("status context should load");
        assert!(context.cwd.is_absolute());
        assert!(context.discovered_config_files >= context.loaded_config_files);
        assert!(context.loaded_config_files <= context.discovered_config_files);
    }

    #[test]
    fn normalizes_supported_permission_modes() {
        assert_eq!(normalize_permission_mode("read-only"), Some("read-only"));
        assert_eq!(
            normalize_permission_mode("workspace-write"),
            Some("workspace-write")
        );
        assert_eq!(
            normalize_permission_mode("danger-full-access"),
            Some("danger-full-access")
        );
        assert_eq!(normalize_permission_mode("unknown"), None);
    }

    #[test]
    fn clear_command_requires_explicit_confirmation_flag() {
        assert_eq!(
            SlashCommand::parse("/clear"),
            Ok(Some(SlashCommand::Clear { confirm: false }))
        );
        assert_eq!(
            SlashCommand::parse("/clear --confirm"),
            Ok(Some(SlashCommand::Clear { confirm: true }))
        );
    }

    #[test]
    fn parses_resume_and_config_slash_commands() {
        assert_eq!(
            SlashCommand::parse("/resume saved-session.jsonl"),
            Ok(Some(SlashCommand::Resume {
                session_path: Some("saved-session.jsonl".to_string())
            }))
        );
        assert_eq!(
            SlashCommand::parse("/clear --confirm"),
            Ok(Some(SlashCommand::Clear { confirm: true }))
        );
        assert_eq!(
            SlashCommand::parse("/config"),
            Ok(Some(SlashCommand::Config { section: None }))
        );
        assert_eq!(
            SlashCommand::parse("/config env"),
            Ok(Some(SlashCommand::Config {
                section: Some("env".to_string())
            }))
        );
        assert_eq!(
            SlashCommand::parse("/memory"),
            Ok(Some(SlashCommand::Memory))
        );
        assert_eq!(SlashCommand::parse("/init"), Ok(Some(SlashCommand::Init)));
        assert_eq!(
            SlashCommand::parse("/session fork incident-review"),
            Ok(Some(SlashCommand::Session {
                action: Some("fork".to_string()),
                target: Some("incident-review".to_string())
            }))
        );
    }

    #[test]
    fn help_mentions_jsonl_resume_examples() {
        let mut help = Vec::new();
        print_help_to(&mut help).expect("help should render");
        let help = String::from_utf8(help).expect("help should be utf8");
        assert!(help.contains("Himalaya --resume [SESSION.jsonl|session-id|latest]"));
        assert!(help.contains("Use `latest` with --resume, /resume, or /session switch"));
        assert!(help.contains("Himalaya --resume latest"));
        assert!(help.contains("Himalaya --resume latest /status /diff /export notes.txt"));
        assert!(help.contains("Himalaya tasks daemon status"));
        assert!(help.contains("Himalaya tasks daemon report --limit 20 --max-ticks 3"));
        assert!(help.contains("Himalaya tasks daemon evaluate --limit 20"));
        assert!(help.contains("Himalaya tasks daemon replay --limit 20"));
        assert!(help.contains("Himalaya policy apply --dry-run --domain routing"));
    }

    #[test]
    fn managed_sessions_default_to_jsonl_and_resolve_legacy_json() {
        let _guard = cwd_lock().lock().expect("cwd lock");
        let workspace = temp_workspace("session-resolution");
        std::fs::create_dir_all(&workspace).expect("workspace should create");
        let previous = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&workspace).expect("switch cwd");

        let handle = create_managed_session_handle("session-alpha").expect("jsonl handle");
        assert!(handle.path.ends_with("session-alpha.jsonl"));

        let legacy_path = workspace.join(".Himalaya/sessions/legacy.json");
        std::fs::create_dir_all(
            legacy_path
                .parent()
                .expect("legacy path should have parent directory"),
        )
        .expect("session dir should exist");
        Session::new()
            .with_persistence_path(legacy_path.clone())
            .save_to_path(&legacy_path)
            .expect("legacy session should save");

        let resolved = resolve_session_reference("legacy").expect("legacy session should resolve");
        assert_eq!(
            resolved
                .path
                .canonicalize()
                .expect("resolved path should exist"),
            legacy_path
                .canonicalize()
                .expect("legacy path should exist")
        );

        std::env::set_current_dir(previous).expect("restore cwd");
        std::fs::remove_dir_all(workspace).expect("workspace should clean up");
    }

    #[test]
    fn managed_sessions_resolve_legacy_fingerprint_namespace() {
        let _guard = cwd_lock().lock().expect("cwd lock");
        let workspace = temp_workspace("session-namespace-resolution");
        std::fs::create_dir_all(&workspace).expect("workspace should create");
        let previous = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&workspace).expect("switch cwd");

        let legacy_namespace = workspace
            .join(".Himalaya")
            .join("sessions")
            .join("legacy-workspace-hash");
        std::fs::create_dir_all(&legacy_namespace).expect("legacy namespace should exist");
        let legacy_path = legacy_namespace.join("session-before-rename.jsonl");
        Session::new()
            .with_persistence_path(legacy_path.clone())
            .save_to_path(&legacy_path)
            .expect("legacy namespaced session should save");

        let resolved = resolve_session_reference("session-before-rename")
            .expect("legacy namespaced session should resolve");
        assert_eq!(
            resolved
                .path
                .canonicalize()
                .expect("resolved path should exist"),
            legacy_path
                .canonicalize()
                .expect("legacy path should exist")
        );

        std::env::set_current_dir(previous).expect("restore cwd");
        std::fs::remove_dir_all(workspace).expect("workspace should clean up");
    }

    #[test]
    fn latest_session_alias_resolves_most_recent_managed_session() {
        let _guard = cwd_lock().lock().expect("cwd lock");
        let workspace = temp_workspace("latest-session-alias");
        std::fs::create_dir_all(&workspace).expect("workspace should create");
        let previous = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(&workspace).expect("switch cwd");

        let older = create_managed_session_handle("session-older").expect("older handle");
        Session::new()
            .with_persistence_path(older.path.clone())
            .save_to_path(&older.path)
            .expect("older session should save");
        std::thread::sleep(Duration::from_millis(20));
        let newer = create_managed_session_handle("session-newer").expect("newer handle");
        Session::new()
            .with_persistence_path(newer.path.clone())
            .save_to_path(&newer.path)
            .expect("newer session should save");

        let resolved = resolve_session_reference("latest").expect("latest session should resolve");
        assert_eq!(
            resolved
                .path
                .canonicalize()
                .expect("resolved path should exist"),
            newer.path.canonicalize().expect("newer path should exist")
        );

        std::env::set_current_dir(previous).expect("restore cwd");
        std::fs::remove_dir_all(workspace).expect("workspace should clean up");
    }

    #[test]
    fn unknown_slash_command_guidance_suggests_nearby_commands() {
        let message = format_unknown_slash_command("stats");
        assert!(message.contains("Unknown slash command: /stats"));
        assert!(message.contains("/status"));
        assert!(message.contains("/help"));
    }

    #[test]
    fn unknown_omc_slash_command_guidance_explains_runtime_gap() {
        let message = format_unknown_slash_command("oh-my-Himalayacode:hud");
        assert!(message.contains("Unknown slash command: /oh-my-Himalayacode:hud"));
        assert!(message.contains("Himalaya Code/OMC plugin command"));
        assert!(message.contains("does not yet load plugin slash commands"));
    }

    #[test]
    fn resume_usage_mentions_latest_shortcut() {
        let usage = render_resume_usage();
        assert!(usage.contains("/resume <session-path|session-id|latest>"));
        assert!(usage.contains(".Himalaya/sessions/<session-id>.jsonl"));
        assert!(usage.contains("/session list"));
    }

    fn cwd_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn temp_workspace(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("Himalaya-cli-{label}-{nanos}"))
    }

    #[test]
    fn init_template_mentions_detected_rust_workspace() {
        let _guard = cwd_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let rendered = crate::init::render_init_Himalaya_md(&workspace_root);
        assert!(rendered.contains("# Himalaya.md"));
        assert!(rendered.contains("cargo clippy --workspace --all-targets -- -D warnings"));
    }

    #[test]
    fn converts_tool_roundtrip_messages() {
        let messages = vec![
            ConversationMessage::user_text("hello"),
            ConversationMessage::assistant(vec![ContentBlock::ToolUse {
                id: "tool-1".to_string(),
                name: "bash".to_string(),
                input: "{\"command\":\"pwd\"}".to_string(),
            }]),
            ConversationMessage {
                role: MessageRole::Tool,
                blocks: vec![ContentBlock::ToolResult {
                    tool_use_id: "tool-1".to_string(),
                    tool_name: "bash".to_string(),
                    output: "ok".to_string(),
                    is_error: false,
                }],
                usage: None,
            },
        ];

        let converted = super::convert_messages(&messages);
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[1].role, "assistant");
        assert_eq!(converted[2].role, "user");
    }
    #[test]
    fn repl_help_mentions_history_completion_and_multiline() {
        let help = render_repl_help();
        assert!(help.contains("Up/Down"));
        assert!(help.contains("Tab"));
        assert!(help.contains("Shift+Enter/Ctrl+J"));
        assert!(help.contains("Ctrl-R"));
        assert!(help.contains("Reverse-search prompt history"));
        assert!(help.contains("/history [count]"));
    }

    #[test]
    fn parse_history_count_defaults_to_twenty_when_missing() {
        // given
        let raw: Option<&str> = None;

        // when
        let parsed = parse_history_count(raw);

        // then
        assert_eq!(parsed, Ok(20));
    }

    #[test]
    fn parse_history_count_accepts_positive_integers() {
        // given
        let raw = Some("25");

        // when
        let parsed = parse_history_count(raw);

        // then
        assert_eq!(parsed, Ok(25));
    }

    #[test]
    fn parse_history_count_rejects_zero() {
        // given
        let raw = Some("0");

        // when
        let parsed = parse_history_count(raw);

        // then
        assert!(parsed.is_err());
        assert!(parsed.unwrap_err().contains("greater than 0"));
    }

    #[test]
    fn parse_history_count_rejects_non_numeric() {
        // given
        let raw = Some("abc");

        // when
        let parsed = parse_history_count(raw);

        // then
        assert!(parsed.is_err());
        assert!(parsed.unwrap_err().contains("invalid count 'abc'"));
    }

    #[test]
    fn format_history_timestamp_renders_iso8601_utc() {
        // given
        // 2023-01-15T12:34:56.789Z -> 1673786096789 ms
        let timestamp_ms: u64 = 1_673_786_096_789;

        // when
        let formatted = format_history_timestamp(timestamp_ms);

        // then
        assert_eq!(formatted, "2023-01-15T12:34:56.789Z");
    }

    #[test]
    fn format_history_timestamp_renders_unix_epoch_origin() {
        // given
        let timestamp_ms: u64 = 0;

        // when
        let formatted = format_history_timestamp(timestamp_ms);

        // then
        assert_eq!(formatted, "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn render_prompt_history_report_lists_entries_with_timestamps() {
        // given
        let entries = vec![
            PromptHistoryEntry {
                timestamp_ms: 1_673_786_096_000,
                text: "first prompt".to_string(),
            },
            PromptHistoryEntry {
                timestamp_ms: 1_673_786_100_000,
                text: "second prompt".to_string(),
            },
        ];

        // when
        let rendered = render_prompt_history_report(&entries, 10);

        // then
        assert!(rendered.contains("Prompt history"));
        assert!(rendered.contains("Total            2"));
        assert!(rendered.contains("Showing          2 most recent"));
        assert!(rendered.contains("Reverse search   Ctrl-R in the REPL"));
        assert!(rendered.contains("2023-01-15T12:34:56.000Z"));
        assert!(rendered.contains("first prompt"));
        assert!(rendered.contains("second prompt"));
    }

    #[test]
    fn render_prompt_history_report_truncates_to_limit_from_the_tail() {
        // given
        let entries = vec![
            PromptHistoryEntry {
                timestamp_ms: 1_000,
                text: "older".to_string(),
            },
            PromptHistoryEntry {
                timestamp_ms: 2_000,
                text: "middle".to_string(),
            },
            PromptHistoryEntry {
                timestamp_ms: 3_000,
                text: "latest".to_string(),
            },
        ];

        // when
        let rendered = render_prompt_history_report(&entries, 2);

        // then
        assert!(rendered.contains("Total            3"));
        assert!(rendered.contains("Showing          2 most recent"));
        assert!(!rendered.contains("older"));
        assert!(rendered.contains("middle"));
        assert!(rendered.contains("latest"));
    }

    #[test]
    fn render_prompt_history_report_handles_empty_history() {
        // given
        let entries: Vec<PromptHistoryEntry> = Vec::new();

        // when
        let rendered = render_prompt_history_report(&entries, 10);

        // then
        assert!(rendered.contains("no prompts recorded yet"));
    }

    #[test]
    fn collect_session_prompt_history_extracts_user_text_blocks() {
        // given
        let mut session = Session::new();
        session.push_user_text("hello").unwrap();
        session.push_user_text("world").unwrap();

        // when
        let entries = collect_session_prompt_history(&session);

        // then
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].text, "hello");
        assert_eq!(entries[1].text, "world");
    }

    #[test]
    fn tool_rendering_helpers_compact_output() {
        let start = format_tool_call_start("read_file", r#"{"path":"src/main.rs"}"#);
        assert!(start.contains("read_file"));
        assert!(start.contains("src/main.rs"));

        let done = format_tool_result(
            "read_file",
            r#"{"file":{"filePath":"src/main.rs","content":"hello","numLines":1,"startLine":1,"totalLines":1}}"#,
            false,
        );
        assert!(done.contains("📄 Read src/main.rs"));
        assert!(done.contains("hello"));
    }

    #[test]
    fn tool_rendering_truncates_large_read_output_for_display_only() {
        let content = (0..200)
            .map(|index| format!("line {index:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        let output = json!({
            "file": {
                "filePath": "src/main.rs",
                "content": content,
                "numLines": 200,
                "startLine": 1,
                "totalLines": 200
            }
        })
        .to_string();

        let rendered = format_tool_result("read_file", &output, false);

        assert!(rendered.contains("line 000"));
        assert!(rendered.contains("line 079"));
        assert!(!rendered.contains("line 199"));
        assert!(rendered.contains("full result preserved in session"));
        assert!(output.contains("line 199"));
    }

    #[test]
    fn tool_rendering_truncates_large_bash_output_for_display_only() {
        let stdout = (0..120)
            .map(|index| format!("stdout {index:03}"))
            .collect::<Vec<_>>()
            .join("\n");
        let output = json!({
            "stdout": stdout,
            "stderr": "",
            "returnCodeInterpretation": "completed successfully"
        })
        .to_string();

        let rendered = format_tool_result("bash", &output, false);

        assert!(rendered.contains("stdout 000"));
        assert!(rendered.contains("stdout 059"));
        assert!(!rendered.contains("stdout 119"));
        assert!(rendered.contains("full result preserved in session"));
        assert!(output.contains("stdout 119"));
    }

    #[test]
    fn tool_rendering_truncates_generic_long_output_for_display_only() {
        let items = (0..120)
            .map(|index| format!("payload {index:03}"))
            .collect::<Vec<_>>();
        let output = json!({
            "summary": "plugin payload",
            "items": items,
        })
        .to_string();

        let rendered = format_tool_result("plugin_echo", &output, false);

        assert!(rendered.contains("plugin_echo"));
        assert!(rendered.contains("payload 000"));
        assert!(rendered.contains("payload 040"));
        assert!(!rendered.contains("payload 080"));
        assert!(!rendered.contains("payload 119"));
        assert!(rendered.contains("full result preserved in session"));
        assert!(output.contains("payload 119"));
    }

    #[test]
    fn tool_rendering_truncates_raw_generic_output_for_display_only() {
        let output = (0..120)
            .map(|index| format!("raw {index:03}"))
            .collect::<Vec<_>>()
            .join("\n");

        let rendered = format_tool_result("plugin_echo", &output, false);

        assert!(rendered.contains("plugin_echo"));
        assert!(rendered.contains("raw 000"));
        assert!(rendered.contains("raw 059"));
        assert!(!rendered.contains("raw 119"));
        assert!(rendered.contains("full result preserved in session"));
        assert!(output.contains("raw 119"));
    }

    #[test]
    fn ultraplan_progress_lines_include_phase_step_and_elapsed_status() {
        let snapshot = InternalPromptProgressState {
            command_label: "Ultraplan",
            task_label: "ship plugin progress".to_string(),
            step: 3,
            phase: "running read_file".to_string(),
            detail: Some("reading rust/crates/rusty-Himalaya-cli/src/main.rs".to_string()),
            saw_final_text: false,
        };

        let started = format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Started,
            &snapshot,
            Duration::from_secs(0),
            None,
        );
        let heartbeat = format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Heartbeat,
            &snapshot,
            Duration::from_secs(9),
            None,
        );
        let completed = format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Complete,
            &snapshot,
            Duration::from_secs(12),
            None,
        );
        let failed = format_internal_prompt_progress_line(
            InternalPromptProgressEvent::Failed,
            &snapshot,
            Duration::from_secs(12),
            Some("network timeout"),
        );

        assert!(started.contains("planning started"));
        assert!(started.contains("current step 3"));
        assert!(heartbeat.contains("heartbeat"));
        assert!(heartbeat.contains("9s elapsed"));
        assert!(heartbeat.contains("phase running read_file"));
        assert!(completed.contains("completed"));
        assert!(completed.contains("3 steps total"));
        assert!(failed.contains("failed"));
        assert!(failed.contains("network timeout"));
    }

    #[test]
    fn describe_tool_progress_summarizes_known_tools() {
        assert_eq!(
            describe_tool_progress("read_file", r#"{"path":"src/main.rs"}"#),
            "reading src/main.rs"
        );
        assert!(describe_tool_progress(
            "bash",
            r#"{"command":"cargo test -p rusty-Himalaya-cli"}"#
        )
        .contains("cargo test -p rusty-Himalaya-cli"));
        assert_eq!(
            describe_tool_progress("grep_search", r#"{"pattern":"ultraplan","path":"rust"}"#),
            "grep `ultraplan` in rust"
        );
    }

    #[test]
    fn push_output_block_renders_markdown_text() {
        let mut out = Vec::new();
        let mut events = Vec::new();
        let mut pending_tool = None;
        let mut block_has_thinking_summary = false;

        push_output_block(
            OutputContentBlock::Text {
                text: "# Heading".to_string(),
            },
            &mut out,
            &mut events,
            &mut pending_tool,
            false,
            &mut block_has_thinking_summary,
            false,
        )
        .expect("text block should render");

        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("Heading"));
        assert!(rendered.contains('\u{1b}'));
    }

    #[test]
    fn push_output_block_skips_empty_object_prefix_for_tool_streams() {
        let mut out = Vec::new();
        let mut events = Vec::new();
        let mut pending_tool = None;
        let mut block_has_thinking_summary = false;

        push_output_block(
            OutputContentBlock::ToolUse {
                id: "tool-1".to_string(),
                name: "read_file".to_string(),
                input: json!({}),
            },
            &mut out,
            &mut events,
            &mut pending_tool,
            true,
            &mut block_has_thinking_summary,
            false,
        )
        .expect("tool block should accumulate");

        assert!(events.is_empty());
        assert_eq!(
            pending_tool,
            Some(("tool-1".to_string(), "read_file".to_string(), String::new(),))
        );
    }

    #[test]
    fn response_to_events_preserves_empty_object_json_input_outside_streaming() {
        let mut out = Vec::new();
        let events = response_to_events(
            MessageResponse {
                id: "msg-1".to_string(),
                kind: "message".to_string(),
                model: "Himalaya-opus-4-6".to_string(),
                role: "assistant".to_string(),
                content: vec![OutputContentBlock::ToolUse {
                    id: "tool-1".to_string(),
                    name: "read_file".to_string(),
                    input: json!({}),
                }],
                stop_reason: Some("tool_use".to_string()),
                stop_sequence: None,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                },
                request_id: None,
            },
            &mut out,
            false,
        )
        .expect("response conversion should succeed");

        assert!(matches!(
            &events[0],
            AssistantEvent::ToolUse { name, input, .. }
                if name == "read_file" && input == "{}"
        ));
    }

    #[test]
    fn response_to_events_preserves_non_empty_json_input_outside_streaming() {
        let mut out = Vec::new();
        let events = response_to_events(
            MessageResponse {
                id: "msg-2".to_string(),
                kind: "message".to_string(),
                model: "Himalaya-opus-4-6".to_string(),
                role: "assistant".to_string(),
                content: vec![OutputContentBlock::ToolUse {
                    id: "tool-2".to_string(),
                    name: "read_file".to_string(),
                    input: json!({ "path": "rust/Cargo.toml" }),
                }],
                stop_reason: Some("tool_use".to_string()),
                stop_sequence: None,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                },
                request_id: None,
            },
            &mut out,
            false,
        )
        .expect("response conversion should succeed");

        assert!(matches!(
            &events[0],
            AssistantEvent::ToolUse { name, input, .. }
                if name == "read_file" && input == "{\"path\":\"rust/Cargo.toml\"}"
        ));
    }

    #[test]
    fn response_to_events_renders_collapsed_thinking_summary() {
        let mut out = Vec::new();
        let events = response_to_events(
            MessageResponse {
                id: "msg-3".to_string(),
                kind: "message".to_string(),
                model: "Himalaya-opus-4-6".to_string(),
                role: "assistant".to_string(),
                content: vec![
                    OutputContentBlock::Thinking {
                        thinking: "step 1".to_string(),
                        signature: Some("sig_123".to_string()),
                    },
                    OutputContentBlock::Text {
                        text: "Final answer".to_string(),
                    },
                ],
                stop_reason: Some("end_turn".to_string()),
                stop_sequence: None,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                },
                request_id: None,
            },
            &mut out,
            false,
        )
        .expect("response conversion should succeed");

        assert!(matches!(
            &events[0],
            AssistantEvent::ReasoningStep(runtime::ReasoningStep::Analysis {
                content,
                signature,
                ..
            }) if content == "step 1" && signature.as_deref() == Some("sig_123")
        ));
        assert!(matches!(
            &events[1],
            AssistantEvent::TextDelta(text) if text == "Final answer"
        ));
        let rendered = String::from_utf8(out).expect("utf8");
        assert!(rendered.contains("▶ Thinking (6 chars hidden)"));
        assert!(!rendered.contains("step 1"));
    }

    #[test]
    fn login_browser_failure_keeps_json_stdout_clean() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let error = std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no supported browser opener command found",
        );

        super::emit_login_browser_open_failure(
            CliOutputFormat::Json,
            "https://example.test/oauth/authorize",
            &error,
            &mut stdout,
            &mut stderr,
        )
        .expect("browser warning should render");

        assert!(stdout.is_empty());
        let stderr = String::from_utf8(stderr).expect("utf8");
        assert!(stderr.contains("failed to open browser automatically"));
        assert!(stderr.contains("Open this URL manually:"));
        assert!(stderr.contains("https://example.test/oauth/authorize"));
    }

    #[test]
    fn build_runtime_plugin_state_merges_plugin_hooks_into_runtime_features() {
        let config_home = temp_dir();
        let workspace = temp_dir();
        let source_root = temp_dir();
        fs::create_dir_all(&config_home).expect("config home");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(&source_root).expect("source root");
        write_plugin_fixture(&source_root, "hook-runtime-demo", true, false);

        let mut manager = PluginManager::new(PluginManagerConfig::new(&config_home));
        manager
            .install(source_root.to_str().expect("utf8 source path"))
            .expect("plugin install should succeed");
        let loader = ConfigLoader::new(&workspace, &config_home);
        let runtime_config = loader.load().expect("runtime config should load");
        let state = build_runtime_plugin_state_with_loader(&workspace, &loader, &runtime_config)
            .expect("plugin state should load");
        let pre_hooks = state.feature_config.hooks().pre_tool_use();
        assert_eq!(pre_hooks.len(), 1);
        assert!(
            pre_hooks[0].ends_with("hooks/pre.sh"),
            "expected installed plugin hook path, got {pre_hooks:?}"
        );

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(workspace);
        let _ = fs::remove_dir_all(source_root);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn build_runtime_plugin_state_discovers_mcp_tools_and_surfaces_pending_servers() {
        let config_home = temp_dir();
        let workspace = temp_dir();
        fs::create_dir_all(&config_home).expect("config home");
        fs::create_dir_all(&workspace).expect("workspace");
        let script_path = workspace.join("fixture-mcp.py");
        write_mcp_server_fixture(&script_path);
        fs::write(
            config_home.join("settings.json"),
            format!(
                r#"{{
                  "mcpServers": {{
                    "alpha": {{
                      "command": "python3",
                      "args": ["{}"]
                    }},
                    "broken": {{
                      "command": "python3",
                      "args": ["-c", "import sys; sys.exit(0)"]
                    }}
                  }}
                }}"#,
                script_path.to_string_lossy()
            ),
        )
        .expect("write mcp settings");

        let loader = ConfigLoader::new(&workspace, &config_home);
        let runtime_config = loader.load().expect("runtime config should load");
        let state = build_runtime_plugin_state_with_loader(&workspace, &loader, &runtime_config)
            .expect("runtime plugin state should load");

        let allowed = state
            .tool_registry
            .normalize_allowed_tools(&["mcp__alpha__echo".to_string(), "MCPTool".to_string()])
            .expect("mcp tools should be allow-listable")
            .expect("allow-list should exist");
        assert!(allowed.contains("mcp__alpha__echo"));
        assert!(allowed.contains("MCPTool"));

        let mut executor = CliToolExecutor::new(
            None,
            false,
            false,
            state.tool_registry.clone(),
            state.mcp_state.clone(),
        );

        let tool_output = executor
            .execute("mcp__alpha__echo", r#"{"text":"hello"}"#)
            .expect("discovered mcp tool should execute");
        let tool_json: serde_json::Value =
            serde_json::from_str(&tool_output).expect("tool output should be json");
        assert_eq!(tool_json["structuredContent"]["echoed"], "hello");

        let wrapped_output = executor
            .execute(
                "MCPTool",
                r#"{"qualifiedName":"mcp__alpha__echo","arguments":{"text":"wrapped"}}"#,
            )
            .expect("generic mcp wrapper should execute");
        let wrapped_json: serde_json::Value =
            serde_json::from_str(&wrapped_output).expect("wrapped output should be json");
        assert_eq!(wrapped_json["structuredContent"]["echoed"], "wrapped");

        let search_output = executor
            .execute("ToolSearch", r#"{"query":"alpha echo","max_results":5}"#)
            .expect("tool search should execute");
        let search_json: serde_json::Value =
            serde_json::from_str(&search_output).expect("search output should be json");
        assert_eq!(search_json["matches"][0], "mcp__alpha__echo");
        assert_eq!(search_json["pending_mcp_servers"][0], "broken");
        assert_eq!(
            search_json["mcp_degraded"]["failed_servers"][0]["server_name"],
            "broken"
        );
        assert_eq!(
            search_json["mcp_degraded"]["failed_servers"][0]["phase"],
            "tool_discovery"
        );
        assert_eq!(
            search_json["mcp_degraded"]["available_tools"][0],
            "mcp__alpha__echo"
        );

        let listed = executor
            .execute("ListMcpResourcesTool", r#"{"server":"alpha"}"#)
            .expect("resources should list");
        let listed_json: serde_json::Value =
            serde_json::from_str(&listed).expect("resource output should be json");
        assert_eq!(listed_json["resources"][0]["uri"], "file://guide.txt");

        let read = executor
            .execute(
                "ReadMcpResourceTool",
                r#"{"server":"alpha","uri":"file://guide.txt"}"#,
            )
            .expect("resource should read");
        let read_json: serde_json::Value =
            serde_json::from_str(&read).expect("resource read output should be json");
        assert_eq!(
            read_json["contents"][0]["text"],
            "contents for file://guide.txt"
        );

        if let Some(mcp_state) = state.mcp_state {
            mcp_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .shutdown()
                .expect("mcp shutdown should succeed");
        }

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn build_runtime_plugin_state_surfaces_unsupported_mcp_servers_structurally() {
        let config_home = temp_dir();
        let workspace = temp_dir();
        fs::create_dir_all(&config_home).expect("config home");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::write(
            config_home.join("settings.json"),
            r#"{
              "mcpServers": {
                "remote": {
                  "url": "https://example.test/mcp"
                }
              }
            }"#,
        )
        .expect("write mcp settings");

        let loader = ConfigLoader::new(&workspace, &config_home);
        let runtime_config = loader.load().expect("runtime config should load");
        let state = build_runtime_plugin_state_with_loader(&workspace, &loader, &runtime_config)
            .expect("runtime plugin state should load");
        let mut executor = CliToolExecutor::new(
            None,
            false,
            false,
            state.tool_registry.clone(),
            state.mcp_state.clone(),
        );

        let search_output = executor
            .execute("ToolSearch", r#"{"query":"remote","max_results":5}"#)
            .expect("tool search should execute");
        let search_json: serde_json::Value =
            serde_json::from_str(&search_output).expect("search output should be json");
        assert_eq!(search_json["pending_mcp_servers"][0], "remote");
        assert_eq!(
            search_json["mcp_degraded"]["failed_servers"][0]["server_name"],
            "remote"
        );
        assert_eq!(
            search_json["mcp_degraded"]["failed_servers"][0]["phase"],
            "server_registration"
        );
        assert_eq!(
            search_json["mcp_degraded"]["failed_servers"][0]["error"]["context"]["transport"],
            "http"
        );

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn build_runtime_runs_plugin_lifecycle_init_and_shutdown() {
        // Serialize access to process-wide env vars so parallel tests that
        // set/remove ANTHROPIC_API_KEY do not race with this test.
        let _guard = env_lock();
        let config_home = temp_dir();
        // Inject a dummy API key so runtime construction succeeds without real credentials.
        // This test only exercises plugin lifecycle (init/shutdown), never calls the API.
        std::env::set_var("ANTHROPIC_API_KEY", "test-dummy-key-for-plugin-lifecycle");
        let workspace = temp_dir();
        let source_root = temp_dir();
        fs::create_dir_all(&config_home).expect("config home");
        fs::create_dir_all(&workspace).expect("workspace");
        fs::create_dir_all(&source_root).expect("source root");
        write_plugin_fixture(&source_root, "lifecycle-runtime-demo", false, true);

        let mut manager = PluginManager::new(PluginManagerConfig::new(&config_home));
        let install = manager
            .install(source_root.to_str().expect("utf8 source path"))
            .expect("plugin install should succeed");
        let log_path = install.install_path.join("lifecycle.log");
        let loader = ConfigLoader::new(&workspace, &config_home);
        let runtime_config = loader.load().expect("runtime config should load");
        let runtime_plugin_state =
            build_runtime_plugin_state_with_loader(&workspace, &loader, &runtime_config)
                .expect("plugin state should load");
        let mut runtime = build_runtime_with_plugin_state(
            Session::new(),
            "runtime-plugin-lifecycle",
            DEFAULT_MODEL.to_string(),
            vec!["test system prompt".to_string()],
            true,
            false,
            false,
            None,
            default_permission_mode_for_tests(),
            None,
            runtime_plugin_state,
        )
        .expect("runtime should build");

        assert_eq!(
            fs::read_to_string(&log_path).expect("init log should exist"),
            "init\n"
        );

        runtime
            .shutdown_plugins()
            .expect("plugin shutdown should succeed");

        assert_eq!(
            fs::read_to_string(&log_path).expect("shutdown log should exist"),
            "init\nshutdown\n"
        );

        let _ = fs::remove_dir_all(config_home);
        let _ = fs::remove_dir_all(workspace);
        let _ = fs::remove_dir_all(source_root);
        std::env::remove_var("ANTHROPIC_API_KEY");
    }

    #[test]
    fn rejects_invalid_reasoning_effort_value() {
        let err = parse_args(&[
            "--reasoning-effort".to_string(),
            "turbo".to_string(),
            "prompt".to_string(),
            "hello".to_string(),
        ])
        .unwrap_err();
        assert!(
            err.contains("invalid value for --reasoning-effort"),
            "unexpected error: {err}"
        );
        assert!(err.contains("turbo"), "unexpected error: {err}");
    }

    #[test]
    fn accepts_valid_reasoning_effort_values() {
        for value in ["low", "medium", "high"] {
            let result = parse_args(&[
                "--reasoning-effort".to_string(),
                value.to_string(),
                "prompt".to_string(),
                "hello".to_string(),
            ]);
            assert!(
                result.is_ok(),
                "--reasoning-effort {value} should be accepted, got: {result:?}",
            );
            if let Ok(CliAction::Prompt {
                reasoning_effort, ..
            }) = result
            {
                assert_eq!(reasoning_effort.as_deref(), Some(value));
            }
        }
    }

    #[test]
    fn stub_commands_absent_from_repl_completions() {
        let candidates =
            slash_command_completion_candidates_with_sessions("Himalaya-3-5-sonnet", None, vec![]);
        for stub in commands::stub_slash_commands() {
            let with_slash = format!("/{stub}");
            assert!(
                !candidates.contains(&with_slash),
                "stub command {with_slash} should not appear in REPL completions"
            );
        }
    }
}

fn write_mcp_server_fixture(script_path: &Path) {
    let script = [
            "#!/usr/bin/env python3",
            "import json, sys",
            "",
            "def read_message():",
            "    header = b''",
            r"    while not header.endswith(b'\r\n\r\n'):",
            "        chunk = sys.stdin.buffer.read(1)",
            "        if not chunk:",
            "            return None",
            "        header += chunk",
            "    length = 0",
            r"    for line in header.decode().split('\r\n'):",
            r"        if line.lower().startswith('content-length:'):",
            "            length = int(line.split(':', 1)[1].strip())",
            "    payload = sys.stdin.buffer.read(length)",
            "    return json.loads(payload.decode())",
            "",
            "def send_message(message):",
            "    payload = json.dumps(message).encode()",
            r"    sys.stdout.buffer.write(f'Content-Length: {len(payload)}\r\n\r\n'.encode() + payload)",
            "    sys.stdout.buffer.flush()",
            "",
            "while True:",
            "    request = read_message()",
            "    if request is None:",
            "        break",
            "    method = request['method']",
            "    if method == 'initialize':",
            "        send_message({",
            "            'jsonrpc': '2.0',",
            "            'id': request['id'],",
            "            'result': {",
            "                'protocolVersion': request['params']['protocolVersion'],",
            "                'capabilities': {'tools': {}, 'resources': {}},",
            "                'serverInfo': {'name': 'fixture', 'version': '1.0.0'}",
            "            }",
            "        })",
            "    elif method == 'tools/list':",
            "        send_message({",
            "            'jsonrpc': '2.0',",
            "            'id': request['id'],",
            "            'result': {",
            "                'tools': [",
            "                    {",
            "                        'name': 'echo',",
            "                        'description': 'Echo from MCP fixture',",
            "                        'inputSchema': {",
            "                            'type': 'object',",
            "                            'properties': {'text': {'type': 'string'}},",
            "                            'required': ['text'],",
            "                            'additionalProperties': False",
            "                        },",
            "                        'annotations': {'readOnlyHint': True}",
            "                    }",
            "                ]",
            "            }",
            "        })",
            "    elif method == 'tools/call':",
            "        args = request['params'].get('arguments') or {}",
            "        send_message({",
            "            'jsonrpc': '2.0',",
            "            'id': request['id'],",
            "            'result': {",
            "                'content': [{'type': 'text', 'text': f\"echo:{args.get('text', '')}\"}],",
            "                'structuredContent': {'echoed': args.get('text', '')},",
            "                'isError': False",
            "            }",
            "        })",
            "    elif method == 'resources/list':",
            "        send_message({",
            "            'jsonrpc': '2.0',",
            "            'id': request['id'],",
            "            'result': {",
            "                'resources': [{'uri': 'file://guide.txt', 'name': 'guide', 'mimeType': 'text/plain'}]",
            "            }",
            "        })",
            "    elif method == 'resources/read':",
            "        uri = request['params']['uri']",
            "        send_message({",
            "            'jsonrpc': '2.0',",
            "            'id': request['id'],",
            "            'result': {",
            "                'contents': [{'uri': uri, 'mimeType': 'text/plain', 'text': f'contents for {uri}'}]",
            "            }",
            "        })",
            "    else:",
            "        send_message({",
            "            'jsonrpc': '2.0',",
            "            'id': request['id'],",
            "            'error': {'code': -32601, 'message': method}",
            "        })",
            "",
        ]
        .join("\n");
    fs::write(script_path, script).expect("mcp fixture script should write");
}

#[cfg(test)]
mod sandbox_report_tests {
    use super::{format_sandbox_report, HookAbortMonitor};
    use runtime::HookAbortSignal;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn sandbox_report_renders_expected_fields() {
        let report = format_sandbox_report(&runtime::SandboxStatus::default());
        assert!(report.contains("Sandbox"));
        assert!(report.contains("Enabled"));
        assert!(report.contains("Filesystem mode"));
        assert!(report.contains("Fallback reason"));
    }

    #[test]
    fn hook_abort_monitor_stops_without_aborting() {
        let abort_signal = HookAbortSignal::new();
        let (ready_tx, ready_rx) = mpsc::channel();
        let monitor = HookAbortMonitor::spawn_with_waiter(
            abort_signal.clone(),
            move |stop_rx, abort_signal| {
                ready_tx.send(()).expect("ready signal");
                let _ = stop_rx.recv();
                assert!(!abort_signal.is_aborted());
            },
        );

        ready_rx.recv().expect("waiter should be ready");
        monitor.stop();

        assert!(!abort_signal.is_aborted());
    }

    #[test]
    fn hook_abort_monitor_propagates_interrupt() {
        let abort_signal = HookAbortSignal::new();
        let (done_tx, done_rx) = mpsc::channel();
        let monitor = HookAbortMonitor::spawn_with_waiter(
            abort_signal.clone(),
            move |_stop_rx, abort_signal| {
                abort_signal.abort();
                done_tx.send(()).expect("done signal");
            },
        );

        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("interrupt should complete");
        monitor.stop();

        assert!(abort_signal.is_aborted());
    }
}
