#![allow(
    clippy::match_wildcard_for_single_variants,
    clippy::must_use_candidate,
    clippy::uninlined_format_args
)]
//! Permission enforcement layer that gates tool execution based on the
//! active `PermissionPolicy`.

use crate::permissions::{PermissionMode, PermissionOutcome, PermissionPolicy};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome")]
pub enum EnforcementResult {
    /// Tool execution is allowed.
    Allowed,
    /// Tool execution was denied due to insufficient permissions.
    Denied {
        tool: String,
        active_mode: String,
        required_mode: String,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionEnforcer {
    policy: PermissionPolicy,
}

impl PermissionEnforcer {
    #[must_use]
    pub fn new(policy: PermissionPolicy) -> Self {
        Self { policy }
    }

    /// Check whether a tool can be executed under the current permission policy.
    /// When prompting is required but no prompter is provided, auto-denies
    /// rather than silently allowing.
    pub fn check(&self, tool_name: &str, input: &str) -> EnforcementResult {
        // When the active mode is Prompt but no prompter is available, deny.
        if self.policy.active_mode() == PermissionMode::Prompt {
            return EnforcementResult::Denied {
                tool: tool_name.to_owned(),
                active_mode: self.policy.active_mode().as_str().to_owned(),
                required_mode: self.policy.required_mode_for(tool_name).as_str().to_owned(),
                reason: "interactive prompt required but no prompter is available".to_owned(),
            };
        }

        self.evaluate_policy(tool_name, input)
    }

    fn evaluate_policy(&self, tool_name: &str, input: &str) -> EnforcementResult {
        let outcome = self.policy.authorize(tool_name, input, None);

        match outcome {
            PermissionOutcome::Allow => EnforcementResult::Allowed,
            PermissionOutcome::Deny { reason } => {
                let active_mode = self.policy.active_mode();
                let required_mode = self.policy.required_mode_for(tool_name);
                EnforcementResult::Denied {
                    tool: tool_name.to_owned(),
                    active_mode: active_mode.as_str().to_owned(),
                    required_mode: required_mode.as_str().to_owned(),
                    reason,
                }
            }
        }
    }

    #[must_use]
    pub fn is_allowed(&self, tool_name: &str, input: &str) -> bool {
        matches!(self.check(tool_name, input), EnforcementResult::Allowed)
    }

    #[must_use]
    pub fn active_mode(&self) -> PermissionMode {
        self.policy.active_mode()
    }

    /// Classify a file operation against workspace boundaries. Evaluates the
    /// full permission policy first, then applies workspace boundary checks.
    pub fn check_file_write(&self, path: &str, workspace_root: &str) -> EnforcementResult {
        // Always evaluate the full permission policy first so deny/allow/ask
        // rules are honoured for file-write tools.
        let policy_result = self.evaluate_policy("write_file", path);
        if let EnforcementResult::Denied { .. } = &policy_result {
            return policy_result;
        }

        let mode = self.policy.active_mode();
        match mode {
            PermissionMode::ReadOnly => EnforcementResult::Denied {
                tool: "write_file".to_owned(),
                active_mode: mode.as_str().to_owned(),
                required_mode: PermissionMode::WorkspaceWrite.as_str().to_owned(),
                reason: format!("file writes are not allowed in '{}' mode", mode.as_str()),
            },
            PermissionMode::WorkspaceWrite => {
                if is_within_workspace(path, workspace_root) {
                    policy_result
                } else {
                    EnforcementResult::Denied {
                        tool: "write_file".to_owned(),
                        active_mode: mode.as_str().to_owned(),
                        required_mode: PermissionMode::DangerFullAccess.as_str().to_owned(),
                        reason: format!(
                            "path '{}' is outside workspace root '{}'",
                            path, workspace_root
                        ),
                    }
                }
            }
            PermissionMode::Allow | PermissionMode::DangerFullAccess => policy_result,
            PermissionMode::Prompt => EnforcementResult::Denied {
                tool: "write_file".to_owned(),
                active_mode: mode.as_str().to_owned(),
                required_mode: PermissionMode::WorkspaceWrite.as_str().to_owned(),
                reason: "file write requires confirmation in prompt mode".to_owned(),
            },
        }
    }

    /// Check if a bash command should be allowed. Evaluates the full
    /// permission policy first, then applies read-only heuristics.
    pub fn check_bash(&self, command: &str) -> EnforcementResult {
        // Always evaluate the full permission policy first so deny/allow/ask
        // rules are honoured for bash.
        let policy_result = self.evaluate_policy("bash", command);
        if let EnforcementResult::Denied { .. } = &policy_result {
            return policy_result;
        }

        let mode = self.policy.active_mode();
        match mode {
            PermissionMode::ReadOnly => {
                if is_read_only_command(command) {
                    policy_result
                } else {
                    EnforcementResult::Denied {
                        tool: "bash".to_owned(),
                        active_mode: mode.as_str().to_owned(),
                        required_mode: PermissionMode::WorkspaceWrite.as_str().to_owned(),
                        reason: format!(
                            "command may modify state; not allowed in '{}' mode",
                            mode.as_str()
                        ),
                    }
                }
            }
            PermissionMode::Prompt => EnforcementResult::Denied {
                tool: "bash".to_owned(),
                active_mode: mode.as_str().to_owned(),
                required_mode: PermissionMode::DangerFullAccess.as_str().to_owned(),
                reason: "bash requires confirmation in prompt mode".to_owned(),
            },
            _ => policy_result,
        }
    }
}

/// Workspace boundary check with path canonicalization to prevent
/// traversal attacks (e.g. `/workspace/../../etc/passwd`).
fn is_within_workspace(path: &str, workspace_root: &str) -> bool {
    use std::path::Path;

    let root_path = Path::new(workspace_root);
    let resolved_root = root_path
        .canonicalize()
        .unwrap_or_else(|_| root_path.to_path_buf());
    let candidate = Path::new(path);
    let absolute_candidate = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        resolved_root.join(candidate)
    };
    let resolved_path = absolute_candidate.canonicalize().unwrap_or_else(|_| {
        if let Some(parent) = absolute_candidate.parent() {
            let canonical_parent = parent
                .canonicalize()
                .unwrap_or_else(|_| parent.to_path_buf());
            if let Some(name) = absolute_candidate.file_name() {
                return canonical_parent.join(name);
            }
        }
        absolute_candidate
    });

    resolved_path.starts_with(&resolved_root)
}

fn contains_bare_command(command: &str, target: &str) -> bool {
    command
        .split_whitespace()
        .map(|part| part.trim_matches(|ch: char| matches!(ch, '|' | ';' | '&' | '(' | ')')))
        .any(|part| part.rsplit('/').next().unwrap_or(part) == target)
}

fn contains_inline_code_execution(command: &str) -> bool {
    let parts: Vec<&str> = command.split_whitespace().collect();
    for (idx, part) in parts.iter().enumerate() {
        let cmd = part.rsplit('/').next().unwrap_or(part);
        if !matches!(cmd, "python3" | "python" | "node" | "ruby" | "perl" | "php") {
            continue;
        }

        for arg in parts.iter().skip(idx + 1) {
            if *arg == "-c" || *arg == "-e" || arg.starts_with("-c") || arg.starts_with("-e") {
                return true;
            }
        }
    }
    false
}

fn contains_sed_in_place(command: &str) -> bool {
    let parts: Vec<&str> = command.split_whitespace().collect();
    for (idx, part) in parts.iter().enumerate() {
        if part.rsplit('/').next().unwrap_or(part) != "sed" {
            continue;
        }

        for arg in parts.iter().skip(idx + 1) {
            if *arg == "-i" || *arg == "--in-place" || arg.starts_with("-i") {
                return true;
            }
        }
    }
    false
}

/// Conservative heuristic: is this bash command read-only?
fn is_read_only_command(command: &str) -> bool {
    let first_token = command
        .split_whitespace()
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("");

    let is_read_only_binary = matches!(
        first_token,
        "cat"
            | "head"
            | "tail"
            | "less"
            | "more"
            | "wc"
            | "ls"
            | "find"
            | "grep"
            | "rg"
            | "awk"
            | "sed"
            | "echo"
            | "printf"
            | "which"
            | "where"
            | "whoami"
            | "pwd"
            | "env"
            | "printenv"
            | "date"
            | "cal"
            | "df"
            | "du"
            | "free"
            | "uptime"
            | "uname"
            | "file"
            | "stat"
            | "diff"
            | "sort"
            | "uniq"
            | "tr"
            | "cut"
            | "paste"
            | "xargs"
            | "test"
            | "true"
            | "false"
            | "type"
            | "readlink"
            | "realpath"
            | "basename"
            | "dirname"
            | "sha256sum"
            | "md5sum"
            | "b3sum"
            | "xxd"
            | "hexdump"
            | "od"
            | "strings"
            | "tree"
            | "jq"
            | "yq"
            | "git"
            | "gh"
    );
    if !is_read_only_binary {
        return false;
    }

    if contains_inline_code_execution(command) {
        return false;
    }

    if contains_bare_command(command, "tee") {
        return false;
    }

    if contains_sed_in_place(command) {
        return false;
    }

    // Deny output redirects that create or overwrite files.
    if command.contains('>') {
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_enforcer(mode: PermissionMode) -> PermissionEnforcer {
        let policy = PermissionPolicy::new(mode);
        PermissionEnforcer::new(policy)
    }

    #[test]
    fn allow_mode_permits_everything() {
        let enforcer = make_enforcer(PermissionMode::Allow);
        assert!(enforcer.is_allowed("bash", ""));
        assert!(enforcer.is_allowed("write_file", ""));
        assert!(enforcer.is_allowed("edit_file", ""));
        assert_eq!(
            enforcer.check_file_write("/outside/path", "/workspace"),
            EnforcementResult::Allowed
        );
        assert_eq!(enforcer.check_bash("rm -rf /"), EnforcementResult::Allowed);
    }

    #[test]
    fn read_only_denies_writes() {
        let policy = PermissionPolicy::new(PermissionMode::ReadOnly)
            .with_tool_requirement("read_file", PermissionMode::ReadOnly)
            .with_tool_requirement("grep_search", PermissionMode::ReadOnly)
            .with_tool_requirement("write_file", PermissionMode::WorkspaceWrite);

        let enforcer = PermissionEnforcer::new(policy);
        assert!(enforcer.is_allowed("read_file", ""));
        assert!(enforcer.is_allowed("grep_search", ""));

        // write_file requires WorkspaceWrite but we're in ReadOnly
        let result = enforcer.check("write_file", "");
        assert!(matches!(result, EnforcementResult::Denied { .. }));

        let result = enforcer.check_file_write("/workspace/file.rs", "/workspace");
        assert!(matches!(result, EnforcementResult::Denied { .. }));
    }

    #[test]
    fn read_only_allows_read_commands() {
        let policy = PermissionPolicy::new(PermissionMode::ReadOnly)
            .with_tool_requirement("bash", PermissionMode::ReadOnly);
        let enforcer = PermissionEnforcer::new(policy);
        assert_eq!(
            enforcer.check_bash("cat src/main.rs"),
            EnforcementResult::Allowed
        );
        assert_eq!(
            enforcer.check_bash("grep -r 'pattern' ."),
            EnforcementResult::Allowed
        );
        assert_eq!(enforcer.check_bash("ls -la"), EnforcementResult::Allowed);
    }

    #[test]
    fn read_only_denies_write_commands() {
        let policy = PermissionPolicy::new(PermissionMode::ReadOnly)
            .with_tool_requirement("bash", PermissionMode::ReadOnly);
        let enforcer = PermissionEnforcer::new(policy);
        let result = enforcer.check_bash("rm file.txt");
        assert!(matches!(result, EnforcementResult::Denied { .. }));
    }

    #[test]
    fn workspace_write_allows_within_workspace() {
        let policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite)
            .with_tool_requirement("write_file", PermissionMode::WorkspaceWrite);
        let enforcer = PermissionEnforcer::new(policy);
        let result = enforcer.check_file_write("/workspace/src/main.rs", "/workspace");
        assert_eq!(result, EnforcementResult::Allowed);
    }

    #[test]
    fn workspace_write_denies_outside_workspace() {
        let policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite)
            .with_tool_requirement("write_file", PermissionMode::WorkspaceWrite);
        let enforcer = PermissionEnforcer::new(policy);
        let result = enforcer.check_file_write("/etc/passwd", "/workspace");
        assert!(matches!(result, EnforcementResult::Denied { .. }));
    }

    #[test]
    fn prompt_mode_denies_without_prompter() {
        let enforcer = make_enforcer(PermissionMode::Prompt);
        let result = enforcer.check_bash("echo test");
        assert!(matches!(result, EnforcementResult::Denied { .. }));

        let result = enforcer.check_file_write("/workspace/file.rs", "/workspace");
        assert!(matches!(result, EnforcementResult::Denied { .. }));
    }

    #[test]
    fn workspace_boundary_check() {
        assert!(is_within_workspace("/workspace/src/main.rs", "/workspace"));
        assert!(is_within_workspace("/workspace", "/workspace"));
        assert!(!is_within_workspace("/etc/passwd", "/workspace"));
        assert!(!is_within_workspace("/workspacex/hack", "/workspace"));
    }

    #[test]
    fn workspace_boundary_canonicalizes_relative_missing_and_symlink_paths() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time should move forward")
            .as_nanos();
        let base = std::env::temp_dir().join(format!("himalaya-permission-boundary-{unique}"));
        let workspace = base.join("workspace");
        let child = workspace.join("child");
        std::fs::create_dir_all(&child).expect("workspace child should create");
        let outside = base.join("outside.txt");
        std::fs::write(&outside, "outside").expect("outside fixture should write");

        assert!(is_within_workspace(
            "src/new.rs",
            workspace.to_str().unwrap()
        ));
        assert!(is_within_workspace(
            workspace.join("child/../new.rs").to_str().unwrap(),
            workspace.to_str().unwrap()
        ));
        assert!(!is_within_workspace(
            workspace.join("../outside.txt").to_str().unwrap(),
            workspace.to_str().unwrap()
        ));

        #[cfg(unix)]
        {
            let link = workspace.join("outside-link.txt");
            std::os::unix::fs::symlink(&outside, &link).expect("symlink should create");
            assert!(!is_within_workspace(
                link.to_str().unwrap(),
                workspace.to_str().unwrap()
            ));
        }

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn read_only_command_heuristic() {
        assert!(is_read_only_command("cat file.txt"));
        assert!(is_read_only_command("grep pattern file"));
        assert!(is_read_only_command("git log --oneline"));
        assert!(!is_read_only_command("rm file.txt"));
        assert!(!is_read_only_command("echo test > file.txt"));
        assert!(!is_read_only_command("sed -i 's/a/b/' file"));
    }

    #[test]
    fn read_only_command_heuristic_blocks_shell_write_escape_patterns() {
        let cases = [
            "printf hello > out.txt",
            "cat Cargo.toml 2> errors.log",
            "echo hello | tee out.txt",
            "cat input.txt | sed -i.bak 's/a/b/'",
            "sed --in-place 's/a/b/' file.txt",
            "python -c 'open(\"out.txt\", \"w\").write(\"x\")'",
            "node -e 'require(\"fs\").writeFileSync(\"out.txt\", \"x\")'",
        ];

        for command in cases {
            assert!(
                !is_read_only_command(command),
                "expected {command} to be blocked"
            );
        }
    }

    #[test]
    fn active_mode_returns_policy_mode() {
        // given
        let modes = [
            PermissionMode::ReadOnly,
            PermissionMode::WorkspaceWrite,
            PermissionMode::DangerFullAccess,
            PermissionMode::Prompt,
            PermissionMode::Allow,
        ];

        // when
        let active_modes: Vec<_> = modes
            .into_iter()
            .map(|mode| make_enforcer(mode).active_mode())
            .collect();

        // then
        assert_eq!(active_modes, modes);
    }

    #[test]
    fn danger_full_access_permits_file_writes_and_bash() {
        // given
        let enforcer = make_enforcer(PermissionMode::DangerFullAccess);

        // when
        let file_result = enforcer.check_file_write("/outside/workspace/file.txt", "/workspace");
        let bash_result = enforcer.check_bash("rm -rf /tmp/scratch");

        // then
        assert_eq!(file_result, EnforcementResult::Allowed);
        assert_eq!(bash_result, EnforcementResult::Allowed);
    }

    #[test]
    fn check_denied_payload_contains_tool_and_modes() {
        // given
        let policy = PermissionPolicy::new(PermissionMode::ReadOnly)
            .with_tool_requirement("write_file", PermissionMode::WorkspaceWrite);
        let enforcer = PermissionEnforcer::new(policy);

        // when
        let result = enforcer.check("write_file", "{}");

        // then
        match result {
            EnforcementResult::Denied {
                tool,
                active_mode,
                required_mode,
                reason,
            } => {
                assert_eq!(tool, "write_file");
                assert_eq!(active_mode, "read-only");
                assert_eq!(required_mode, "workspace-write");
                assert!(reason.contains("requires workspace-write permission"));
            }
            other => panic!("expected denied result, got {other:?}"),
        }
    }

    #[test]
    fn workspace_write_relative_path_resolved() {
        // given
        let policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite)
            .with_tool_requirement("write_file", PermissionMode::WorkspaceWrite);
        let enforcer = PermissionEnforcer::new(policy);

        // when
        let result = enforcer.check_file_write("src/main.rs", "/workspace");

        // then
        assert_eq!(result, EnforcementResult::Allowed);
    }

    #[test]
    fn workspace_root_with_trailing_slash() {
        // given
        let policy = PermissionPolicy::new(PermissionMode::WorkspaceWrite)
            .with_tool_requirement("write_file", PermissionMode::WorkspaceWrite);
        let enforcer = PermissionEnforcer::new(policy);

        // when
        let result = enforcer.check_file_write("/workspace/src/main.rs", "/workspace/");

        // then
        assert_eq!(result, EnforcementResult::Allowed);
    }

    #[test]
    fn workspace_root_equality() {
        // given
        let root = "/workspace/";

        // when
        let equal_to_root = is_within_workspace("/workspace", root);

        // then
        assert!(equal_to_root);
    }

    #[test]
    fn bash_heuristic_full_path_prefix() {
        // given
        let full_path_command = "/usr/bin/cat Cargo.toml";
        let git_path_command = "/usr/local/bin/git status";

        // when
        let cat_result = is_read_only_command(full_path_command);
        let git_result = is_read_only_command(git_path_command);

        // then
        assert!(cat_result);
        assert!(git_result);
    }

    #[test]
    fn bash_heuristic_redirects_block_read_only_commands() {
        // given
        let overwrite = "cat Cargo.toml > out.txt";
        let append = "echo test >> out.txt";

        // when
        let overwrite_result = is_read_only_command(overwrite);
        let append_result = is_read_only_command(append);

        // then
        assert!(!overwrite_result);
        assert!(!append_result);
    }

    #[test]
    fn bash_heuristic_in_place_flag_blocks() {
        // given
        let interactive_python = "python -i script.py";
        let in_place_sed = "sed --in-place 's/a/b/' file.txt";

        // when
        let interactive_result = is_read_only_command(interactive_python);
        let in_place_result = is_read_only_command(in_place_sed);

        // then
        assert!(!interactive_result);
        assert!(!in_place_result);
    }

    #[test]
    fn bash_heuristic_empty_command() {
        // given
        let empty = "";
        let whitespace = "   ";

        // when
        let empty_result = is_read_only_command(empty);
        let whitespace_result = is_read_only_command(whitespace);

        // then
        assert!(!empty_result);
        assert!(!whitespace_result);
    }

    #[test]
    fn prompt_mode_check_bash_denied_payload_fields() {
        // given
        let enforcer = make_enforcer(PermissionMode::Prompt);

        // when
        let result = enforcer.check_bash("git status");

        // then
        match result {
            EnforcementResult::Denied {
                tool,
                active_mode,
                required_mode,
                reason,
            } => {
                assert_eq!(tool, "bash");
                assert_eq!(active_mode, "prompt");
                assert_eq!(required_mode, "danger-full-access");
                assert_eq!(
                    reason,
                    "tool 'bash' requires approval to escalate from prompt to danger-full-access"
                );
            }
            other => panic!("expected denied result, got {other:?}"),
        }
    }

    #[test]
    fn read_only_check_file_write_denied_payload() {
        // given
        let policy = PermissionPolicy::new(PermissionMode::ReadOnly)
            .with_tool_requirement("write_file", PermissionMode::WorkspaceWrite);
        let enforcer = PermissionEnforcer::new(policy);

        // when
        let result = enforcer.check_file_write("/workspace/file.txt", "/workspace");

        // then
        match result {
            EnforcementResult::Denied {
                tool,
                active_mode,
                required_mode,
                reason,
            } => {
                assert_eq!(tool, "write_file");
                assert_eq!(active_mode, "read-only");
                assert_eq!(required_mode, "workspace-write");
                assert!(reason.contains("workspace-write"));
            }
            other => panic!("expected denied result, got {other:?}"),
        }
    }
}
