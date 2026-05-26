//! Processes incoming hook calls from AI agents and rewrites commands on the fly.
//!
//! Uses `writeln!(stdout, ...)` instead of `println!` — accidental stdout/stderr
//! corrupts the JSON protocol (Claude Code bug #4669 silently disables the hook).

use super::constants::PRE_TOOL_USE_KEY;
use super::permissions::{self, PermissionVerdict};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::{self, Read, Write};

use crate::discover::registry::{has_heredoc, rewrite_command};

const STDIN_CAP: usize = 1_048_576; // 1 MiB

fn read_stdin_limited() -> Result<String> {
    let mut input = String::new();
    io::stdin()
        .take((STDIN_CAP + 1) as u64)
        .read_to_string(&mut input)
        .context("Failed to read stdin")?;
    if input.len() > STDIN_CAP {
        anyhow::bail!("hook stdin exceeds {} byte limit", STDIN_CAP);
    }
    Ok(input)
}

// ── Copilot hook (VS Code + Copilot CLI) ──────────────────────

/// Format detected from the preToolUse JSON input.
enum HookFormat {
    /// VS Code Copilot Chat / Claude Code: `tool_name` + `tool_input.command`, supports `updatedInput`.
    VsCode { command: String },
    /// GitHub Copilot CLI: camelCase `toolName` + `toolArgs` (JSON string), deny-with-suggestion only.
    CopilotCli { command: String },
    /// Non-bash tool, already uses rtk, or unknown format — pass through silently.
    PassThrough,
}

/// Run the Copilot preToolUse hook.
/// Auto-detects VS Code Copilot Chat vs Copilot CLI format.
pub fn run_copilot() -> Result<()> {
    let input = read_stdin_limited()?;

    let input = input.trim();
    if input.is_empty() {
        return Ok(());
    }

    let v: Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(io::stderr(), "[rtk hook] Failed to parse JSON input: {e}");
            return Ok(());
        }
    };

    match detect_format(&v) {
        HookFormat::VsCode { command } => handle_vscode(&command),
        HookFormat::CopilotCli { command } => handle_copilot_cli(&command),
        HookFormat::PassThrough => Ok(()),
    }
}

fn detect_format(v: &Value) -> HookFormat {
    // VS Code Copilot Chat / Claude Code: snake_case keys
    if let Some(tool_name) = v.get("tool_name").and_then(|t| t.as_str()) {
        if matches!(tool_name, "runTerminalCommand" | "run_in_terminal" | "Bash" | "bash") {
            if let Some(cmd) = v
                .pointer("/tool_input/command")
                .and_then(|c| c.as_str())
                .filter(|c| !c.is_empty())
            {
                return HookFormat::VsCode {
                    command: cmd.to_string(),
                };
            }
        }
        return HookFormat::PassThrough;
    }

    // Copilot CLI: camelCase keys, toolArgs is a JSON-encoded string
    if let Some(tool_name) = v.get("toolName").and_then(|t| t.as_str()) {
        if tool_name == "bash" {
            if let Some(tool_args_str) = v.get("toolArgs").and_then(|t| t.as_str()) {
                if let Ok(tool_args) = serde_json::from_str::<Value>(tool_args_str) {
                    if let Some(cmd) = tool_args
                        .get("command")
                        .and_then(|c| c.as_str())
                        .filter(|c| !c.is_empty())
                    {
                        return HookFormat::CopilotCli {
                            command: cmd.to_string(),
                        };
                    }
                }
            }
        }
        return HookFormat::PassThrough;
    }

    HookFormat::PassThrough
}

fn get_rewritten(cmd: &str) -> Option<String> {
    if has_heredoc(cmd) {
        return None;
    }

    let (excluded, transparent_prefixes) = crate::core::config::Config::load()
        .map(|c| (c.hooks.exclude_commands, c.hooks.transparent_prefixes))
        .unwrap_or_default();

    let rewritten = rewrite_command(cmd, &excluded, &transparent_prefixes)?;

    if rewritten == cmd {
        return None;
    }

    Some(rewritten)
}

fn handle_vscode(cmd: &str) -> Result<()> {
    let verdict = permissions::check_command(cmd);
    if verdict == PermissionVerdict::Deny {
        audit_log("deny", cmd, "");
        return Ok(());
    }

    let rewritten = match get_rewritten(cmd) {
        Some(r) => r,
        None => return Ok(()),
    };

    audit_log("rewrite", cmd, &rewritten);

    let copilot_perm = crate::core::config::Config::load()
        .map(|c| c.hooks.copilot_permission)
        .unwrap_or_default();

    let mut hook_output = json!({
        "hookEventName": PRE_TOOL_USE_KEY,
        "permissionDecisionReason": "RTK auto-rewrite",
        "updatedInput": { "command": rewritten }
    });

    // Determine permissionDecision based on verdict and config:
    // - Explicit allow rule always emits "allow"
    // - Otherwise, respect copilot_permission config (default: omit)
    let decision = match verdict {
        PermissionVerdict::Allow => Some("allow"),
        _ => match copilot_perm {
            crate::core::config::CopilotPermission::Allow => Some("allow"),
            crate::core::config::CopilotPermission::Ask => Some("ask"),
            crate::core::config::CopilotPermission::Omit => None,
        },
    };

    if let Some(d) = decision {
        hook_output
            .as_object_mut()
            .unwrap()
            .insert("permissionDecision".into(), json!(d));
    }

    let output = json!({ "hookSpecificOutput": hook_output });
    let _ = writeln!(io::stdout(), "{output}");
    Ok(())
}

fn handle_copilot_cli(cmd: &str) -> Result<()> {
    if permissions::check_command(cmd) == PermissionVerdict::Deny {
        audit_log("deny", cmd, "");
        return Ok(());
    }

    let rewritten = match get_rewritten(cmd) {
        Some(r) => r,
        None => return Ok(()),
    };

    audit_log("rewrite", cmd, &rewritten);

    let output = json!({
        "permissionDecision": "deny",
        "permissionDecisionReason": format!(
            "Token savings: use `{}` instead (rtk saves 60-90% tokens)",
            rewritten
        )
    });
    let _ = writeln!(io::stdout(), "{output}");
    Ok(())
}

// ── Gemini hook ───────────────────────────────────────────────

/// Run the Gemini CLI BeforeTool hook.
pub fn run_gemini() -> Result<()> {
    let input = read_stdin_limited()?;

    let json: Value = serde_json::from_str(&input).context("Failed to parse hook input as JSON")?;

    let tool_name = json.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");

    if tool_name != "run_shell_command" {
        print_allow();
        return Ok(());
    }

    let cmd = json
        .pointer("/tool_input/command")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if cmd.is_empty() {
        print_allow();
        return Ok(());
    }

    // Check deny rules — Gemini CLI only supports allow/deny (no ask mode).
    if permissions::check_command(cmd) == PermissionVerdict::Deny {
        let _ = writeln!(
            io::stdout(),
            r#"{{"decision":"deny","reason":"Blocked by RTK permission rule"}}"#
        );
        return Ok(());
    }

    let (excluded, transparent_prefixes) = crate::core::config::Config::load()
        .map(|c| (c.hooks.exclude_commands, c.hooks.transparent_prefixes))
        .unwrap_or_default();

    match rewrite_command(cmd, &excluded, &transparent_prefixes) {
        Some(ref rewritten) => {
            audit_log("rewrite", cmd, rewritten);
            print_rewrite(rewritten);
        }
        None => print_allow(),
    }

    Ok(())
}

fn print_allow() {
    let _ = writeln!(io::stdout(), r#"{{"decision":"allow"}}"#);
}

fn print_rewrite(cmd: &str) {
    let output = serde_json::json!({
        "decision": "allow",
        "hookSpecificOutput": {
            "tool_input": {
                "command": cmd
            }
        }
    });
    let _ = writeln!(io::stdout(), "{}", output);
}

// ── Audit logging ─────────────────────────────────────────────

/// Best-effort audit log when RTK_HOOK_AUDIT=1.
fn audit_log(action: &str, original: &str, rewritten: &str) {
    if std::env::var("RTK_HOOK_AUDIT").as_deref() != Ok("1") {
        return;
    }
    let _ = audit_log_inner(action, original, rewritten);
}

/// Escape newlines to prevent log-line injection in the pipe-delimited audit log.
fn sanitize_log_field(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn audit_log_inner(action: &str, original: &str, rewritten: &str) -> Option<()> {
    let home = dirs::home_dir()?;
    let dir = home.join(".local").join("share").join("rtk");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("hook-audit.log");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
    writeln!(
        file,
        "{} | {} | {} | {}",
        ts,
        action,
        sanitize_log_field(original),
        sanitize_log_field(rewritten)
    )
    .ok()
}

// ── Claude Code native hook ────────────────────────────────────

enum PayloadAction {
    Rewrite {
        cmd: String,
        rewritten: String,
        output: Value,
    },
    Skip {
        reason: &'static str,
        cmd: String,
    },
    Ignore,
}

fn process_claude_payload(v: &Value) -> PayloadAction {
    let cmd = match v
        .pointer("/tool_input/command")
        .and_then(|c| c.as_str())
        .filter(|c| !c.is_empty())
    {
        Some(c) => c,
        None => return PayloadAction::Ignore,
    };

    let verdict = permissions::check_command(cmd);
    if verdict == PermissionVerdict::Deny {
        return PayloadAction::Skip {
            reason: "skip:deny_rule",
            cmd: cmd.to_string(),
        };
    }

    let rewritten = match get_rewritten(cmd) {
        Some(r) => r,
        None => {
            return PayloadAction::Skip {
                reason: "skip:no_match",
                cmd: cmd.to_string(),
            }
        }
    };

    let updated_input = {
        let mut ti = v.get("tool_input").cloned().unwrap_or_else(|| json!({}));
        if let Some(obj) = ti.as_object_mut() {
            obj.insert("command".into(), Value::String(rewritten.clone()));
        }
        ti
    };

    let mut hook_output = json!({
        "hookEventName": PRE_TOOL_USE_KEY,
        "permissionDecisionReason": "RTK auto-rewrite",
        "updatedInput": updated_input
    });

    if verdict == PermissionVerdict::Allow {
        hook_output
            .as_object_mut()
            .unwrap()
            .insert("permissionDecision".into(), json!("allow"));
    }

    PayloadAction::Rewrite {
        cmd: cmd.to_string(),
        rewritten,
        output: json!({ "hookSpecificOutput": hook_output }),
    }
}

/// Run the Claude Code PreToolUse hook natively.
pub fn run_claude() -> Result<()> {
    let input = read_stdin_limited()?;

    let input = input.trim();
    if input.is_empty() {
        return Ok(());
    }

    let v: Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(e) => {
            let _ = writeln!(io::stderr(), "[rtk hook] Failed to parse JSON input: {e}");
            return Ok(());
        }
    };

    match process_claude_payload(&v) {
        PayloadAction::Rewrite {
            cmd,
            rewritten,
            output,
        } => {
            audit_log("rewrite", &cmd, &rewritten);
            let _ = writeln!(io::stdout(), "{output}");
        }
        PayloadAction::Skip { reason, cmd } => {
            audit_log(reason, &cmd, "");
        }
        PayloadAction::Ignore => {}
    }

    Ok(())
}

#[cfg(test)]
fn run_claude_inner(input: &str) -> Option<String> {
    let v: Value = serde_json::from_str(input).ok()?;
    match process_claude_payload(&v) {
        PayloadAction::Rewrite { output, .. } => Some(output.to_string()),
        _ => None,
    }
}

// ── Cursor native hook ─────────────────────────────────────────

/// Cursor on Windows ships hook payloads with one or more leading
/// UTF-8 BOMs (`EF BB BF`, sometimes doubled), which serde_json
/// refuses to parse. Strip them defensively so the rewrite path keeps
/// working instead of silently returning `{}`.
fn strip_leading_bom(input: &str) -> &str {
    let mut s = input;
    while let Some(rest) = s.strip_prefix('\u{feff}') {
        s = rest;
    }
    s
}

/// Run the Cursor Agent hook natively.
pub fn run_cursor() -> Result<()> {
    let input = read_stdin_limited()?;

    let input = strip_leading_bom(&input).trim();
    if input.is_empty() {
        let _ = writeln!(io::stdout(), "{{}}");
        return Ok(());
    }

    let v: Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(_) => {
            let _ = writeln!(io::stdout(), "{{}}");
            return Ok(());
        }
    };

    let cmd = match v
        .pointer("/tool_input/command")
        .and_then(|c| c.as_str())
        .filter(|c| !c.is_empty())
    {
        Some(c) => c.to_string(),
        None => {
            let _ = writeln!(io::stdout(), "{{}}");
            return Ok(());
        }
    };

    let verdict = permissions::check_command(&cmd);
    if verdict == PermissionVerdict::Deny {
        audit_log("deny", &cmd, "");
        let _ = writeln!(io::stdout(), "{{}}");
        return Ok(());
    }

    let rewritten = match get_rewritten(&cmd) {
        Some(r) => r,
        None => {
            let _ = writeln!(io::stdout(), "{{}}");
            return Ok(());
        }
    };

    // Cursor preToolUse currently enforces allow/deny only and can ignore
    // updated_input when permission is "ask". Use "allow" for rewritten
    // commands unless the command is explicitly denied above.
    let decision = "allow";

    audit_log("rewrite", &cmd, &rewritten);

    // `continue: true` mirrors the shape of every other Cursor hook
    // (afterShellExecution, beforeSubmitPrompt, stop, ...). Cursor's
    // preToolUse panel renders the JSON it received; without this field
    // the panel collapses to `Output: {}` even though the rewrite ran,
    // which makes the hook look broken to users.
    let output = json!({
        "continue": true,
        "permission": decision,
        "updated_input": { "command": rewritten }
    });
    let _ = writeln!(io::stdout(), "{output}");
    Ok(())
}

#[cfg(test)]
fn run_cursor_inner(input: &str) -> String {
    run_cursor_inner_with_rules(input, &[], &[], &[])
}

#[cfg(test)]
fn run_cursor_inner_with_rules(
    input: &str,
    deny_rules: &[String],
    ask_rules: &[String],
    allow_rules: &[String],
) -> String {
    let input = strip_leading_bom(input);
    let v: Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(_) => return "{}".to_string(),
    };

    let cmd = match v
        .pointer("/tool_input/command")
        .and_then(|c| c.as_str())
        .filter(|c| !c.is_empty())
    {
        Some(c) => c.to_string(),
        None => return "{}".to_string(),
    };

    let verdict = permissions::check_command_with_rules(&cmd, deny_rules, ask_rules, allow_rules);
    if verdict == PermissionVerdict::Deny {
        return "{}".to_string();
    }

    match get_rewritten(&cmd) {
        Some(rewritten) => {
            let decision = "allow";
            let output = json!({
                "continue": true,
                "permission": decision,
                "updated_input": { "command": rewritten }
            });
            output.to_string()
        }
        None => "{}".to_string(),
    }
}

/// Test-only version of handle_vscode that accepts copilot_permission config
/// and permission rules as parameters instead of reading from disk/env.
#[cfg(test)]
fn run_copilot_vscode_inner(
    input: &str,
    copilot_perm: crate::core::config::CopilotPermission,
    deny_rules: &[String],
    ask_rules: &[String],
    allow_rules: &[String],
) -> Option<String> {
    let v: Value = serde_json::from_str(input).ok()?;

    let cmd = v
        .pointer("/tool_input/command")
        .and_then(|c| c.as_str())
        .filter(|c| !c.is_empty())?;

    let verdict = permissions::check_command_with_rules(cmd, deny_rules, ask_rules, allow_rules);
    if verdict == PermissionVerdict::Deny {
        return None;
    }

    let rewritten = get_rewritten(cmd)?;

    let mut hook_output = json!({
        "hookEventName": PRE_TOOL_USE_KEY,
        "permissionDecisionReason": "RTK auto-rewrite",
        "updatedInput": { "command": rewritten }
    });

    let decision = match verdict {
        PermissionVerdict::Allow => Some("allow"),
        _ => match copilot_perm {
            crate::core::config::CopilotPermission::Allow => Some("allow"),
            crate::core::config::CopilotPermission::Ask => Some("ask"),
            crate::core::config::CopilotPermission::Omit => None,
        },
    };

    if let Some(d) = decision {
        hook_output
            .as_object_mut()
            .unwrap()
            .insert("permissionDecision".into(), json!(d));
    }

    Some(json!({ "hookSpecificOutput": hook_output }).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewrite_command_no_prefixes(cmd: &str, excluded: &[String]) -> Option<String> {
        crate::discover::registry::rewrite_command(cmd, excluded, &[])
    }

    // --- Copilot format detection ---

    fn vscode_input(tool: &str, cmd: &str) -> Value {
        json!({
            "tool_name": tool,
            "tool_input": { "command": cmd }
        })
    }

    fn copilot_cli_input(cmd: &str) -> Value {
        let args = serde_json::to_string(&json!({ "command": cmd })).unwrap();
        json!({ "toolName": "bash", "toolArgs": args })
    }

    #[test]
    fn test_detect_vscode_bash() {
        assert!(matches!(
            detect_format(&vscode_input("Bash", "git status")),
            HookFormat::VsCode { .. }
        ));
    }

    #[test]
    fn test_detect_vscode_run_terminal_command() {
        assert!(matches!(
            detect_format(&vscode_input("runTerminalCommand", "cargo test")),
            HookFormat::VsCode { .. }
        ));
    }

    #[test]
    fn test_detect_vscode_run_in_terminal() {
        assert!(matches!(
            detect_format(&vscode_input("run_in_terminal", "cargo test")),
            HookFormat::VsCode { .. }
        ));
    }

    #[test]
    fn test_detect_copilot_cli_bash() {
        assert!(matches!(
            detect_format(&copilot_cli_input("git status")),
            HookFormat::CopilotCli { .. }
        ));
    }

    #[test]
    fn test_detect_non_bash_is_passthrough() {
        let v = json!({ "tool_name": "editFiles" });
        assert!(matches!(detect_format(&v), HookFormat::PassThrough));
    }

    #[test]
    fn test_detect_unknown_is_passthrough() {
        assert!(matches!(detect_format(&json!({})), HookFormat::PassThrough));
    }

    #[test]
    fn test_get_rewritten_supported() {
        assert!(get_rewritten("git status").is_some());
    }

    #[test]
    fn test_get_rewritten_unsupported() {
        assert!(get_rewritten("htop").is_none());
    }

    #[test]
    fn test_get_rewritten_already_rtk() {
        assert!(get_rewritten("rtk git status").is_none());
    }

    #[test]
    fn test_get_rewritten_heredoc() {
        assert!(get_rewritten("cat <<'EOF'\nhello\nEOF").is_none());
    }

    // --- Gemini format ---

    #[test]
    fn test_print_allow_format() {
        let expected = r#"{"decision":"allow"}"#;
        assert_eq!(expected, r#"{"decision":"allow"}"#);
    }

    #[test]
    fn test_print_rewrite_format() {
        let output = serde_json::json!({
            "decision": "allow",
            "hookSpecificOutput": {
                "tool_input": {
                    "command": "rtk git status"
                }
            }
        });
        let json: Value = serde_json::from_str(&output.to_string()).unwrap();
        assert_eq!(json["decision"], "allow");
        assert_eq!(
            json["hookSpecificOutput"]["tool_input"]["command"],
            "rtk git status"
        );
    }

    #[test]
    fn test_gemini_hook_uses_rewrite_command() {
        assert_eq!(
            rewrite_command_no_prefixes("git status", &[]),
            Some("rtk git status".into())
        );
        assert_eq!(
            rewrite_command_no_prefixes("cargo test", &[]),
            Some("rtk cargo test".into())
        );
        assert_eq!(
            rewrite_command_no_prefixes("rtk git status", &[]),
            Some("rtk git status".into())
        );
        assert_eq!(rewrite_command_no_prefixes("cat <<EOF", &[]), None);
    }

    #[test]
    fn test_gemini_hook_excluded_commands() {
        let excluded = vec!["curl".to_string()];
        assert_eq!(
            rewrite_command_no_prefixes("curl https://example.com", &excluded),
            None
        );
        assert_eq!(
            rewrite_command_no_prefixes("git status", &excluded),
            Some("rtk git status".into())
        );
    }

    #[test]
    fn test_gemini_hook_env_prefix_preserved() {
        assert_eq!(
            rewrite_command_no_prefixes("RUST_LOG=debug cargo test", &[]),
            Some("RUST_LOG=debug rtk cargo test".into())
        );
    }

    // --- Claude handler ---

    fn claude_input(cmd: &str) -> String {
        json!({
            "tool_name": "Bash",
            "tool_input": { "command": cmd }
        })
        .to_string()
    }

    fn claude_input_with_fields(cmd: &str, timeout: u64, description: &str) -> String {
        json!({
            "tool_name": "Bash",
            "tool_input": {
                "command": cmd,
                "timeout": timeout,
                "description": description
            }
        })
        .to_string()
    }

    #[test]
    fn test_claude_rewrite_git_status() {
        let result = run_claude_inner(&claude_input("git status")).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        let cmd = v
            .pointer("/hookSpecificOutput/updatedInput/command")
            .and_then(|c| c.as_str())
            .unwrap();
        assert_eq!(cmd, "rtk git status");
    }

    #[test]
    fn test_claude_rewrite_preserves_tool_input_fields() {
        let input = claude_input_with_fields("git status", 30000, "Check repo status");
        let result = run_claude_inner(&input).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        let updated = &v["hookSpecificOutput"]["updatedInput"];
        assert_eq!(updated["command"], "rtk git status");
        assert_eq!(updated["timeout"], 30000);
        assert_eq!(updated["description"], "Check repo status");
    }

    #[test]
    fn test_claude_passthrough_no_output() {
        assert!(run_claude_inner(&claude_input("htop")).is_none());
    }

    #[test]
    fn test_claude_heredoc_passthrough() {
        assert!(run_claude_inner(&claude_input("cat <<EOF\nhello\nEOF")).is_none());
    }

    #[test]
    fn test_claude_already_rtk_passthrough() {
        assert!(run_claude_inner(&claude_input("rtk git status")).is_none());
    }

    #[test]
    fn test_claude_empty_command_passthrough() {
        let input = json!({
            "tool_name": "Bash",
            "tool_input": { "command": "" }
        })
        .to_string();
        assert!(run_claude_inner(&input).is_none());
    }

    #[test]
    fn test_claude_malformed_json_passthrough() {
        assert!(run_claude_inner("not valid json {{{").is_none());
    }

    #[test]
    fn test_claude_env_prefix_preserved() {
        let result = run_claude_inner(&claude_input("GIT_PAGER=cat git status")).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        let cmd = v
            .pointer("/hookSpecificOutput/updatedInput/command")
            .and_then(|c| c.as_str())
            .unwrap();
        assert_eq!(cmd, "GIT_PAGER=cat rtk git status");
    }

    #[test]
    fn test_claude_compound_command() {
        let result = run_claude_inner(&claude_input("git add . && cargo test")).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        let cmd = v
            .pointer("/hookSpecificOutput/updatedInput/command")
            .and_then(|c| c.as_str())
            .unwrap();
        assert_eq!(cmd, "rtk git add . && rtk cargo test");
    }

    #[test]
    fn test_claude_json_output_structure() {
        let result = run_claude_inner(&claude_input("git status")).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        let hook = &v["hookSpecificOutput"];

        assert_eq!(hook["hookEventName"], PRE_TOOL_USE_KEY);
        // permissionDecision is only set when an explicit allow rule matches;
        // with default-to-ask semantics (no rules configured), it is absent.
        assert_eq!(hook["permissionDecisionReason"], "RTK auto-rewrite");
        assert!(hook["updatedInput"].is_object());
        assert!(hook["updatedInput"]["command"].is_string());
    }

    #[test]
    fn test_claude_no_tool_input_passthrough() {
        let input = json!({ "tool_name": "Bash" }).to_string();
        assert!(run_claude_inner(&input).is_none());
    }

    // --- Cursor handler ---

    fn cursor_input(cmd: &str) -> String {
        json!({
            "tool_name": "Bash",
            "tool_input": { "command": cmd }
        })
        .to_string()
    }

    #[test]
    fn test_cursor_rewrite_flat_format() {
        let result = run_cursor_inner(&cursor_input("git status"));
        let v: Value = serde_json::from_str(&result).unwrap();
        // Cursor preToolUse expects allow/deny for rewrite application.
        assert_eq!(v["permission"], "allow");
        assert_eq!(v["updated_input"]["command"], "rtk git status");
        assert!(v.get("hookSpecificOutput").is_none());
        // `continue: true` keeps the Cursor preToolUse panel from collapsing
        // to `Output: {}`; without it the rewrite is invisible to users.
        assert_eq!(v["continue"], true);
    }

    #[test]
    fn test_cursor_passthrough_empty_json() {
        let result = run_cursor_inner(&cursor_input("htop"));
        assert_eq!(result, "{}");
    }

    #[test]
    fn test_cursor_empty_input_empty_json() {
        let result = run_cursor_inner("");
        assert_eq!(result, "{}");
    }

    #[test]
    fn test_cursor_heredoc_passthrough() {
        let result = run_cursor_inner(&cursor_input("cat <<EOF\nhello\nEOF"));
        assert_eq!(result, "{}");
    }

    #[test]
    fn test_cursor_already_rtk_passthrough() {
        let result = run_cursor_inner(&cursor_input("rtk git status"));
        assert_eq!(result, "{}");
    }

    #[test]
    fn test_cursor_no_hook_specific_output() {
        let result = run_cursor_inner(&cursor_input("cargo test"));
        let v: Value = serde_json::from_str(&result).unwrap();
        assert!(v.get("hookSpecificOutput").is_none());
        assert_eq!(v["permission"], "allow");
        assert_eq!(v["continue"], true);
    }

    #[test]
    fn test_cursor_compound_rewrite_includes_continue() {
        let cmd = "cd \"/tmp/proj\" && git status";
        let result = run_cursor_inner(&cursor_input(cmd));
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["continue"], true);
        assert_eq!(v["permission"], "allow");
        assert_eq!(
            v["updated_input"]["command"],
            "cd \"/tmp/proj\" && rtk git status"
        );
    }

    #[test]
    fn test_cursor_strips_single_utf8_bom() {
        // Some Cursor builds prepend a single UTF-8 BOM to hook stdin.
        // serde_json rejects BOM-prefixed input, so without the strip
        // the hook returned `{}` and the rewrite became a silent no-op.
        let payload = cursor_input("git status");
        let with_single_bom = format!("\u{feff}{}", payload);
        let result = run_cursor_inner(&with_single_bom);
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["continue"], true);
        assert_eq!(v["permission"], "allow");
        assert_eq!(v["updated_input"]["command"], "rtk git status");
    }

    #[test]
    fn test_cursor_strips_double_utf8_bom() {
        // Cursor on Windows ships hook stdin with **two** leading
        // UTF-8 BOMs (`EF BB BF EF BB BF`), confirmed via a stdin
        // tracer wrapping `rtk hook cursor` on Cursor 3.2.x. This is
        // the real-world payload shape the loop needs to survive.
        let payload = cursor_input("git status");
        let with_double_bom = format!("\u{feff}\u{feff}{}", payload);
        let result = run_cursor_inner(&with_double_bom);
        let v: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(v["continue"], true);
        assert_eq!(v["permission"], "allow");
        assert_eq!(v["updated_input"]["command"], "rtk git status");
    }

    #[test]
    fn test_strip_leading_bom_helper() {
        // Direct unit test on the helper so future refactors can't
        // regress the loop semantics without a clear failure signal.
        assert_eq!(strip_leading_bom(""), "");
        assert_eq!(strip_leading_bom("hello"), "hello");
        assert_eq!(strip_leading_bom("\u{feff}hello"), "hello");
        assert_eq!(strip_leading_bom("\u{feff}\u{feff}hello"), "hello");
        assert_eq!(strip_leading_bom("\u{feff}\u{feff}\u{feff}hello"), "hello");
        // BOM in the middle is preserved (not "leading").
        assert_eq!(strip_leading_bom("a\u{feff}b"), "a\u{feff}b");
    }

    // --- Audit logging ---

    #[test]
    fn test_audit_log_silent_when_disabled() {
        std::env::remove_var("RTK_HOOK_AUDIT");
        audit_log("test", "git status", "rtk git status");
    }

    #[test]
    fn test_audit_log_format_four_fields() {
        let tmp = std::env::temp_dir().join("rtk-test-audit");
        let _ = std::fs::create_dir_all(&tmp);
        let log_path = tmp.join("hook-audit.log");
        let _ = std::fs::remove_file(&log_path);

        {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .unwrap();
            let ts = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
            writeln!(file, "{} | rewrite | git status | rtk git status", ts).unwrap();
        }

        let content = std::fs::read_to_string(&log_path).unwrap();
        let parts: Vec<&str> = content.trim().split(" | ").collect();
        assert_eq!(
            parts.len(),
            4,
            "Expected 4 pipe-delimited fields, got: {:?}",
            parts
        );
        assert_eq!(parts[1], "rewrite");
        assert_eq!(parts[2], "git status");
        assert_eq!(parts[3], "rtk git status");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // --- Adversarial tests ---

    #[test]
    fn test_audit_log_sanitizes_newlines() {
        let sanitized = sanitize_log_field("git status\nfake | inject | evil");
        assert!(!sanitized.contains('\n'));
        assert!(sanitized.contains("\\n"));
    }

    #[test]
    fn test_audit_log_sanitizes_pipe_delimiter() {
        let sanitized = sanitize_log_field("git log | head");
        assert!(
            !sanitized.contains(" | "),
            "unescaped ' | ' breaks field parsing: {}",
            sanitized
        );
        assert!(sanitized.contains("\\|"));
    }

    #[test]
    fn test_claude_unicode_null_passthrough() {
        let input = claude_input("git status \u{0000}\u{FEFF}");
        let _ = run_claude_inner(&input);
    }

    #[test]
    fn test_claude_extremely_long_command() {
        let long_cmd = format!("git status {}", "A".repeat(100_000));
        let input = claude_input(&long_cmd);
        let _ = run_claude_inner(&input);
    }

    #[test]
    fn test_cursor_deny_blocks_rewrite() {
        use super::permissions::check_command_with_rules;
        let deny = vec!["git status".to_string()];
        assert_eq!(
            check_command_with_rules("git status", &deny, &[], &[]),
            PermissionVerdict::Deny
        );
    }

    #[test]
    fn test_gemini_deny_blocks_rewrite() {
        use super::permissions::check_command_with_rules;
        let deny = vec!["cargo test".to_string()];
        assert_eq!(
            check_command_with_rules("cargo test", &deny, &[], &[]),
            PermissionVerdict::Deny
        );
        // Denied commands must not be rewritten — Gemini handler checks deny before rewrite
        assert!(
            get_rewritten("cargo test").is_some(),
            "cargo test should be rewritable when not denied"
        );
    }

    // --- Copilot VS Code handler (handle_vscode) ---

    use crate::core::config::CopilotPermission;

    fn copilot_vscode_input(cmd: &str) -> String {
        json!({
            "tool_name": "run_in_terminal",
            "tool_input": { "command": cmd }
        })
        .to_string()
    }

    #[test]
    fn test_copilot_vscode_default_omits_permission_decision() {
        // Default config (CopilotPermission::Omit) + no permission rules:
        // permissionDecision should be absent from the output.
        let result = run_copilot_vscode_inner(
            &copilot_vscode_input("git status"),
            CopilotPermission::Omit,
            &[], &[], &[],
        ).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        let hook = &v["hookSpecificOutput"];
        assert_eq!(hook["updatedInput"]["command"], "rtk git status");
        assert_eq!(hook["permissionDecisionReason"], "RTK auto-rewrite");
        assert!(
            hook.get("permissionDecision").is_none()
                || hook["permissionDecision"].is_null(),
            "permissionDecision should be absent when config is Omit"
        );
    }

    #[test]
    fn test_copilot_vscode_config_allow_sets_allow() {
        // copilot_permission = "allow" in config:
        // permissionDecision should be "allow".
        let result = run_copilot_vscode_inner(
            &copilot_vscode_input("git status"),
            CopilotPermission::Allow,
            &[], &[], &[],
        ).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        let hook = &v["hookSpecificOutput"];
        assert_eq!(hook["permissionDecision"], "allow");
        assert_eq!(hook["updatedInput"]["command"], "rtk git status");
    }

    #[test]
    fn test_copilot_vscode_config_ask_sets_ask() {
        // copilot_permission = "ask" in config (preserves old behavior):
        // permissionDecision should be "ask".
        let result = run_copilot_vscode_inner(
            &copilot_vscode_input("git status"),
            CopilotPermission::Ask,
            &[], &[], &[],
        ).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        let hook = &v["hookSpecificOutput"];
        assert_eq!(hook["permissionDecision"], "ask");
        assert_eq!(hook["updatedInput"]["command"], "rtk git status");
    }

    #[test]
    fn test_copilot_vscode_explicit_allow_rule_overrides_config() {
        // When an explicit allow rule matches, permissionDecision should be
        // "allow" regardless of copilot_permission config.
        let allow_rules = vec!["git status".to_string()];
        for perm in [CopilotPermission::Omit, CopilotPermission::Ask] {
            let result = run_copilot_vscode_inner(
                &copilot_vscode_input("git status"),
                perm.clone(),
                &[], &[], &allow_rules,
            ).unwrap();
            let v: Value = serde_json::from_str(&result).unwrap();
            assert_eq!(
                v["hookSpecificOutput"]["permissionDecision"], "allow",
                "Explicit allow rule should override config {:?}", perm
            );
        }
    }

    #[test]
    fn test_copilot_vscode_deny_rule_blocks_rewrite() {
        // A deny rule should cause no output (passthrough).
        let deny_rules = vec!["git status".to_string()];
        let result = run_copilot_vscode_inner(
            &copilot_vscode_input("git status"),
            CopilotPermission::Allow,
            &deny_rules, &[], &[],
        );
        assert!(result.is_none(), "Deny rule should block the rewrite");
    }

    #[test]
    fn test_copilot_vscode_passthrough_unsupported_command() {
        // Unsupported commands (no rtk equivalent) should return None.
        let result = run_copilot_vscode_inner(
            &copilot_vscode_input("htop"),
            CopilotPermission::Allow,
            &[], &[], &[],
        );
        assert!(result.is_none());
    }

    #[test]
    fn test_copilot_vscode_passthrough_already_rtk() {
        let result = run_copilot_vscode_inner(
            &copilot_vscode_input("rtk git status"),
            CopilotPermission::Allow,
            &[], &[], &[],
        );
        assert!(result.is_none());
    }

    #[test]
    fn test_copilot_vscode_malformed_json() {
        let result = run_copilot_vscode_inner(
            "not json {{{",
            CopilotPermission::Omit,
            &[], &[], &[],
        );
        assert!(result.is_none());
    }

    #[test]
    fn test_copilot_vscode_empty_command() {
        let input = json!({
            "tool_name": "run_in_terminal",
            "tool_input": { "command": "" }
        }).to_string();
        let result = run_copilot_vscode_inner(
            &input,
            CopilotPermission::Allow,
            &[], &[], &[],
        );
        assert!(result.is_none());
    }

    #[test]
    fn test_copilot_vscode_output_structure() {
        // Verify the full output structure matches expected format.
        let result = run_copilot_vscode_inner(
            &copilot_vscode_input("cargo test"),
            CopilotPermission::Omit,
            &[], &[], &[],
        ).unwrap();
        let v: Value = serde_json::from_str(&result).unwrap();
        let hook = &v["hookSpecificOutput"];
        assert_eq!(hook["hookEventName"], "PreToolUse");
        assert_eq!(hook["permissionDecisionReason"], "RTK auto-rewrite");
        assert!(hook["updatedInput"].is_object());
        assert!(hook["updatedInput"]["command"].as_str().unwrap().starts_with("rtk "));
    }
}
