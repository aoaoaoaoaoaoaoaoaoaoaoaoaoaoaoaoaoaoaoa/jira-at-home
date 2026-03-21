use clap as _;
use dirs as _;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use libmcp as _;
use libmcp_testkit::read_json_lines;
use serde as _;
use serde_json::{Value, json};
use thiserror as _;
use time as _;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn must<T, E: std::fmt::Display, C: std::fmt::Display>(
    result: Result<T, E>,
    context: C,
) -> TestResult<T> {
    result.map_err(|error| io::Error::other(format!("{context}: {error}")).into())
}

fn must_some<T>(value: Option<T>, context: &str) -> TestResult<T> {
    value.ok_or_else(|| io::Error::other(context).into())
}

fn temp_project_root(name: &str) -> TestResult<PathBuf> {
    let root = std::env::temp_dir().join(format!(
        "jira_at_home_{name}_{}_{}",
        std::process::id(),
        must(
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH),
            "current time after unix epoch",
        )?
        .as_nanos()
    ));
    must(fs::create_dir_all(&root), "create temp project root")?;
    Ok(root)
}

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_jira-at-home"))
}

struct McpHarness {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl McpHarness {
    fn spawn(
        project_root: Option<&Path>,
        state_home: &Path,
        extra_env: &[(&str, &str)],
    ) -> TestResult<Self> {
        let mut command = Command::new(binary_path());
        let _ = command
            .arg("mcp")
            .arg("serve")
            .env("XDG_STATE_HOME", state_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(project_root) = project_root {
            let _ = command.arg("--project").arg(project_root);
        }
        for (key, value) in extra_env {
            let _ = command.env(key, value);
        }
        let mut child = must(command.spawn(), "spawn mcp host")?;
        let stdin = must_some(child.stdin.take(), "host stdin")?;
        let stdout = BufReader::new(must_some(child.stdout.take(), "host stdout")?);
        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    fn initialize(&mut self) -> TestResult<Value> {
        self.request(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "mcp-hardening-test", "version": "0" }
            }
        }))
    }

    fn notify_initialized(&mut self) -> TestResult {
        self.notify(json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }))
    }

    fn tools_list(&mut self) -> TestResult<Value> {
        self.request(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {},
        }))
    }

    fn bind_project(&mut self, id: u64, path: &Path) -> TestResult<Value> {
        self.call_tool(
            id,
            "project.bind",
            json!({ "path": path.display().to_string() }),
        )
    }

    fn call_tool(&mut self, id: u64, name: &str, arguments: Value) -> TestResult<Value> {
        self.request(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": arguments,
            }
        }))
    }

    fn call_tool_full(&mut self, id: u64, name: &str, arguments: Value) -> TestResult<Value> {
        let mut arguments = arguments.as_object().cloned().unwrap_or_default();
        let _ = arguments.insert("render".to_owned(), json!("json"));
        let _ = arguments.insert("detail".to_owned(), json!("full"));
        self.call_tool(id, name, Value::Object(arguments))
    }

    fn request(&mut self, message: Value) -> TestResult<Value> {
        let encoded = must(serde_json::to_string(&message), "request json")?;
        must(writeln!(self.stdin, "{encoded}"), "write request")?;
        must(self.stdin.flush(), "flush request")?;
        let mut line = String::new();
        let byte_count = must(self.stdout.read_line(&mut line), "read response")?;
        if byte_count == 0 {
            return Err(io::Error::other("unexpected EOF reading response").into());
        }
        must(serde_json::from_str(&line), "response json")
    }

    fn notify(&mut self, message: Value) -> TestResult {
        let encoded = must(serde_json::to_string(&message), "notify json")?;
        must(writeln!(self.stdin, "{encoded}"), "write notify")?;
        must(self.stdin.flush(), "flush notify")?;
        Ok(())
    }
}

impl Drop for McpHarness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn assert_tool_ok(response: &Value) {
    assert_eq!(
        response["result"]["isError"].as_bool(),
        Some(false),
        "tool response unexpectedly errored: {response:#}"
    );
}

fn tool_content(response: &Value) -> &Value {
    &response["result"]["structuredContent"]
}

fn tool_names(response: &Value) -> Vec<&str> {
    response["result"]["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["name"].as_str())
        .collect()
}

#[test]
fn cold_start_exposes_basic_toolset_and_binding_surface() -> TestResult {
    let project_root = temp_project_root("cold_start")?;
    let state_home = project_root.join("state-home");
    must(fs::create_dir_all(&state_home), "create state home")?;

    let mut harness = McpHarness::spawn(None, &state_home, &[])?;
    let initialize = harness.initialize()?;
    assert_eq!(
        initialize["result"]["protocolVersion"].as_str(),
        Some("2025-11-25")
    );
    harness.notify_initialized()?;

    let tools = harness.tools_list()?;
    let tool_names = tool_names(&tools);
    assert!(tool_names.contains(&"project.bind"));
    assert!(tool_names.contains(&"issue.save"));
    assert!(tool_names.contains(&"issue.list"));
    assert!(tool_names.contains(&"issue.read"));
    assert!(tool_names.contains(&"system.health"));
    assert!(tool_names.contains(&"system.telemetry"));

    let health = harness.call_tool(3, "system.health", json!({}))?;
    assert_tool_ok(&health);
    assert_eq!(tool_content(&health)["bound"].as_bool(), Some(false));

    let nested = project_root.join("nested").join("deeper");
    must(fs::create_dir_all(&nested), "create nested path")?;
    must(
        fs::create_dir_all(project_root.join(".git")),
        "create fake git root",
    )?;
    let bind = harness.bind_project(4, &nested)?;
    assert_tool_ok(&bind);
    assert_eq!(
        tool_content(&bind)["project_root"].as_str(),
        Some(project_root.display().to_string().as_str())
    );
    assert_eq!(tool_content(&bind)["issue_count"].as_u64(), Some(0));

    let rebound_health = harness.call_tool(5, "system.health", json!({}))?;
    assert_tool_ok(&rebound_health);
    assert_eq!(tool_content(&rebound_health)["bound"].as_bool(), Some(true));
    Ok(())
}

#[test]
fn save_list_and_read_roundtrip_through_state_backed_issue_dir() -> TestResult {
    let project_root = temp_project_root("roundtrip")?;
    let state_home = project_root.join("state-home");
    must(fs::create_dir_all(&state_home), "create state home")?;
    let mut harness = McpHarness::spawn(None, &state_home, &[])?;
    let _ = harness.initialize()?;
    harness.notify_initialized()?;

    let bind = harness.bind_project(2, &project_root)?;
    assert_tool_ok(&bind);
    let state_root = must_some(
        tool_content(&bind)["state_root"]
            .as_str()
            .map(PathBuf::from),
        "state root in bind response",
    )?;

    let body = "# Feral Machine\n\nMake note parking brutally small.";
    let save = harness.call_tool(
        3,
        "issue.save",
        json!({
            "slug": "feral-machine",
            "body": body,
        }),
    )?;
    assert_tool_ok(&save);
    assert_eq!(
        tool_content(&save)["path"].as_str(),
        Some("issues/feral-machine.md")
    );

    let saved_path = state_root.join("issues").join("feral-machine.md");
    assert_eq!(
        must(fs::read_to_string(&saved_path), "read saved issue")?,
        body
    );
    assert!(!project_root.join("issues").exists());

    let list = harness.call_tool(4, "issue.list", json!({}))?;
    assert_tool_ok(&list);
    assert_eq!(tool_content(&list)["count"].as_u64(), Some(1));
    assert_eq!(
        tool_content(&list)["issues"][0]["slug"].as_str(),
        Some("feral-machine")
    );
    assert!(tool_content(&list)["issues"][0].get("body").is_none());

    let read = harness.call_tool_full(
        5,
        "issue.read",
        json!({
            "slug": "feral-machine",
        }),
    )?;
    assert_tool_ok(&read);
    assert_eq!(tool_content(&read)["body"].as_str(), Some(body));
    assert_eq!(
        tool_content(&read)["path"].as_str(),
        Some("issues/feral-machine.md")
    );

    let telemetry_path = state_root.join("mcp").join("telemetry.jsonl");
    let events = must(
        read_json_lines::<Value>(&telemetry_path),
        "read telemetry log",
    )?;
    assert!(
        events
            .iter()
            .any(|event| event["event"] == "tool_call" && event["tool_name"] == "issue.save"),
        "expected issue.save tool_call event: {events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event["event"] == "hot_paths_snapshot"),
        "expected hot_paths_snapshot event: {events:#?}"
    );
    Ok(())
}

#[test]
fn convergent_issue_list_survives_worker_crash() -> TestResult {
    let project_root = temp_project_root("worker_retry")?;
    let state_home = project_root.join("state-home");
    must(fs::create_dir_all(&state_home), "create state home")?;
    let mut harness = McpHarness::spawn(
        Some(&project_root),
        &state_home,
        &[(
            "JIRA_AT_HOME_MCP_TEST_HOST_CRASH_ONCE_KEY",
            "tools/call:issue.list",
        )],
    )?;
    let _ = harness.initialize()?;
    harness.notify_initialized()?;

    let save = harness.call_tool(
        2,
        "issue.save",
        json!({
            "slug": "one-shot",
            "body": "body",
        }),
    )?;
    assert_tool_ok(&save);

    let list = harness.call_tool(3, "issue.list", json!({}))?;
    assert_tool_ok(&list);
    assert_eq!(tool_content(&list)["count"].as_u64(), Some(1));

    let telemetry = harness.call_tool_full(4, "system.telemetry", json!({}))?;
    assert_tool_ok(&telemetry);
    assert_eq!(
        tool_content(&telemetry)["telemetry"]["totals"]["retry_count"].as_u64(),
        Some(1)
    );
    assert!(
        tool_content(&telemetry)["telemetry"]["restart_count"]
            .as_u64()
            .is_some_and(|count| count >= 1)
    );
    Ok(())
}

#[test]
fn host_rollout_reexec_preserves_session_and_binding() -> TestResult {
    let project_root = temp_project_root("rollout")?;
    let state_home = project_root.join("state-home");
    must(fs::create_dir_all(&state_home), "create state home")?;
    let mut harness = McpHarness::spawn(
        Some(&project_root),
        &state_home,
        &[(
            "JIRA_AT_HOME_MCP_TEST_FORCE_ROLLOUT_KEY",
            "tools/call:issue.list",
        )],
    )?;
    let _ = harness.initialize()?;
    harness.notify_initialized()?;

    let save = harness.call_tool(
        2,
        "issue.save",
        json!({
            "slug": "after-rollout",
            "body": "body",
        }),
    )?;
    assert_tool_ok(&save);

    let list = harness.call_tool(3, "issue.list", json!({}))?;
    assert_tool_ok(&list);
    assert_eq!(tool_content(&list)["count"].as_u64(), Some(1));

    let health = harness.call_tool(4, "system.health", json!({}))?;
    assert_tool_ok(&health);
    assert_eq!(tool_content(&health)["bound"].as_bool(), Some(true));

    let read = harness.call_tool(
        5,
        "issue.read",
        json!({
            "slug": "after-rollout",
        }),
    )?;
    assert_tool_ok(&read);
    assert_eq!(tool_content(&read)["body"].as_str(), Some("body"));

    let telemetry = harness.call_tool_full(6, "system.telemetry", json!({}))?;
    assert_tool_ok(&telemetry);
    assert!(
        tool_content(&telemetry)["host_rollouts"]
            .as_u64()
            .is_some_and(|count| count >= 1)
    );
    Ok(())
}
