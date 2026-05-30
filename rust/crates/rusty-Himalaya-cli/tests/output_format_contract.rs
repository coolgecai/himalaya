use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn benchmark_commands_emit_suite_and_record_runs() {
    let root = unique_temp_dir("benchmark-json");
    fs::create_dir_all(&root).expect("temp dir should exist");

    let suite = assert_json_command(&root, &["--output-format", "json", "benchmark", "list"]);
    assert_eq!(suite["type"], "benchmark_suite");
    assert_eq!(suite["tasks"].as_array().expect("tasks array").len(), 10);

    let first_task = suite["tasks"][0]["id"]
        .as_str()
        .expect("task id")
        .to_string();
    let task = assert_json_command(
        &root,
        &["--output-format", "json", "benchmark", "show", &first_task],
    );
    assert_eq!(task["type"], "benchmark_task");
    assert_eq!(task["task"]["id"], first_task);

    let run = assert_json_command(
        &root,
        &[
            "--output-format",
            "json",
            "benchmark",
            "run",
            "--record",
            "--max-parallelism",
            "3",
        ],
    );
    assert_eq!(run["type"], "benchmark_run");
    assert_eq!(run["run"]["summary"]["total_tasks"], 10);
    assert_eq!(run["run"]["max_parallelism"], 3);
    assert!(
        run["run"]["summary"]["average_total_score"]
            .as_f64()
            .expect("average score")
            > 0.5
    );
    assert_eq!(run["run"]["summary"]["completed_harness_tasks"], 10);
    assert!(
        run["run"]["summary"]["total_scheduler_ticks"]
            .as_u64()
            .expect("scheduler tick count")
            >= 10
    );
    assert!(
        run["run"]["results"][0]["harness"]["worker_dispatches"]
            .as_u64()
            .expect("worker dispatch count")
            > 0
    );
    assert!(
        run["run"]["results"][0]["score"]["execution_score"]
            .as_f64()
            .expect("execution score")
            > 0.0
    );
    assert!(root.join(".Himalaya/benchmarks/runs.jsonl").exists());
}

#[test]
fn help_emits_json_when_requested() {
    let root = unique_temp_dir("help-json");
    fs::create_dir_all(&root).expect("temp dir should exist");

    let parsed = assert_json_command(&root, &["--output-format", "json", "help"]);
    assert_eq!(parsed["kind"], "help");
    assert!(parsed["message"]
        .as_str()
        .expect("help text")
        .contains("Usage:"));
}

#[test]
fn version_emits_json_when_requested() {
    let root = unique_temp_dir("version-json");
    fs::create_dir_all(&root).expect("temp dir should exist");

    let parsed = assert_json_command(&root, &["--output-format", "json", "version"]);
    assert_eq!(parsed["kind"], "version");
    assert_eq!(parsed["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn status_and_sandbox_emit_json_when_requested() {
    let root = unique_temp_dir("status-sandbox-json");
    fs::create_dir_all(&root).expect("temp dir should exist");

    let status = assert_json_command(&root, &["--output-format", "json", "status"]);
    assert_eq!(status["kind"], "status");
    assert!(status["workspace"]["cwd"].as_str().is_some());

    let sandbox = assert_json_command(&root, &["--output-format", "json", "sandbox"]);
    assert_eq!(sandbox["kind"], "sandbox");
    assert!(sandbox["filesystem_mode"].as_str().is_some());
}

#[test]
fn inventory_commands_emit_structured_json_when_requested() {
    let root = unique_temp_dir("inventory-json");
    fs::create_dir_all(&root).expect("temp dir should exist");

    let isolated_home = root.join("home");
    let isolated_config = root.join("config-home");
    let isolated_codex = root.join("codex-home");
    fs::create_dir_all(&isolated_home).expect("isolated home should exist");

    let agents = assert_json_command_with_env(
        &root,
        &["--output-format", "json", "agents"],
        &[
            ("HOME", isolated_home.to_str().expect("utf8 home")),
            (
                "Himalaya_CONFIG_HOME",
                isolated_config.to_str().expect("utf8 config home"),
            ),
            (
                "CODEX_HOME",
                isolated_codex.to_str().expect("utf8 codex home"),
            ),
        ],
    );
    assert_eq!(agents["kind"], "agents");
    assert_eq!(agents["action"], "list");
    assert_eq!(agents["count"], 0);
    assert_eq!(agents["summary"]["active"], 0);
    assert!(agents["agents"]
        .as_array()
        .expect("agents array")
        .is_empty());

    let mcp = assert_json_command(&root, &["--output-format", "json", "mcp"]);
    assert_eq!(mcp["kind"], "mcp");
    assert_eq!(mcp["action"], "list");

    let skills = assert_json_command(&root, &["--output-format", "json", "skills"]);
    assert_eq!(skills["kind"], "skills");
    assert_eq!(skills["action"], "list");
}

#[test]
fn task_packet_commands_persist_structured_tasks() {
    let root = unique_temp_dir("task-packet-json");
    fs::create_dir_all(&root).expect("temp dir should exist");
    let packet_path = root.join("packet.json");
    fs::write(
        &packet_path,
        r#"{
  "objective": "Fix the parser regression",
  "scope": "rust/crates/rusty-Himalaya-cli",
  "repo": ".",
  "branch_policy": "use current branch",
  "acceptance_tests": ["cargo test --workspace --manifest-path rust/Cargo.toml -p rusty-Himalaya-cli"],
  "commit_policy": "do not commit automatically",
  "reporting_contract": "return JSON status and verification evidence",
  "escalation_policy": "ask the user if tests cannot run"
}
"#,
    )
    .expect("packet fixture should write");

    let packet_arg = packet_path.to_str().expect("packet path should be utf8");
    let created = assert_json_command(
        &root,
        &[
            "--output-format",
            "json",
            "tasks",
            "packet",
            "create",
            packet_arg,
        ],
    );
    assert_eq!(created["type"], "task_packet_create");
    assert_eq!(created["task"]["prompt"], "Fix the parser regression");
    assert_eq!(created["task"]["status"], "created");
    assert!(created["task"]["plan"].is_object());
    assert!(created["task"]["plan"]["dag"].is_object());
    assert!(created["task"]["plan"]["execution"].is_object());
    assert!(created["event_log"]
        .as_array()
        .expect("created event log")
        .iter()
        .any(|entry| entry["event"] == "plan_recorded"));
    assert!(created["event_log"]
        .as_array()
        .expect("created event log")
        .iter()
        .any(|entry| entry["event"] == "created"));
    assert_eq!(
        created["verification_handoff"]["policy"], "full",
        "workspace acceptance tests should request full verification"
    );

    let task_id = created["task"]["task_id"]
        .as_str()
        .expect("created task id")
        .to_string();
    let status = assert_json_command(
        &root,
        &[
            "--output-format",
            "json",
            "tasks",
            "packet",
            "status",
            &task_id,
        ],
    );
    assert_eq!(status["type"], "task_packet_status");
    assert_eq!(status["task"]["task_id"], task_id);
    assert!(status["task"]["task_packet"].is_object());
    assert!(root.join(".Himalaya/tasks/tasks.json").exists());
    assert!(root.join(".Himalaya/tasks/ledger.jsonl").exists());
    let event_log_path = root.join(".Himalaya/tasks/events.jsonl");
    assert!(event_log_path.exists());
    assert_eq!(
        fs::read_to_string(&event_log_path)
            .expect("event log should read")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        2
    );

    let running = assert_json_command(
        &root,
        &[
            "--output-format",
            "json",
            "tasks",
            "packet",
            "run",
            packet_arg,
        ],
    );
    assert_eq!(running["type"], "task_packet_run");
    assert_eq!(running["task"]["status"], "running");
    assert!(running["event_log"]
        .as_array()
        .expect("running event log")
        .iter()
        .any(|entry| entry["event"] == "status_changed"));
    assert_eq!(
        fs::read_to_string(&event_log_path)
            .expect("event log should read")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        5
    );
}

#[test]
fn task_scheduler_tick_persists_durable_status() {
    let root = unique_temp_dir("task-scheduler-json");
    fs::create_dir_all(&root).expect("temp dir should exist");
    let packet_path = root.join("packet.json");
    fs::write(
        &packet_path,
        r#"{
  "objective": "Schedule the durable task",
  "scope": "rust/crates/rusty-Himalaya-cli",
  "repo": ".",
  "branch_policy": "use current branch",
  "acceptance_tests": [],
  "commit_policy": "do not commit automatically",
  "reporting_contract": "return scheduler status",
  "escalation_policy": "ask the user if blocked"
}
"#,
    )
    .expect("packet fixture should write");

    let packet_arg = packet_path.to_str().expect("packet path should be utf8");
    let created = assert_json_command(
        &root,
        &[
            "--output-format",
            "json",
            "tasks",
            "packet",
            "create",
            packet_arg,
        ],
    );
    let task_id = created["task"]["task_id"]
        .as_str()
        .expect("created task id")
        .to_string();

    let queue = assert_json_command(
        &root,
        &["--output-format", "json", "tasks", "scheduler", "queue"],
    );
    assert_eq!(queue["type"], "task_scheduler_queue");
    assert_eq!(queue["queue"][0]["task_id"], task_id);
    assert_eq!(queue["queue"][0]["status"], "pending");

    let tick = assert_json_command(
        &root,
        &["--output-format", "json", "tasks", "scheduler", "tick"],
    );
    assert_eq!(tick["type"], "task_scheduler_tick");
    assert_eq!(tick["tick"]["selected_task_id"], task_id);
    assert_eq!(tick["tick"]["status"], "running");
    assert_eq!(tick["tick"]["task"]["status"], "running");
    assert_eq!(tick["tick"]["outcome"]["blocked"], false);
    assert!(tick["tick"]["outcome"]["steps"]
        .as_array()
        .expect("steps array")
        .iter()
        .any(|step| step["kind"] == "dispatch_worker"));
    assert!(root.join(".Himalaya/workers/workers.json").exists());

    let second_tick = assert_json_command(
        &root,
        &["--output-format", "json", "tasks", "scheduler", "tick"],
    );
    assert_eq!(second_tick["tick"]["status"], "running");
    assert_eq!(second_tick["tick"]["task"]["status"], "running");
    assert!(second_tick["tick"]["outcome"]["steps"]
        .as_array()
        .expect("second tick steps array")
        .iter()
        .any(|step| step["kind"] == "await_worker"));

    let daemon = assert_json_command(
        &root,
        &[
            "--output-format",
            "json",
            "tasks",
            "scheduler",
            "run",
            "--once",
        ],
    );
    assert_eq!(daemon["type"], "task_scheduler_daemon_run");
    assert!(daemon["runs"].as_array().expect("daemon runs array").len() >= 1);
    assert_eq!(daemon["state"]["tick_count"], 1);

    let daemon_status = assert_json_command(
        &root,
        &["--output-format", "json", "tasks", "scheduler", "status"],
    );
    assert_eq!(daemon_status["type"], "task_scheduler_daemon_status");
    assert_eq!(daemon_status["state"]["tick_count"], 1);
    assert!(root.join(".Himalaya/scheduler/state.json").exists());
    assert!(root.join(".Himalaya/scheduler/events.jsonl").exists());
    assert!(root.join(".Himalaya/tasks/tasks.json").exists());
    assert!(root.join(".Himalaya/tasks/events.jsonl").exists());
}

#[test]
fn worker_supervisor_commands_persist_worker_state() {
    let root = unique_temp_dir("worker-supervisor-json");
    fs::create_dir_all(&root).expect("temp dir should exist");

    let created = assert_json_command(
        &root,
        &[
            "--output-format",
            "json",
            "workers",
            "create",
            "--cwd",
            root.to_str().expect("root should be utf8"),
            "--trusted-root",
            root.to_str().expect("root should be utf8"),
        ],
    );
    assert_eq!(created["type"], "worker_create");
    assert_eq!(created["worker"]["status"], "spawning");
    let worker_id = created["worker"]["worker_id"]
        .as_str()
        .expect("worker id")
        .to_string();

    let observed = assert_json_command(
        &root,
        &[
            "--output-format",
            "json",
            "workers",
            "observe",
            &worker_id,
            "Ready for input >",
        ],
    );
    assert_eq!(observed["type"], "worker_observe");
    assert_eq!(observed["worker"]["status"], "ready_for_prompt");

    let prompted = assert_json_command(
        &root,
        &[
            "--output-format",
            "json",
            "workers",
            "prompt",
            &worker_id,
            "Implement worker supervision",
        ],
    );
    assert_eq!(prompted["type"], "worker_prompt");
    assert_eq!(prompted["worker"]["status"], "prompt_accepted");

    let running = assert_json_command(
        &root,
        &[
            "--output-format",
            "json",
            "workers",
            "observe",
            &worker_id,
            "Thinking about tests",
        ],
    );
    assert_eq!(running["worker"]["status"], "running");

    let supervise =
        assert_json_command(&root, &["--output-format", "json", "workers", "supervise"]);
    assert_eq!(supervise["type"], "worker_supervisor_tick");
    assert_eq!(supervise["tick"]["status"], "running");
    assert_eq!(supervise["tick"]["active_workers"], 1);

    let completed = assert_json_command(
        &root,
        &[
            "--output-format",
            "json",
            "workers",
            "complete",
            &worker_id,
            "stop",
            "12",
        ],
    );
    assert_eq!(completed["type"], "worker_complete");
    assert_eq!(completed["worker"]["status"], "finished");

    let supervise_after_completion =
        assert_json_command(&root, &["--output-format", "json", "workers", "supervise"]);
    assert_eq!(supervise_after_completion["tick"]["status"], "idle");
    assert_eq!(supervise_after_completion["tick"]["active_workers"], 0);
    assert!(root.join(".Himalaya/workers/workers.json").exists());
}

#[test]
fn agents_command_emits_structured_agent_entries_when_requested() {
    let root = unique_temp_dir("agents-json-populated");
    let workspace = root.join("workspace");
    let project_agents = workspace.join(".codex").join("agents");
    let home = root.join("home");
    let user_agents = home.join(".codex").join("agents");
    let isolated_config = root.join("config-home");
    let isolated_codex = root.join("codex-home");
    fs::create_dir_all(&workspace).expect("workspace should exist");
    write_agent(
        &project_agents,
        "planner",
        "Project planner",
        "gpt-5.4",
        "medium",
    );
    write_agent(
        &project_agents,
        "verifier",
        "Verification agent",
        "gpt-5.4-mini",
        "high",
    );
    write_agent(
        &user_agents,
        "planner",
        "User planner",
        "gpt-5.4-mini",
        "high",
    );

    let parsed = assert_json_command_with_env(
        &workspace,
        &["--output-format", "json", "agents"],
        &[
            ("HOME", home.to_str().expect("utf8 home")),
            (
                "Himalaya_CONFIG_HOME",
                isolated_config.to_str().expect("utf8 config home"),
            ),
            (
                "CODEX_HOME",
                isolated_codex.to_str().expect("utf8 codex home"),
            ),
        ],
    );

    assert_eq!(parsed["kind"], "agents");
    assert_eq!(parsed["action"], "list");
    assert_eq!(parsed["count"], 3);
    assert_eq!(parsed["summary"]["active"], 2);
    assert_eq!(parsed["summary"]["shadowed"], 1);
    assert_eq!(parsed["agents"][0]["name"], "planner");
    assert_eq!(parsed["agents"][0]["source"]["id"], "project_Himalaya");
    assert_eq!(parsed["agents"][0]["active"], true);
    assert_eq!(parsed["agents"][1]["name"], "verifier");
    assert_eq!(parsed["agents"][2]["name"], "planner");
    assert_eq!(parsed["agents"][2]["active"], false);
    assert_eq!(parsed["agents"][2]["shadowed_by"]["id"], "project_Himalaya");
}

#[test]
fn bootstrap_and_system_prompt_emit_json_when_requested() {
    let root = unique_temp_dir("bootstrap-system-prompt-json");
    fs::create_dir_all(&root).expect("temp dir should exist");

    let plan = assert_json_command(&root, &["--output-format", "json", "bootstrap-plan"]);
    assert_eq!(plan["kind"], "bootstrap-plan");
    assert!(plan["phases"].as_array().expect("phases").len() > 1);

    let prompt = assert_json_command(&root, &["--output-format", "json", "system-prompt"]);
    assert_eq!(prompt["kind"], "system-prompt");
    assert!(prompt["message"]
        .as_str()
        .expect("prompt text")
        .contains("interactive agent"));
}

#[test]
fn dump_manifests_and_init_emit_json_when_requested() {
    let root = unique_temp_dir("manifest-init-json");
    fs::create_dir_all(&root).expect("temp dir should exist");

    let upstream = write_upstream_fixture(&root);
    let manifests = assert_json_command_with_env(
        &root,
        &["--output-format", "json", "dump-manifests"],
        &[(
            "Himalaya_CODE_UPSTREAM",
            upstream.to_str().expect("utf8 upstream"),
        )],
    );
    assert_eq!(manifests["kind"], "dump-manifests");
    assert_eq!(manifests["commands"], 1);
    assert_eq!(manifests["tools"], 1);

    let workspace = root.join("workspace");
    fs::create_dir_all(&workspace).expect("workspace should exist");
    let init = assert_json_command(&workspace, &["--output-format", "json", "init"]);
    assert_eq!(init["kind"], "init");
    assert!(workspace.join("Himalaya.md").exists());
}

#[test]
fn doctor_and_resume_status_emit_json_when_requested() {
    let root = unique_temp_dir("doctor-resume-json");
    fs::create_dir_all(&root).expect("temp dir should exist");

    let doctor = assert_json_command(&root, &["--output-format", "json", "doctor"]);
    assert_eq!(doctor["kind"], "doctor");
    assert!(doctor["message"].is_string());
    let summary = doctor["summary"].as_object().expect("doctor summary");
    assert!(summary["ok"].as_u64().is_some());
    assert!(summary["warnings"].as_u64().is_some());
    assert!(summary["failures"].as_u64().is_some());

    let checks = doctor["checks"].as_array().expect("doctor checks");
    assert_eq!(checks.len(), 5);
    let check_names = checks
        .iter()
        .map(|check| {
            assert!(check["status"].as_str().is_some());
            assert!(check["summary"].as_str().is_some());
            assert!(check["details"].is_array());
            check["name"].as_str().expect("doctor check name")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        check_names,
        vec!["auth", "config", "workspace", "sandbox", "system"]
    );

    let workspace = checks
        .iter()
        .find(|check| check["name"] == "workspace")
        .expect("workspace check");
    assert!(workspace["cwd"].as_str().is_some());
    assert!(workspace["in_git_repo"].is_boolean());

    let sandbox = checks
        .iter()
        .find(|check| check["name"] == "sandbox")
        .expect("sandbox check");
    assert!(sandbox["filesystem_mode"].as_str().is_some());
    assert!(sandbox["enabled"].is_boolean());
    assert!(sandbox["fallback_reason"].is_null() || sandbox["fallback_reason"].is_string());

    let session_path = root.join("session.jsonl");
    fs::write(
        &session_path,
        "{\"type\":\"session_meta\",\"version\":3,\"session_id\":\"resume-json\",\"created_at_ms\":0,\"updated_at_ms\":0}\n{\"type\":\"message\",\"message\":{\"role\":\"user\",\"blocks\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n",
    )
    .expect("session should write");
    let resumed = assert_json_command(
        &root,
        &[
            "--output-format",
            "json",
            "--resume",
            session_path.to_str().expect("utf8 session path"),
            "/status",
        ],
    );
    assert_eq!(resumed["kind"], "status");
    // model is null in resume mode (not known without --model flag)
    assert!(resumed["model"].is_null());
    assert_eq!(resumed["usage"]["messages"], 1);
    assert!(resumed["workspace"]["cwd"].as_str().is_some());
    assert!(resumed["sandbox"]["filesystem_mode"].as_str().is_some());
}

#[test]
fn resumed_inventory_commands_emit_structured_json_when_requested() {
    let root = unique_temp_dir("resume-inventory-json");
    let config_home = root.join("config-home");
    let home = root.join("home");
    fs::create_dir_all(&config_home).expect("config home should exist");
    fs::create_dir_all(&home).expect("home should exist");

    let session_path = root.join("session.jsonl");
    fs::write(
        &session_path,
        "{\"type\":\"session_meta\",\"version\":3,\"session_id\":\"resume-inventory-json\",\"created_at_ms\":0,\"updated_at_ms\":0}\n{\"type\":\"message\",\"message\":{\"role\":\"user\",\"blocks\":[{\"type\":\"text\",\"text\":\"inventory\"}]}}\n",
    )
    .expect("session should write");

    let mcp = assert_json_command_with_env(
        &root,
        &[
            "--output-format",
            "json",
            "--resume",
            session_path.to_str().expect("utf8 session path"),
            "/mcp",
        ],
        &[
            (
                "Himalaya_CONFIG_HOME",
                config_home.to_str().expect("utf8 config home"),
            ),
            ("HOME", home.to_str().expect("utf8 home")),
        ],
    );
    assert_eq!(mcp["kind"], "mcp");
    assert_eq!(mcp["action"], "list");
    assert!(mcp["servers"].is_array());

    let skills = assert_json_command_with_env(
        &root,
        &[
            "--output-format",
            "json",
            "--resume",
            session_path.to_str().expect("utf8 session path"),
            "/skills",
        ],
        &[
            (
                "Himalaya_CONFIG_HOME",
                config_home.to_str().expect("utf8 config home"),
            ),
            ("HOME", home.to_str().expect("utf8 home")),
        ],
    );
    assert_eq!(skills["kind"], "skills");
    assert_eq!(skills["action"], "list");
    assert!(skills["summary"]["total"].is_number());
    assert!(skills["skills"].is_array());
}

#[test]
fn local_commands_emit_structured_json_when_requested() {
    let root = unique_temp_dir("local-command-json");
    fs::create_dir_all(&root).expect("temp dir should exist");

    let workspace = assert_json_command(&root, &["--output-format", "json", "workspace"]);
    assert_eq!(workspace["type"], "local_command");
    assert_eq!(workspace["command"], "workspace");
    assert_eq!(workspace["status"], "ok");
    assert_eq!(workspace["summary"], "workspace context loaded");
    assert!(workspace["workspace"]["changed_files"].is_number());

    let test = assert_json_command(&root, &["--output-format", "json", "test", "parser"]);
    assert_eq!(test["type"], "local_command");
    assert_eq!(test["command"], "test");
    assert_eq!(test["status"], "skipped");
    assert_eq!(test["request"]["argument"], "parser");
    assert!(test["execution"].is_null());
    assert!(test["summary"]
        .as_str()
        .expect("summary")
        .contains("no test command detected"));
}

#[test]
fn direct_slash_local_command_emits_stream_json_event() {
    let root = unique_temp_dir("local-command-stream-json");
    fs::create_dir_all(&root).expect("temp dir should exist");

    let workspace = assert_json_command(&root, &["--output-format", "stream-json", "/workspace"]);
    assert_eq!(workspace["type"], "local_command");
    assert_eq!(workspace["protocol_version"], 1);
    assert_eq!(workspace["command"], "workspace");
    assert_eq!(workspace["status"], "ok");
}

#[test]
fn resumed_version_and_init_emit_structured_json_when_requested() {
    let root = unique_temp_dir("resume-version-init-json");
    fs::create_dir_all(&root).expect("temp dir should exist");

    let session_path = root.join("session.jsonl");
    fs::write(
        &session_path,
        "{\"type\":\"session_meta\",\"version\":3,\"session_id\":\"resume-version-init-json\",\"created_at_ms\":0,\"updated_at_ms\":0}\n",
    )
    .expect("session should write");

    let version = assert_json_command(
        &root,
        &[
            "--output-format",
            "json",
            "--resume",
            session_path.to_str().expect("utf8 session path"),
            "/version",
        ],
    );
    assert_eq!(version["kind"], "version");
    assert_eq!(version["version"], env!("CARGO_PKG_VERSION"));

    let init = assert_json_command(
        &root,
        &[
            "--output-format",
            "json",
            "--resume",
            session_path.to_str().expect("utf8 session path"),
            "/init",
        ],
    );
    assert_eq!(init["kind"], "init");
    assert!(root.join("Himalaya.md").exists());
}

fn assert_json_command(current_dir: &Path, args: &[&str]) -> Value {
    assert_json_command_with_env(current_dir, args, &[])
}

fn assert_json_command_with_env(current_dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Value {
    let output = run_Himalaya(current_dir, args, envs);
    assert!(
        output.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("stdout should be valid json")
}

fn run_Himalaya(current_dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_Himalaya"));
    command.current_dir(current_dir).args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("Himalaya should launch")
}

fn write_upstream_fixture(root: &Path) -> PathBuf {
    let upstream = root.join("Himalaya-code");
    let src = upstream.join("src");
    let entrypoints = src.join("entrypoints");
    fs::create_dir_all(&entrypoints).expect("upstream entrypoints dir should exist");
    fs::write(
        src.join("commands.ts"),
        "import FooCommand from './commands/foo'\n",
    )
    .expect("commands fixture should write");
    fs::write(
        src.join("tools.ts"),
        "import ReadTool from './tools/read'\n",
    )
    .expect("tools fixture should write");
    fs::write(
        entrypoints.join("cli.tsx"),
        "if (args[0] === '--version') {}\nstartupProfiler()\n",
    )
    .expect("cli fixture should write");
    upstream
}

fn write_agent(root: &Path, name: &str, description: &str, model: &str, reasoning: &str) {
    fs::create_dir_all(root).expect("agent root should exist");
    fs::write(
        root.join(format!("{name}.toml")),
        format!(
            "name = \"{name}\"\ndescription = \"{description}\"\nmodel = \"{model}\"\nmodel_reasoning_effort = \"{reasoning}\"\n"
        ),
    )
    .expect("agent fixture should write");
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_millis();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "Himalaya-output-format-{label}-{}-{millis}-{counter}",
        std::process::id()
    ))
}
