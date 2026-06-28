//! Resolves Poe tasks from pyproject.toml and runs each sub-command
//! through the appropriate RTK filter.

use super::{mypy_cmd, pytest_cmd, ruff_cmd};
use crate::core::runner;
use anyhow::{bail, Context, Result};
use std::ffi::OsString;
use std::path::Path;
use toml::Value;

pub fn run(args: &[String], verbose: u8) -> Result<i32> {
    if args.is_empty() {
        bail!("Usage: rtk poe <task> [args...]");
    }

    let task_name = &args[0];
    let extra_args = &args[1..];

    let pyproject_path = Path::new("pyproject.toml");
    if !pyproject_path.exists() {
        bail!("No pyproject.toml found in current directory");
    }

    let content =
        std::fs::read_to_string(pyproject_path).context("Failed to read pyproject.toml")?;
    let doc: Value = content
        .parse::<Value>()
        .context("Failed to parse pyproject.toml")?;

    let tasks = doc
        .get("tool")
        .and_then(|t| t.get("poe"))
        .and_then(|p| p.get("tasks"))
        .context("No [tool.poe.tasks] section found in pyproject.toml")?;

    run_task(task_name, extra_args, tasks, verbose)
}

fn run_task(name: &str, extra_args: &[String], tasks: &Value, verbose: u8) -> Result<i32> {
    let task = tasks
        .get(name)
        .with_context(|| format!("Poe task '{}' not found in pyproject.toml", name))?;

    // Sequence task: { sequence = ["task1", "task2", ...] }
    if let Some(seq) = task.get("sequence").and_then(|s| s.as_array()) {
        for sub_task_val in seq {
            let sub_name = sub_task_val
                .as_str()
                .context("Sequence items must be strings")?;
            if verbose > 0 {
                eprintln!("poe: running sub-task '{}'", sub_name);
            }
            let code = run_task(sub_name, &[], tasks, verbose)?;
            if code != 0 {
                return Ok(code);
            }
        }
        return Ok(0);
    }

    // Cmd task: { cmd = "ruff check swiss" } or inline string "ruff check swiss"
    let cmd_str = if let Some(cmd) = task.get("cmd").and_then(|c| c.as_str()) {
        cmd.to_string()
    } else if let Some(cmd) = task.as_str() {
        cmd.to_string()
    } else {
        bail!(
            "Task '{}': only cmd and sequence tasks are supported (ref/script/shell not supported)",
            name
        );
    };

    let full_cmd = if extra_args.is_empty() {
        cmd_str
    } else {
        format!("{} {}", cmd_str, extra_args.join(" "))
    };

    if verbose > 0 {
        eprintln!("poe: {}", full_cmd);
    }

    run_cmd_string(&full_cmd, verbose)
}

fn run_cmd_string(cmd_str: &str, verbose: u8) -> Result<i32> {
    let parts: Vec<&str> = cmd_str.split_whitespace().collect();
    if parts.is_empty() {
        bail!("Empty command in poe task");
    }

    let tool = parts[0];
    let args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();

    match tool {
        "ruff" => ruff_cmd::run(&args, verbose),
        "mypy" => mypy_cmd::run(&args, verbose),
        "pytest" => pytest_cmd::run(&args, verbose),
        _ => {
            if verbose > 0 {
                eprintln!("poe: no RTK filter for '{}', running passthrough", tool);
            }
            let os_args: Vec<OsString> = args.iter().map(OsString::from).collect();
            runner::run_passthrough(tool, &os_args, verbose)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_tasks() -> Value {
        let toml_str = r#"
[ruff-format-check]
cmd = "ruff format --check swiss"

[ruff-check]
cmd = "ruff check swiss"

[mypy]
cmd = "mypy swiss"

[lint]
sequence = ["ruff-format-check", "ruff-check", "mypy"]
"#;
        toml_str.parse::<Value>().unwrap()
    }

    #[test]
    fn test_resolve_cmd_task() {
        let tasks = sample_tasks();
        let task = tasks.get("ruff-check").unwrap();
        let cmd = task.get("cmd").and_then(|c| c.as_str()).unwrap();
        assert_eq!(cmd, "ruff check swiss");
    }

    #[test]
    fn test_resolve_sequence_task() {
        let tasks = sample_tasks();
        let task = tasks.get("lint").unwrap();
        let seq = task
            .get("sequence")
            .and_then(|s| s.as_array())
            .unwrap();
        let names: Vec<&str> = seq.iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(names, vec!["ruff-format-check", "ruff-check", "mypy"]);
    }

    #[test]
    fn test_missing_task_returns_error() {
        let tasks = sample_tasks();
        let result = run_task("nonexistent", &[], &tasks, 0);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not found")
        );
    }

    #[test]
    fn test_inline_string_task() {
        let toml_str = r#"
hello = "echo hello"
"#;
        let tasks: Value = toml_str.parse().unwrap();
        let task = tasks.get("hello").unwrap();
        let cmd = task.as_str().unwrap();
        assert_eq!(cmd, "echo hello");
    }
}
