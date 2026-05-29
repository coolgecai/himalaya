use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::{green_contract::GreenLevel, VerificationRequest, VerificationResult};

const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationCommandResult {
    pub command: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone)]
pub struct VerificationRunner {
    workspace_root: Option<PathBuf>,
    command_timeout: Duration,
}

impl VerificationRunner {
    #[must_use]
    pub fn new(workspace_root: Option<PathBuf>) -> Self {
        Self {
            workspace_root,
            command_timeout: DEFAULT_COMMAND_TIMEOUT,
        }
    }

    #[must_use]
    pub fn with_command_timeout(mut self, timeout: Duration) -> Self {
        self.command_timeout = timeout;
        self
    }

    pub fn run(&self, request: &VerificationRequest) -> VerificationResult {
        if request.acceptance_tests.is_empty() {
            return VerificationResult {
                task_id: request.task_id.clone(),
                passed: false,
                observed_green_level: None,
                summary:
                    "external verification required; no executable acceptance tests were provided"
                        .to_string(),
                evidence: vec![request.reporting_contract.clone()],
            };
        }

        let mut command_results = Vec::new();
        for command in &request.acceptance_tests {
            match self.run_command(command) {
                Ok(result) => {
                    let passed = result.exit_code == Some(0);
                    command_results.push(result);
                    if !passed {
                        return self.failed_result(request, &command_results);
                    }
                }
                Err(reason) => {
                    command_results.push(VerificationCommandResult {
                        command: command.clone(),
                        exit_code: None,
                        stdout: String::new(),
                        stderr: reason,
                    });
                    return self.failed_result(request, &command_results);
                }
            }
        }

        VerificationResult {
            task_id: request.task_id.clone(),
            passed: true,
            observed_green_level: request
                .required_green_level
                .or(Some(GreenLevel::TargetedTests)),
            summary: format!(
                "verification passed: {} acceptance test(s)",
                command_results.len()
            ),
            evidence: command_results
                .into_iter()
                .map(format_command_result)
                .collect(),
        }
    }

    fn run_command(&self, command: &str) -> Result<VerificationCommandResult, String> {
        let argv = split_command(command)?;
        let Some((program, args)) = argv.split_first() else {
            return Err("verification command is empty".to_string());
        };
        if !allowed_command(program, args) {
            return Err(format!(
                "verification command program is not allowed: {program}"
            ));
        }

        let mut child = Command::new(program);
        child.args(args);
        if let Some(root) = self.workspace_root.as_deref() {
            if root.exists() {
                child.current_dir(root);
            }
        }
        child.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = child
            .spawn()
            .map_err(|error| format!("failed to run verification command `{command}`: {error}"))?;
        let deadline = Instant::now() + self.command_timeout;
        loop {
            if let Some(status) = child.try_wait().map_err(|error| {
                format!("failed to poll verification command `{command}`: {error}")
            })? {
                let output = child.wait_with_output().map_err(|error| {
                    format!("failed to collect verification command `{command}` output: {error}")
                })?;
                return Ok(VerificationCommandResult {
                    command: command.to_string(),
                    exit_code: status.code(),
                    stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                    stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                });
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "verification command `{command}` timed out after {} seconds",
                    self.command_timeout.as_secs()
                ));
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn failed_result(
        &self,
        request: &VerificationRequest,
        command_results: &[VerificationCommandResult],
    ) -> VerificationResult {
        let summary = command_results.last().map_or_else(
            || "verification failed".to_string(),
            summarize_failed_command,
        );
        VerificationResult {
            task_id: request.task_id.clone(),
            passed: false,
            observed_green_level: None,
            summary,
            evidence: command_results
                .iter()
                .cloned()
                .map(format_command_result)
                .collect(),
        }
    }

    #[must_use]
    pub fn workspace_root(&self) -> Option<&Path> {
        self.workspace_root.as_deref()
    }

    #[must_use]
    pub fn command_timeout(&self) -> Duration {
        self.command_timeout
    }
}

fn summarize_failed_command(result: &VerificationCommandResult) -> String {
    let detail = first_non_empty_line(&result.stderr)
        .or_else(|| first_non_empty_line(&result.stdout))
        .unwrap_or("no output");
    match result.exit_code {
        Some(code) => format!(
            "verification command `{}` failed with exit code {code}: {detail}",
            result.command
        ),
        None => format!("verification command `{}` failed: {detail}", result.command),
    }
}

fn first_non_empty_line(value: &str) -> Option<&str> {
    value.lines().map(str::trim).find(|line| !line.is_empty())
}

fn format_command_result(result: VerificationCommandResult) -> String {
    match serde_json::to_string(&result) {
        Ok(serialized) => serialized,
        Err(_) => result.command,
    }
}

fn split_command(command: &str) -> Result<Vec<String>, String> {
    let mut argv = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for ch in command.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        match quote {
            Some(active) if ch == active => quote = None,
            Some(_) => current.push(ch),
            None if ch == '\'' || ch == '"' => quote = Some(ch),
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    argv.push(std::mem::take(&mut current));
                }
            }
            None if matches!(ch, '|' | '&' | ';' | '<' | '>' | '`' | '$') => {
                return Err(format!(
                    "verification command contains unsupported shell syntax: {ch}"
                ));
            }
            None => current.push(ch),
        }
    }

    if escaped {
        return Err("verification command ends with an escape character".to_string());
    }
    if quote.is_some() {
        return Err("verification command contains an unterminated quote".to_string());
    }
    if !current.is_empty() {
        argv.push(current);
    }
    Ok(argv)
}

fn allowed_command(program: &str, args: &[String]) -> bool {
    match program {
        "cargo" | "npm" | "node" | "python" | "python3" | "deno" | "bun" | "go" | "rustc" => true,
        "bash" | "sh" => args.first().is_some_and(|arg| arg.ends_with(".sh")),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(commands: Vec<&str>) -> VerificationRequest {
        VerificationRequest {
            task_id: "task-1".to_string(),
            objective: "verify".to_string(),
            scope: "runtime".to_string(),
            acceptance_tests: commands.into_iter().map(str::to_string).collect(),
            reporting_contract: "report".to_string(),
            policy: crate::VerificationPolicy::Targeted,
            required_green_level: Some(GreenLevel::TargetedTests),
        }
    }

    #[test]
    fn rejects_shell_control_syntax() {
        let runner = VerificationRunner::new(None);
        let result = runner.run(&request(vec!["cargo test; rm -rf target"]));

        assert!(!result.passed);
        assert!(result.summary.contains("unsupported shell syntax"));
    }

    #[test]
    fn rejects_unapproved_programs() {
        let runner = VerificationRunner::new(None);
        let result = runner.run(&request(vec!["curl https://example.invalid"]));

        assert!(!result.passed);
        assert!(result.summary.contains("not allowed"));
    }

    #[test]
    fn parses_quoted_args() {
        assert_eq!(
            split_command("cargo test -p runtime 'some test'").expect("command should parse"),
            vec!["cargo", "test", "-p", "runtime", "some test"]
        );
    }
}
