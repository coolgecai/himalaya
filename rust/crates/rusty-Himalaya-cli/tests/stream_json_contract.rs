use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use mock_anthropic_service::{MockAnthropicService, SCENARIO_PREFIX};
use serde_json::Value;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn stream_json_text_events_include_protocol_version() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let workspace = HarnessWorkspace::new(unique_temp_dir("stream-json-text"));
    workspace.create();

    let events = run_stream_json_case(
        &workspace,
        server.base_url().as_str(),
        "streaming_text",
        None,
    );

    assert_all_events_are_versioned(&events);

    let session_meta = events.first().expect("session_meta should be first event");
    assert_eq!(session_meta["type"], "session_meta");
    assert_non_empty_string(&session_meta["session_id"]);
    assert_non_empty_string(&session_meta["session_path"]);
    assert_non_empty_string(&session_meta["model"]);
    let message_start = events
        .get(1)
        .expect("message_start should follow session_meta");
    assert_eq!(message_start["type"], "message_start");

    assert!(events.iter().any(|event| event["type"] == "message_start"));
    assert!(events.iter().any(|event| event["type"] == "text_delta"));
    assert!(events.iter().any(|event| event["type"] == "done"));
}

#[test]
fn stream_json_tool_events_match_contract() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let workspace = HarnessWorkspace::new(unique_temp_dir("stream-json-tool"));
    workspace.create();
    fs::write(workspace.root.join("fixture.txt"), "alpha parity line\n")
        .expect("fixture should write");

    let events = run_stream_json_case(
        &workspace,
        server.base_url().as_str(),
        "read_file_roundtrip",
        Some("read_file"),
    );

    assert_all_events_are_versioned(&events);

    let tool_use = events
        .iter()
        .find(|event| event["type"] == "tool_use")
        .expect("tool_use event should be present");
    assert!(tool_use["id"].as_str().is_some());
    assert_eq!(tool_use["name"], "read_file");
    assert!(tool_use["input"].is_object());

    let tool_result = events
        .iter()
        .find(|event| event["type"] == "tool_result")
        .expect("tool_result event should be present");
    assert_eq!(tool_result["name"], "read_file");
    assert!(tool_result["output"].as_str().is_some());
    assert_eq!(tool_result["is_error"], false);
    assert!(events.iter().any(|event| event["type"] == "done"));
}

#[test]
fn stream_json_permission_request_events_match_contract() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let workspace = HarnessWorkspace::new(unique_temp_dir("stream-json-permission-request"));
    workspace.create();

    let events = run_stream_json_case(
        &workspace,
        server.base_url().as_str(),
        "write_file_denied",
        Some("write_file"),
    );

    assert_all_events_are_versioned(&events);
    let permission_request = events
        .iter()
        .find(|event| event["type"] == "permission_request")
        .expect("permission_request event should be present");
    assert_eq!(permission_request["tool"], "write_file");
    assert_eq!(permission_request["current_mode"], "read-only");
    assert_eq!(permission_request["required_mode"], "workspace-write");
    assert_non_empty_string(&permission_request["input"]);
    assert_non_empty_string(&permission_request["reason"]);
}

#[test]
fn stream_json_permission_denial_events_match_contract() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let workspace = HarnessWorkspace::new(unique_temp_dir("stream-json-denial"));
    workspace.create();

    let events = run_stream_json_case(
        &workspace,
        server.base_url().as_str(),
        "write_file_denied",
        Some("write_file"),
    );

    assert_all_events_are_versioned(&events);
    let permission_denial = events
        .iter()
        .find(|event| event["type"] == "permission_denial")
        .expect("permission_denial event should be present");
    assert_eq!(permission_denial["tool"], "write_file");
    assert_non_empty_string(&permission_denial["reason"]);

    let recovery_suggestion = events
        .iter()
        .find(|event| event["type"] == "recovery_suggestion")
        .expect("recovery_suggestion event should be present");
    assert_eq!(recovery_suggestion["source_event"], "permission_denial");
    assert_eq!(recovery_suggestion["failure_class"], "trust_gate");
    assert_eq!(recovery_suggestion["tool"], "write_file");
    assert_eq!(
        recovery_suggestion["action"],
        "retry_with_danger_full_access"
    );
    assert_non_empty_string(&recovery_suggestion["reason"]);
    assert_non_empty_string(&recovery_suggestion["suggestion"]);
}

#[test]
fn stream_json_tool_result_error_recovery_suggestion_matches_contract() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let workspace = HarnessWorkspace::new(unique_temp_dir("stream-json-tool-error"));
    workspace.create();

    let events = run_stream_json_case(
        &workspace,
        server.base_url().as_str(),
        "read_file_roundtrip",
        Some("read_file"),
    );

    assert_all_events_are_versioned(&events);
    let tool_result = events
        .iter()
        .find(|event| event["type"] == "tool_result" && event["is_error"] == true)
        .expect("error tool_result event should be present");
    assert_eq!(tool_result["name"], "read_file");

    let recovery_suggestion = events
        .iter()
        .find(|event| event["type"] == "recovery_suggestion")
        .expect("recovery_suggestion event should be present");
    assert_eq!(recovery_suggestion["source_event"], "tool_result");
    assert_eq!(recovery_suggestion["failure_class"], "tool_runtime");
    assert_eq!(recovery_suggestion["tool"], "read_file");
    assert_eq!(recovery_suggestion["action"], "review_tool_error");
    assert_non_empty_string(&recovery_suggestion["reason"]);
    assert_non_empty_string(&recovery_suggestion["suggestion"]);
}

#[test]
fn stream_json_reasoning_step_events_match_contract() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let workspace = HarnessWorkspace::new(unique_temp_dir("stream-json-reasoning"));
    workspace.create();

    let events = run_stream_json_case(
        &workspace,
        server.base_url().as_str(),
        "reasoning_step",
        None,
    );

    assert_all_events_are_versioned(&events);
    let reasoning = events
        .iter()
        .find(|event| event["type"] == "reasoning_step")
        .expect("reasoning_step event should be present");
    assert_eq!(reasoning["reasoning_step"]["step_type"], "analysis");
    assert_eq!(
        reasoning["reasoning_step"]["content"],
        "check stream contract"
    );
    assert_eq!(
        reasoning["reasoning_step"]["signature"],
        "sig_reasoning_contract"
    );
    assert!(events.iter().any(|event| event["type"] == "text_delta"));
    assert!(events.iter().any(|event| event["type"] == "done"));
}

#[test]
fn stream_json_redacted_thinking_events_match_contract() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let workspace = HarnessWorkspace::new(unique_temp_dir("stream-json-redacted-thinking"));
    workspace.create();

    let events = run_stream_json_case(
        &workspace,
        server.base_url().as_str(),
        "redacted_thinking",
        None,
    );

    assert_all_events_are_versioned(&events);
    let reasoning = events
        .iter()
        .find(|event| event["type"] == "reasoning_step")
        .expect("reasoning_step event should be present");
    assert_eq!(
        reasoning["reasoning_step"]["step_type"],
        "redacted_thinking"
    );
    assert_eq!(
        reasoning["reasoning_step"]["data"]["opaque"],
        "redacted-contract"
    );
    assert!(events.iter().any(|event| event["type"] == "text_delta"));
    assert!(events.iter().any(|event| event["type"] == "done"));
}
#[test]
fn stream_json_decisioning_events_match_contract() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let workspace = HarnessWorkspace::new(unique_temp_dir("stream-json-decisioning"));
    workspace.create();
    fs::write(workspace.root.join("fixture.txt"), "alpha parity line\n")
        .expect("fixture should write");
    fs::write(
        workspace.config_home.join("settings.json"),
        r#"{"decisioning":{"enabled":true,"emitEvents":true,"maxParallelism":2}}"#,
    )
    .expect("decisioning settings should write");

    let events = run_stream_json_case(
        &workspace,
        server.base_url().as_str(),
        "read_file_roundtrip",
        Some("read_file"),
    );

    assert_all_events_are_versioned(&events);
    let decisioning_events = events
        .iter()
        .filter(|event| event["type"] == "decisioning_event")
        .collect::<Vec<_>>();
    assert!(
        decisioning_events.len() >= 4,
        "expected decisioning event set: {events:?}"
    );

    let tool_selection = decisioning_events
        .iter()
        .find(|event| event["decisioning_event"]["kind"] == "tool_selection")
        .expect("tool_selection decisioning event should be present");
    assert_eq!(
        tool_selection["decisioning_event"]["selected_tools"][0],
        "read_file"
    );
    assert!(tool_selection["decisioning_event"]["tool_scores"].is_array());

    let decomposition = decisioning_events
        .iter()
        .find(|event| event["decisioning_event"]["kind"] == "task_decomposition")
        .expect("task_decomposition decisioning event should be present");
    assert_eq!(
        decomposition["decisioning_event"]["plan_tree"]["kind"],
        "task"
    );

    let safety = decisioning_events
        .iter()
        .find(|event| event["decisioning_event"]["kind"] == "safety_assessment")
        .expect("safety_assessment decisioning event should be present");
    assert!(safety["decisioning_event"]["risk_score"].is_number());
    assert_non_empty_string(&safety["decisioning_event"]["risk_level"]);
    assert_non_empty_string(&safety["decisioning_event"]["action"]);

    let plan_events = events
        .iter()
        .filter(|event| event["type"] == "plan_execution_event")
        .collect::<Vec<_>>();
    assert!(
        plan_events.len() >= 3,
        "expected plan execution lifecycle events: {events:?}"
    );
    assert!(plan_events.iter().any(|event| {
        event["plan_execution_event"]["kind"] == "node_ready"
            && event["plan_execution_event"]["status"] == "ready"
    }));
    assert!(plan_events.iter().any(|event| {
        event["plan_execution_event"]["kind"] == "node_started"
            && event["plan_execution_event"]["status"] == "running"
    }));
    assert!(plan_events.iter().any(|event| {
        event["plan_execution_event"]["kind"] == "node_succeeded"
            && event["plan_execution_event"]["status"] == "succeeded"
    }));

    let ledger_events = events
        .iter()
        .filter(|event| event["type"] == "task_ledger_event")
        .collect::<Vec<_>>();
    assert!(
        ledger_events.len() >= 3,
        "expected task ledger lifecycle events: {events:?}"
    );
    assert!(ledger_events.iter().any(|event| {
        event["task_ledger_event"]["event"] == "created"
            && event["task_ledger_event"]["status"] == "created"
    }));
    assert!(ledger_events.iter().any(|event| {
        event["task_ledger_event"]["event"] == "status_changed"
            && event["task_ledger_event"]["status"] == "running"
    }));
    assert!(ledger_events.iter().any(|event| {
        event["task_ledger_event"]["event"] == "status_changed"
            && event["task_ledger_event"]["status"] == "completed"
    }));

    let model_route_events = events
        .iter()
        .filter(|event| event["type"] == "model_route_event")
        .collect::<Vec<_>>();
    assert!(
        model_route_events.len() >= 2,
        "expected model route events: {events:?}"
    );
    assert!(model_route_events.iter().any(|event| {
        event["model_route_event"]["phase"] == "coding"
            && event["model_route_event"]["model"].as_str().is_some()
    }));
    assert!(model_route_events.iter().any(|event| {
        event["model_route_event"]["phase"] == "verification"
            && event["model_route_event"]["model"].as_str().is_some()
    }));

    let team_events = events
        .iter()
        .filter(|event| event["type"] == "team_execution_event")
        .collect::<Vec<_>>();
    assert!(
        team_events.iter().any(|event| {
            event["team_execution_event"]["role"] == "verifier"
                && event["team_execution_event"]["kind"] == "verification_passed"
        }),
        "expected verifier team execution event: {events:?}"
    );
}

#[test]
fn stream_json_packet_verification_failure_emits_recovery_event() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let workspace = HarnessWorkspace::new(unique_temp_dir("stream-json-verification-failure"));
    workspace.create();
    let packet = serde_json::json!({
        "objective": "Verify failed task",
        "scope": "runtime verification",
        "repo": "Himalaya",
        "branch_policy": "no branch changes",
        "acceptance_tests": ["rustc --definitely-not-a-real-flag"],
        "commit_policy": "no commit",
        "reporting_contract": "report verification result",
        "escalation_policy": "manual"
    });
    let prompt = format!("{SCENARIO_PREFIX}streaming_text\n{packet}");

    let events = run_stream_json_prompt(
        &workspace,
        server.base_url().as_str(),
        &prompt,
        "danger-full-access",
        None,
    );

    assert_all_events_are_versioned(&events);
    assert!(events
        .iter()
        .any(|event| { event["task_ledger_event"]["event"] == "verification_recorded" }));
    assert!(events
        .iter()
        .any(|event| { event["team_execution_event"]["kind"] == "verification_failed" }));
    assert!(events.iter().any(|event| {
        event["recovery_event"]["recovery_attempted"]["scenario"] == "compile_red_cross_crate"
    }));
    assert!(events
        .iter()
        .any(|event| { event["task_ledger_event"]["event"] == "recovery_recorded" }));
    assert!(events
        .iter()
        .any(|event| { event["task_ledger_event"]["event"] == "route_feedback_recorded" }));
    assert!(events
        .iter()
        .any(|event| { event["model_route_event"]["confidence"].as_f64().is_some() }));
    assert!(events.iter().any(|event| {
        event["model_route_event"]["fallback_model"]
            .as_str()
            .is_some()
    }));
    assert!(events.iter().any(|event| {
        event["task_ledger_event"]["status"] == "blocked"
            || event["task_ledger_event"]["status"] == "failed"
    }));
    let route_feedback_path = workspace.root.join(".Himalaya/routes/feedback.json");
    assert!(route_feedback_path.exists());
    let route_feedback: Value = serde_json::from_str(
        &fs::read_to_string(&route_feedback_path).expect("route feedback should read"),
    )
    .expect("route feedback should parse");
    assert!(route_feedback["feedback"]
        .as_array()
        .expect("feedback array")
        .iter()
        .any(|entry| entry["route"]["phase"] == "verification"
            && entry["succeeded"] == false
            && entry["recovery_triggered"] == true));

    let replay_events = run_stream_json_case(
        &workspace,
        server.base_url().as_str(),
        "streaming_text",
        None,
    );
    assert_all_events_are_versioned(&replay_events);
    assert!(replay_events.iter().any(|event| {
        event["model_route_event"]["phase"] == "verification"
            && event["model_route_event"]["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("route feedback"))
    }));
}

fn run_stream_json_case(
    workspace: &HarnessWorkspace,
    base_url: &str,
    scenario: &str,
    allowed_tools: Option<&str>,
) -> Vec<Value> {
    let prompt = format!("{SCENARIO_PREFIX}{scenario}");
    run_stream_json_prompt(workspace, base_url, &prompt, "read-only", allowed_tools)
}

fn run_stream_json_prompt(
    workspace: &HarnessWorkspace,
    base_url: &str,
    prompt: &str,
    permission_mode: &str,
    allowed_tools: Option<&str>,
) -> Vec<Value> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_Himalaya"));
    command
        .current_dir(&workspace.root)
        .env_clear()
        .env("ANTHROPIC_API_KEY", "test-stream-json-key")
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("Himalaya_CONFIG_HOME", &workspace.config_home)
        .env("HOME", &workspace.home)
        .env("NO_COLOR", "1")
        .env("PATH", "/usr/bin:/bin")
        .args([
            "--model",
            "sonnet",
            "--permission-mode",
            permission_mode,
            "--output-format",
            "stream-json",
        ]);

    if let Some(allowed_tools) = allowed_tools {
        command.args(["--allowedTools", allowed_tools]);
    }

    command.arg(prompt);
    let output = command.output().expect("Himalaya should launch");
    assert_success(&output);
    parse_stream_json_stdout(&output.stdout)
}

fn parse_stream_json_stdout(stdout: &[u8]) -> Vec<Value> {
    let text = String::from_utf8_lossy(stdout);
    let events = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let value = serde_json::from_str::<Value>(line).expect("stdout line should be JSON");
            assert!(
                value.is_object(),
                "stdout line should be a JSON object event: {line}"
            );
            value
        })
        .collect::<Vec<_>>();
    assert!(!events.is_empty(), "stdout should contain stream events");
    events
}

fn assert_all_events_are_versioned(events: &[Value]) {
    for event in events {
        assert!(
            event["type"].as_str().is_some(),
            "event should have string type: {event:?}"
        );
        assert_eq!(
            event["protocol_version"],
            Value::from(1),
            "event should be protocol v1: {event:?}"
        );
        assert_stream_event_schema(event);
    }
}

fn assert_stream_event_schema(event: &Value) {
    match event["type"]
        .as_str()
        .expect("event type should be present")
    {
        "session_meta" => {
            assert_non_empty_string(&event["session_id"]);
            assert_non_empty_string(&event["session_path"]);
            assert_non_empty_string(&event["model"]);
        }
        "message_start" | "message_stop" => {}
        "text_delta" => assert!(
            event["text"].as_str().is_some(),
            "text_delta requires text: {event:?}"
        ),
        "tool_use" => {
            assert_non_empty_string(&event["id"]);
            assert_non_empty_string(&event["name"]);
            assert!(
                event.get("input").is_some(),
                "tool_use requires input: {event:?}"
            );
        }
        "tool_result" => {
            assert_non_empty_string(&event["name"]);
            assert!(
                event["output"].as_str().is_some(),
                "tool_result requires string output: {event:?}"
            );
            assert!(
                event["is_error"].as_bool().is_some(),
                "tool_result requires boolean is_error: {event:?}"
            );
        }
        "permission_request" => {
            assert_non_empty_string(&event["tool"]);
            assert!(
                event.get("input").is_some(),
                "permission_request requires input: {event:?}"
            );
            assert_non_empty_string(&event["current_mode"]);
            assert_non_empty_string(&event["required_mode"]);
            assert!(
                event["reason"].as_str().is_some(),
                "permission_request requires reason: {event:?}"
            );
        }
        "permission_denial" => {
            assert_non_empty_string(&event["tool"]);
            assert_non_empty_string(&event["reason"]);
        }
        "recovery_suggestion" => {
            for field in [
                "source_event",
                "failure_class",
                "tool",
                "reason",
                "action",
                "suggestion",
            ] {
                assert_non_empty_string(&event[field]);
            }
        }
        "reasoning_step" => {
            let step = &event["reasoning_step"];
            assert!(
                step.is_object(),
                "reasoning_step requires object payload: {event:?}"
            );
            assert_non_empty_string(&step["step_type"]);
            if step["step_type"] == "redacted_thinking" {
                assert!(
                    step.get("data").is_some(),
                    "redacted_thinking requires data: {event:?}"
                );
            }
        }
        "decisioning_event" => {
            let decision = &event["decisioning_event"];
            assert!(
                decision.is_object(),
                "decisioning_event requires object payload: {event:?}"
            );
            assert_non_empty_string(&decision["kind"]);
            assert_non_empty_string(&decision["title"]);
            assert_non_empty_string(&decision["summary"]);
        }
        "plan_execution_event" => {
            let plan = &event["plan_execution_event"];
            assert!(
                plan.is_object(),
                "plan_execution_event requires object payload: {event:?}"
            );
            assert!(
                plan["seq"].as_u64().is_some(),
                "plan_execution_event requires numeric seq: {event:?}"
            );
            assert_non_empty_string(&plan["task_id"]);
            assert_non_empty_string(&plan["node_id"]);
            assert_non_empty_string(&plan["kind"]);
            assert_non_empty_string(&plan["status"]);
        }
        "task_ledger_event" => {
            let ledger = &event["task_ledger_event"];
            assert!(
                ledger.is_object(),
                "task_ledger_event requires object payload: {event:?}"
            );
            assert!(
                ledger["seq"].as_u64().is_some(),
                "task_ledger_event requires numeric seq: {event:?}"
            );
            assert_non_empty_string(&ledger["task_id"]);
            assert_non_empty_string(&ledger["event"]);
            assert_non_empty_string(&ledger["status"]);
        }
        "model_route_event" => {
            let route = &event["model_route_event"];
            assert!(
                route.is_object(),
                "model_route_event requires object payload: {event:?}"
            );
            assert_non_empty_string(&route["phase"]);
            assert_non_empty_string(&route["model"]);
            assert_non_empty_string(&route["reason"]);
        }
        "team_execution_event" => {
            let team = &event["team_execution_event"];
            assert!(
                team.is_object(),
                "team_execution_event requires object payload: {event:?}"
            );
            assert!(
                team["seq"].as_u64().is_some(),
                "team_execution_event requires numeric seq: {event:?}"
            );
            assert_non_empty_string(&team["team_id"]);
            assert_non_empty_string(&team["task_id"]);
            assert_non_empty_string(&team["role"]);
            assert_non_empty_string(&team["kind"]);
        }
        "recovery_event" => {
            assert!(
                event["recovery_event"].is_object() || event["recovery_event"].as_str().is_some(),
                "recovery_event requires string or object payload: {event:?}"
            );
        }
        "recovery_action_event" => {
            let recovery = &event["recovery_action_event"];
            assert!(
                recovery.is_object(),
                "recovery_action_event requires object payload: {event:?}"
            );
            assert_non_empty_string(&recovery["task_id"]);
            assert!(
                recovery["results"].is_array(),
                "recovery_action_event requires results array: {event:?}"
            );
        }
        "task_execution_event" => {
            let outcome = &event["task_execution_event"];
            assert!(
                outcome.is_object(),
                "task_execution_event requires object payload: {event:?}"
            );
            assert_task_execution_outcome_schema(outcome);
        }
        "task_execution" => assert_task_execution_outcome_schema(&event["outcome"]),
        "task_recovery" => {
            assert!(
                event["execution"].is_object(),
                "task_recovery requires execution object: {event:?}"
            );
        }
        "task_verification" => {
            assert!(
                event["result"].is_object(),
                "task_verification requires result object: {event:?}"
            );
        }
        "task_list" => assert!(
            event["tasks"].is_array(),
            "task_list requires tasks array: {event:?}"
        ),
        "task_show" => {
            assert!(
                event["task"].is_object(),
                "task_show requires task object: {event:?}"
            );
            assert!(
                event["ledger"].is_array(),
                "task_show requires ledger array: {event:?}"
            );
        }
        "task_node_retry" => {
            assert!(
                event["task"].is_object(),
                "task_node_retry requires task object: {event:?}"
            );
            assert_non_empty_string(&event["node_id"]);
        }
        "task_node_verification" => {
            assert!(
                event["task"].is_object(),
                "task_node_verification requires task object: {event:?}"
            );
            assert_non_empty_string(&event["node_id"]);
            assert_non_empty_string(&event["command"]);
        }
        "task_compacted" => {
            assert!(
                event["task"].is_object(),
                "task_compacted requires task object: {event:?}"
            );
            assert!(
                event["keep_last"].as_u64().is_some(),
                "task_compacted requires numeric keep_last: {event:?}"
            );
        }
        "task_cancelled" => assert!(
            event["task"].is_object(),
            "task_cancelled requires task object: {event:?}"
        ),
        "task_packet_create" | "task_packet_run" | "task_packet_status" => {
            assert!(
                event["task"].is_object(),
                "task packet event requires task object: {event:?}"
            );
            assert!(
                event["ledger"].is_array(),
                "task packet event requires ledger array: {event:?}"
            );
            assert!(
                event["verification_handoff"].is_object(),
                "task packet event requires verification_handoff object: {event:?}"
            );
        }
        "task_scheduler_tick" => assert!(
            event["tick"].is_object(),
            "task_scheduler_tick requires tick object: {event:?}"
        ),
        "task_scheduler_queue" => assert!(
            event["queue"].is_array(),
            "task_scheduler_queue requires queue array: {event:?}"
        ),
        "benchmark_suite" => {
            assert_non_empty_string(&event["suite_id"]);
            assert_non_empty_string(&event["version"]);
            assert!(
                event["tasks"].is_array(),
                "benchmark_suite requires tasks array: {event:?}"
            );
        }
        "benchmark_task" => assert!(
            event["task"].is_object(),
            "benchmark_task requires task object: {event:?}"
        ),
        "benchmark_run" => assert!(
            event["run"].is_object(),
            "benchmark_run requires run object: {event:?}"
        ),
        "worker_list" => assert!(
            event["workers"].is_array(),
            "worker_list requires workers array: {event:?}"
        ),
        "worker_create"
        | "worker_observe"
        | "worker_resolve_trust"
        | "worker_prompt"
        | "worker_restart"
        | "worker_terminate" => assert!(
            event["worker"].is_object(),
            "worker event requires worker object: {event:?}"
        ),
        "worker_ready" => assert!(
            event["ready"].is_object(),
            "worker_ready requires ready object: {event:?}"
        ),
        "worker_supervisor_tick" => assert!(
            event["tick"].is_object(),
            "worker_supervisor_tick requires tick object: {event:?}"
        ),
        "done" => assert!(
            event["iterations"].as_u64().is_some(),
            "done requires numeric iterations: {event:?}"
        ),
        "error" => assert_non_empty_string(&event["error"]),
        other => panic!("unknown stream-json event type in contract test: {other}"),
    }
}

fn assert_task_execution_outcome_schema(value: &Value) {
    assert!(
        value.is_object(),
        "task execution outcome requires object payload: {value:?}"
    );
    assert_non_empty_string(&value["task_id"]);
    assert!(
        value["steps"].is_array(),
        "task execution outcome requires steps array: {value:?}"
    );
    assert!(
        value["completed"].as_bool().is_some(),
        "task execution outcome requires completed bool: {value:?}"
    );
    assert!(
        value["blocked"].as_bool().is_some(),
        "task execution outcome requires blocked bool: {value:?}"
    );
    assert_non_empty_string(&value["message"]);
}

fn assert_non_empty_string(value: &Value) {
    assert!(
        value.as_str().is_some_and(|value| !value.is_empty()),
        "value should be a non-empty string: {value:?}"
    );
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout:\n{}\n\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct HarnessWorkspace {
    root: PathBuf,
    config_home: PathBuf,
    home: PathBuf,
}

impl HarnessWorkspace {
    fn new(root: PathBuf) -> Self {
        Self {
            config_home: root.join("config-home"),
            home: root.join("home"),
            root,
        }
    }

    fn create(&self) {
        fs::create_dir_all(&self.root).expect("workspace should exist");
        fs::create_dir_all(&self.config_home).expect("config home should exist");
        fs::create_dir_all(&self.home).expect("home should exist");
    }
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after epoch")
        .as_millis();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "Himalaya-stream-json-{label}-{}-{millis}-{counter}",
        std::process::id()
    ))
}
