use std::collections::BTreeSet;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use mock_anthropic_service::{MockAnthropicService, SCENARIO_PREFIX};
use serde_json::{json, Value};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn known_stream_event_types() -> BTreeSet<String> {
    [
        "text_delta",
        "tool_use",
        "tool_result",
        "done",
        "session_meta",
        "message_start",
        "message_stop",
        "command_match",
        "tool_match",
        "permission_denial",
        "permission_request",
        "reasoning_step",
        "decisioning_event",
        "plan_execution_event",
        "task_ledger_event",
        "model_route_event",
        "team_execution_event",
        "recovery_event",
        "recovery_action_event",
        "task_execution_event",
        "local_command",
        "recovery_suggestion",
        "task_list",
        "task_show",
        "task_execution",
        "task_recovery",
        "task_verification",
        "task_node_retry",
        "task_node_verification",
        "task_compacted",
        "task_cancelled",
        "task_packet_create",
        "task_packet_run",
        "task_packet_status",
        "task_scheduler_tick",
        "task_scheduler_queue",
        "task_scheduler_daemon_run",
        "task_scheduler_daemon_status",
        "task_scheduler_daemon_logs",
        "route_feedback_summary",
        "benchmark_suite",
        "benchmark_task",
        "benchmark_run",
        "worker_list",
        "worker_create",
        "worker_spawn",
        "worker_probe",
        "worker_observe",
        "worker_ready",
        "worker_resolve_trust",
        "worker_prompt",
        "worker_complete",
        "worker_restart",
        "worker_terminate",
        "worker_supervisor_tick",
        "error",
        "context_event",
        "user_question",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

#[test]
fn stream_json_schema_covers_known_event_types() {
    let schema_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("protocol/stream-json-v1.schema.json");
    let schema: Value = serde_json::from_str(
        &fs::read_to_string(&schema_path).expect("stream-json schema should be readable"),
    )
    .expect("stream-json schema should parse");

    assert_eq!(schema["properties"]["protocol_version"]["const"], 1);
    let root_required = schema["required"]
        .as_array()
        .expect("schema root required fields should exist")
        .iter()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    assert!(root_required.contains("type"));
    assert!(root_required.contains("protocol_version"));

    let base_required = schema["$defs"]["baseEvent"]["required"]
        .as_array()
        .expect("base event required fields should exist")
        .iter()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    assert!(base_required.contains("type"));
    assert!(base_required.contains("protocol_version"));

    let schema_event_types = schema["$defs"]["eventType"]["enum"]
        .as_array()
        .expect("schema eventType enum should exist")
        .iter()
        .map(|value| {
            value
                .as_str()
                .expect("event type should be string")
                .to_string()
        })
        .collect::<BTreeSet<_>>();
    let expected_event_types = known_stream_event_types();
    assert_eq!(schema_event_types, expected_event_types);

    let branch_types = schema["allOf"][0]["oneOf"]
        .as_array()
        .expect("schema should define event branches")
        .iter()
        .flat_map(|branch| {
            let ref_name = branch["$ref"]
                .as_str()
                .expect("branch should be a definition ref")
                .trim_start_matches("#/$defs/");
            let type_schema = &schema["$defs"][ref_name]["allOf"][1]["properties"]["type"];
            if let Some(value) = type_schema["const"].as_str() {
                return vec![value.to_string()];
            }
            type_schema["enum"]
                .as_array()
                .expect("branch type should use const or enum")
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .expect("branch enum type should be string")
                        .to_string()
                })
                .collect::<Vec<_>>()
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(branch_types, expected_event_types);
}

#[test]
fn golden_stream_json_transcript_matches_contract() {
    let golden_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("protocol/stream-json-v1.golden.ndjson");
    let events = parse_stream_json_stdout(
        fs::read(&golden_path)
            .expect("golden stream transcript should be readable")
            .as_slice(),
    );

    assert_all_events_are_versioned(&events);

    let event_types = events
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(event_types.first(), Some(&"session_meta"));
    assert_eq!(event_types.last(), Some(&"done"));
    for required in [
        "reasoning_step",
        "tool_use",
        "tool_result",
        "decisioning_event",
        "plan_execution_event",
        "task_ledger_event",
        "model_route_event",
        "team_execution_event",
        "recovery_event",
        "recovery_action_event",
        "task_execution_event",
        "task_scheduler_daemon_logs",
        "route_feedback_summary",
        "benchmark_run",
        "worker_spawn",
        "worker_supervisor_tick",
        "permission_request",
        "recovery_suggestion",
    ] {
        assert!(
            event_types.contains(&required),
            "golden transcript should include {required}"
        );
    }
}

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
fn stream_json_mcp_stdio_lifecycle_tool_roundtrip_matches_contract() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let workspace = HarnessWorkspace::new(unique_temp_dir("stream-json-mcp-lifecycle"));
    workspace.create();
    let fixture_path = workspace.root.join("fixture-mcp.py");
    write_mcp_server_fixture(&fixture_path);
    write_mcp_settings(&workspace, &fixture_path);

    let prompt = format!("{SCENARIO_PREFIX}mcp_tool_roundtrip");
    let events = run_stream_json_prompt(
        &workspace,
        server.base_url().as_str(),
        &prompt,
        "workspace-write",
        Some("mcp__alpha__echo"),
    );

    assert_all_events_are_versioned(&events);
    let tool_use = events
        .iter()
        .find(|event| event["type"] == "tool_use")
        .expect("mcp tool_use event should be present");
    assert_eq!(tool_use["name"], "mcp__alpha__echo");
    assert_eq!(tool_use["input"]["text"], "hello from mcp lifecycle");

    let tool_result = events
        .iter()
        .find(|event| event["type"] == "tool_result")
        .expect("mcp tool_result event should be present");
    assert_eq!(tool_result["name"], "mcp__alpha__echo");
    assert_eq!(tool_result["is_error"], false);
    let output = tool_result["output"]
        .as_str()
        .expect("output should be text");
    assert!(output.contains("structuredContent"), "output: {output}");
    assert!(
        output.contains("hello from mcp lifecycle"),
        "output: {output}"
    );

    assert!(events.iter().any(|event| {
        event["type"] == "text_delta"
            && event["text"]
                .as_str()
                .is_some_and(|text| text.contains("mcp tool completed: hello from mcp lifecycle"))
    }));
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

#[test]
fn stream_json_model_routing_config_switches_after_feedback() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let workspace = HarnessWorkspace::new(unique_temp_dir("stream-json-adaptive-routing"));
    workspace.create();
    fs::write(
        workspace.config_home.join("settings.json"),
        serde_json::to_string(&json!({
            "modelRouting": {
                "enabled": true,
                "minFeedbackSamples": 1,
                "switchFailureThresholdPercent": 50,
                "routes": [
                    {
                        "phase": "verification",
                        "model": "Himalaya-opus-4-6",
                        "provider": "anthropic",
                        "capabilities": ["verification"],
                        "qualityWeight": 4
                    }
                ]
            }
        }))
        .expect("settings should serialize"),
    )
    .expect("routing settings should write");
    fs::create_dir_all(workspace.root.join(".Himalaya/routes"))
        .expect("route feedback dir should exist");
    fs::write(
        workspace.root.join(".Himalaya/routes/feedback.json"),
        serde_json::to_string(&json!({
            "feedback": [
                {
                    "task_id": "task-1",
                    "route": {
                        "phase": "verification",
                        "model": "Himalaya-sonnet-4-6",
                        "provider": null,
                        "reason": "test",
                        "confidence": 0.8,
                        "fallback_model": "Himalaya-sonnet-4-6"
                    },
                    "succeeded": false,
                    "latency_ms": null,
                    "verification_passed": false,
                    "recovery_triggered": true,
                    "timestamp": 1,
                    "note": "failed"
                }
            ]
        }))
        .expect("feedback should serialize"),
    )
    .expect("route feedback should write");

    let events = run_stream_json_case(
        &workspace,
        server.base_url().as_str(),
        "streaming_text",
        None,
    );

    assert_all_events_are_versioned(&events);
    assert!(events.iter().any(|event| {
        event["model_route_event"]["phase"] == "verification"
            && event["model_route_event"]["model"] == "Himalaya-opus-4-6"
            && event["model_route_event"]["fallback_model"] == "Himalaya-sonnet-4-6"
            && event["model_route_event"]["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("adaptive route selected"))
    }));
}

#[test]
fn repl_error_and_done_events_include_protocol_version() {
    let workspace = HarnessWorkspace::new(unique_temp_dir("stream-json-repl-error"));
    workspace.create();
    let missing_file = workspace.root.join("missing-fixture.txt");

    let mut child = Command::new(env!("CARGO_BIN_EXE_Himalaya"))
        .current_dir(&workspace.root)
        .env_clear()
        .env("ANTHROPIC_API_KEY", "test-stream-json-key")
        .env("Himalaya_CONFIG_HOME", &workspace.config_home)
        .env("HOME", &workspace.home)
        .env("NO_COLOR", "1")
        .env("PATH", "/usr/bin:/bin")
        .args([
            "--model",
            "Himalaya-sonnet-4-6",
            "--permission-mode",
            "read-only",
            "--repl",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Himalaya repl should launch");

    {
        let stdin = child.stdin.as_mut().expect("stdin should be piped");
        writeln!(
            stdin,
            "{}",
            json!({
                "type": "prompt",
                "text": "try to load a missing file",
                "files": [missing_file],
            })
        )
        .expect("prompt command should write");
        writeln!(stdin, "{}", json!({ "type": "exit" })).expect("exit command should write");
    }

    let output = child.wait_with_output().expect("repl should exit");
    assert_success(&output);
    let events = parse_stream_json_stdout(&output.stdout);
    assert_all_events_are_versioned(&events);
    assert!(events.iter().any(|event| event["type"] == "error"));
    assert!(events.iter().any(|event| event["type"] == "done"));
}

#[test]
fn structured_execution_drives_dag_end_to_end() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let workspace = HarnessWorkspace::new(unique_temp_dir("stream-json-structured-e2e"));
    workspace.create();
    // Enable structured execution at a low complexity threshold so the
    // high-capability prompt below engages the DAG pipeline.
    fs::write(
        workspace.config_home.join("settings.json"),
        r#"{"decisioning":{"enabled":true,"emitEvents":true,"structuredExecutionThreshold":3}}"#,
    )
    .expect("settings should write");

    // Scenario prefix selects the phase-aware mock; the rest of the prompt
    // drives high complexity (implement/refactor/write/test/verify).
    let prompt = format!(
        "{SCENARIO_PREFIX}structured_execution_e2e implement, refactor, write, test and verify the billing module"
    );
    let events = run_stream_json_prompt(
        &workspace,
        server.base_url().as_str(),
        &prompt,
        "danger-full-access",
        None,
    );

    assert_all_events_are_versioned(&events);
    // The structured plan produced real plan-execution events (nodes driven
    // through the scheduler), and the turn completed.
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "plan_execution_event"),
        "expected plan_execution_event from DAG dispatch: {events:?}"
    );
    assert!(events.iter().any(|event| event["type"] == "done"));
}

#[test]
fn team_convergence_drives_roles_end_to_end() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let workspace = HarnessWorkspace::new(unique_temp_dir("stream-json-team-e2e"));
    workspace.create();
    // Structured execution on, plus team convergence for high-effort nodes.
    fs::write(
        workspace.config_home.join("settings.json"),
        r#"{"decisioning":{"enabled":true,"emitEvents":true,"structuredExecutionThreshold":3,"teamConvergenceThreshold":4}}"#,
    )
    .expect("settings should write");

    let prompt = format!(
        "{SCENARIO_PREFIX}team_convergence_e2e implement, refactor, write, test and verify the core engine"
    );
    let events = run_stream_json_prompt(
        &workspace,
        server.base_url().as_str(),
        &prompt,
        "danger-full-access",
        None,
    );

    assert_all_events_are_versioned(&events);
    // The multi-role convergence emitted team-execution dialogue events and
    // the turn completed.
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "team_execution_event"),
        "expected team_execution_event from role convergence: {events:?}"
    );
    assert!(events.iter().any(|event| event["type"] == "done"));
}

#[test]
fn cron_run_fires_due_entry_end_to_end() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
    let server = runtime
        .block_on(MockAnthropicService::spawn())
        .expect("mock service should start");
    let workspace = HarnessWorkspace::new(unique_temp_dir("stream-json-cron-run"));
    workspace.create();

    // Helper: run an arbitrary CLI invocation in the workspace against the mock.
    let run_cli = |args: &[&str]| -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_Himalaya"));
        command
            .current_dir(&workspace.root)
            .env_clear()
            .env("ANTHROPIC_API_KEY", "test-stream-json-key")
            .env("ANTHROPIC_BASE_URL", server.base_url().as_str())
            .env("Himalaya_CONFIG_HOME", &workspace.config_home)
            .env("HOME", &workspace.home)
            .env("NO_COLOR", "1")
            .env("PATH", "/usr/bin:/bin")
            .args(args);
        command.output().expect("Himalaya should launch")
    };

    // Seed a cron due every minute whose prompt selects the mock scenario.
    let add = run_cli(&[
        "cron",
        "add",
        "* * * * *",
        &format!("{SCENARIO_PREFIX}streaming_text"),
    ]);
    assert_success(&add);

    // Fire due crons; each fire re-invokes the binary against the mock.
    let run = run_cli(&["cron", "run", "--output-format", "stream-json"]);
    assert_success(&run);
    let events = parse_stream_json_stdout(&run.stdout);
    assert!(
        events.iter().any(|event| event["type"] == "cron_fired"),
        "expected a cron_fired event: {events:?}"
    );

    // The fired entry's run was recorded in the persisted registry.
    let crons_path = workspace
        .root
        .join(".Himalaya")
        .join("cron")
        .join("crons.json");
    let crons: Value =
        serde_json::from_str(&fs::read_to_string(&crons_path).expect("crons.json should exist"))
            .expect("crons.json should parse");
    let run_count = crons["entries"][0]["run_count"].as_u64().unwrap_or(0);
    assert!(
        run_count >= 1,
        "fired cron should have run_count >= 1: {crons}"
    );
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
            "Himalaya-sonnet-4-6",
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
        "task_scheduler_daemon_run" => {
            assert!(
                event["runs"].is_array(),
                "task_scheduler_daemon_run requires runs array: {event:?}"
            );
            assert!(
                event.get("state").is_some(),
                "task_scheduler_daemon_run requires state field: {event:?}"
            );
        }
        "task_scheduler_daemon_status" => {
            assert!(
                event.get("state").is_some(),
                "task_scheduler_daemon_status requires state field: {event:?}"
            );
            assert_non_empty_string(&event["state_path"]);
            assert_non_empty_string(&event["events_path"]);
        }
        "task_scheduler_daemon_logs" => {
            assert!(
                event["events"].is_array(),
                "task_scheduler_daemon_logs requires events array: {event:?}"
            );
            assert_non_empty_string(&event["events_path"]);
        }
        "route_feedback_summary" => {
            assert!(
                event["summaries"].is_array(),
                "route_feedback_summary requires summaries array: {event:?}"
            );
            assert!(
                event["feedback_count"].as_u64().is_some(),
                "route_feedback_summary requires feedback_count: {event:?}"
            );
        }
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
        | "worker_spawn"
        | "worker_probe"
        | "worker_observe"
        | "worker_resolve_trust"
        | "worker_prompt"
        | "worker_complete"
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

fn write_mcp_server_fixture(script_path: &PathBuf) {
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
        "        send_message({'jsonrpc':'2.0','id':request['id'],'result':{'protocolVersion':'2024-11-05','capabilities':{'tools':{},'resources':{}},'serverInfo':{'name':'alpha-fixture','version':'1.0.0'}}})",
        "    elif method == 'tools/list':",
        "        send_message({'jsonrpc':'2.0','id':request['id'],'result':{'tools':[{'name':'echo','description':'Echo text','inputSchema':{'type':'object','properties':{'text':{'type':'string'}},'required':['text']}}]}})",
        "    elif method == 'resources/list':",
        "        send_message({'jsonrpc':'2.0','id':request['id'],'result':{'resources':[]}})",
        "    elif method == 'tools/call':",
        "        args = request['params'].get('arguments') or {}",
        "        text = args.get('text', '')",
        "        send_message({'jsonrpc':'2.0','id':request['id'],'result':{'content':[{'type':'text','text':'echo: ' + text}], 'structuredContent':{'echoed': text}, 'isError': False}})",
        "    else:",
        "        send_message({'jsonrpc':'2.0','id':request.get('id'),'result':{}})",
    ]
    .join("\n");
    fs::write(script_path, script).expect("mcp fixture should write");
}

fn write_mcp_settings(workspace: &HarnessWorkspace, fixture_path: &PathBuf) {
    let settings = json!({
        "mcpServers": {
            "alpha": {
                "command": "python3",
                "args": [fixture_path]
            }
        }
    });
    fs::write(
        workspace.config_home.join("settings.json"),
        serde_json::to_string(&settings).expect("settings should serialize"),
    )
    .expect("mcp settings should write");
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
