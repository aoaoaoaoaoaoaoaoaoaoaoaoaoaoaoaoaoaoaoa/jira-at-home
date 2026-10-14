use clap as _;
use dirs as _;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use libmcp as _;
use libmcp_testkit::{TestCell, read_json_lines};
use serde as _;
use serde_json::{Value, json};
use thiserror as _;
use time as _;
use toml as _;

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

fn temp_project_root(name: &str) -> TestResult<TestCell> {
    Ok(TestCell::new(&format!("jira-at-home-{name}-"))?)
}

fn install_linked_worktree_gitdir(primary: &Path, linked: &Path) -> TestResult {
    let primary_git = primary.join(".git");
    let linked_gitdir = primary_git.join("worktrees").join("touching-demands");
    must(fs::create_dir_all(&linked_gitdir), "create linked gitdir")?;
    must(fs::create_dir_all(linked), "create linked worktree")?;
    must(
        fs::write(primary_git.join("HEAD"), "ref: refs/heads/main\n"),
        "write primary HEAD",
    )?;
    must(
        fs::write(linked_gitdir.join("HEAD"), "ref: refs/heads/linked\n"),
        "write linked HEAD",
    )?;
    must(
        fs::write(linked_gitdir.join("commondir"), "../..\n"),
        "write linked commondir",
    )?;
    must(
        fs::write(
            linked.join(".git"),
            format!("gitdir: {}\n", linked_gitdir.display()),
        ),
        "write linked gitdir file",
    )?;
    Ok(())
}

fn state_path_for(state_home: &Path, project_root: &Path) -> PathBuf {
    let mut base = state_home.join("jira_at_home").join("projects");
    for component in project_root.components() {
        match component {
            Component::Normal(part) => base.push(part),
            Component::Prefix(prefix) => base.push(prefix.as_os_str()),
            Component::CurDir | Component::ParentDir | Component::RootDir => {}
        }
    }
    base
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
            // Handoff snapshots belong to this fixture, not the caller's runtime.
            .env("XDG_RUNTIME_DIR", state_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(project_root) = project_root {
            let _ = command.current_dir(project_root);
        } else {
            let _ = command.current_dir(must_some(state_home.parent(), "fixture project root")?);
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
        self.request(&json!({
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
        self.notify(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }))
    }

    fn tools_list(&mut self) -> TestResult<Value> {
        self.request(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {},
        }))
    }

    fn call_tool(&mut self, id: u64, name: &str, mut arguments: Value) -> TestResult<Value> {
        if let Some(arguments) = arguments.as_object_mut() {
            let _ = arguments
                .entry("render".to_owned())
                .or_insert_with(|| Value::String("json".to_owned()));
        }
        let mut params = serde_json::Map::new();
        let _ = params.insert("name".to_owned(), Value::String(name.to_owned()));
        let _ = params.insert("arguments".to_owned(), arguments);
        self.request(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": params,
        }))
    }

    fn call_tool_full(&mut self, id: u64, name: &str, arguments: Value) -> TestResult<Value> {
        let mut arguments = match arguments {
            Value::Object(arguments) => arguments,
            _ => serde_json::Map::new(),
        };
        let _ = arguments.insert("render".to_owned(), json!("json"));
        let _ = arguments.insert("detail".to_owned(), json!("full"));
        self.call_tool(id, name, Value::Object(arguments))
    }

    fn request(&mut self, message: &Value) -> TestResult<Value> {
        let encoded = must(serde_json::to_string(&message), "request json")?;
        self.request_raw(encoded.as_bytes())
    }

    fn request_raw(&mut self, encoded: &[u8]) -> TestResult<Value> {
        must(self.stdin.write_all(encoded), "write raw request")?;
        must(self.stdin.write_all(b"\n"), "frame raw request")?;
        must(self.stdin.flush(), "flush request")?;
        let mut line = String::new();
        let byte_count = must(self.stdout.read_line(&mut line), "read response")?;
        if byte_count == 0 {
            return Err(io::Error::other("unexpected EOF reading response").into());
        }
        must(serde_json::from_str(&line), "response json")
    }

    fn notify(&mut self, message: &Value) -> TestResult {
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

#[test]
fn public_protocol_distinguishes_parse_and_envelope_rejections() -> TestResult {
    let project_root = temp_project_root("protocol_rejections")?;
    let state_home = project_root.join("state-home");
    must(fs::create_dir_all(&state_home), "create state home")?;
    let mut harness = McpHarness::spawn(None, &state_home, &[])?;

    let malformed = harness.request_raw(b"{")?;
    assert_eq!(malformed["error"]["code"].as_i64(), Some(-32700));
    assert_eq!(
        malformed["error"]["data"]["fault"]["code"].as_str(),
        Some("parse_error")
    );

    let duplicate = harness.request_raw(
        br#"{"jsonrpc":"2.0","id":1,"method":"initialize","method":"ping","params":{}}"#,
    )?;
    assert_eq!(duplicate["error"]["code"].as_i64(), Some(-32600));
    assert_eq!(
        duplicate["error"]["data"]["fault"]["code"].as_str(),
        Some("invalid_request")
    );

    let initialize = harness.initialize()?;
    assert_eq!(initialize["id"].as_u64(), Some(1));
    assert!(initialize.get("result").is_some());
    Ok(())
}

#[test]
fn public_transport_terminates_on_an_oversized_frame() -> TestResult {
    let project_root = temp_project_root("oversized_frame")?;
    let state_home = project_root.join("state-home");
    must(fs::create_dir_all(&state_home), "create state home")?;
    let mut harness = McpHarness::spawn(None, &state_home, &[])?;
    let oversized = vec![b'x'; libmcp::FrameLimit::DEFAULT.get() + 1];

    if let Err(error) = harness.stdin.write_all(&oversized)
        && error.kind() != io::ErrorKind::BrokenPipe
    {
        return Err(error.into());
    }
    let status = must(harness.child.wait(), "wait for oversized-frame rejection")?;
    assert!(
        !status.success(),
        "oversized public frame must terminate the host"
    );
    Ok(())
}

fn assert_tool_ok(response: &Value) {
    assert_eq!(
        response["result"]["isError"].as_bool(),
        Some(false),
        "tool response unexpectedly errored: {response:#}"
    );
}

fn assert_tool_error(response: &Value) {
    assert_eq!(
        response["result"]["isError"].as_bool(),
        Some(true),
        "tool response unexpectedly succeeded: {response:#}"
    );
}

fn tool_content(response: &Value) -> &Value {
    &response["result"]["structuredContent"]
}

fn error_message(response: &Value) -> &str {
    tool_content(response)["message"]
        .as_str()
        .unwrap_or_default()
}

fn tool_names(response: &Value) -> Vec<&str> {
    response["result"]["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["name"].as_str())
        .collect()
}

fn tool_definition<'a>(response: &'a Value, name: &str) -> TestResult<&'a Value> {
    response["result"]["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|tool| tool["name"].as_str() == Some(name))
        .ok_or_else(|| io::Error::other(format!("missing tool definition `{name}`")).into())
}

#[test]
fn cold_start_uses_cwd_and_isolates_explicit_projects() -> TestResult {
    let project_root = temp_project_root("cold_start")?;
    let other_root = temp_project_root("other_project")?;
    let state_home = project_root.join("state-home");
    must(fs::create_dir_all(&state_home), "create state home")?;
    must(
        fs::create_dir_all(project_root.join(".git")),
        "create fake git root",
    )?;
    must(
        fs::create_dir_all(other_root.join(".git")),
        "create override git root",
    )?;
    must(
        fs::write(project_root.join(".git/HEAD"), "ref: refs/heads/main\n"),
        "write default HEAD",
    )?;
    must(
        fs::write(other_root.join(".git/HEAD"), "ref: refs/heads/main\n"),
        "write override HEAD",
    )?;
    let nested = project_root.join("nested/deeper");
    let other_nested = other_root.join("nested");
    must(
        fs::create_dir_all(&nested),
        "create default nested directory",
    )?;
    must(
        fs::create_dir_all(&other_nested),
        "create override nested directory",
    )?;
    let marker = other_nested.join("marker");
    must(fs::write(&marker, ""), "create override nested file")?;

    let mut harness = McpHarness::spawn(Some(&nested), &state_home, &[])?;
    let initialize = harness.initialize()?;
    assert_eq!(initialize["result"]["protocolVersion"], json!("2025-11-25"));
    harness.notify_initialized()?;
    let tools = harness.tools_list()?;
    assert_eq!(
        tool_names(&tools),
        [
            "issue.save",
            "issue.delete",
            "issue.list",
            "issue.read",
            "system.health",
            "system.telemetry"
        ]
    );
    for name in tool_names(&tools) {
        let tool = tool_definition(&tools, name)?;
        assert_eq!(
            tool["_meta"]["io.libmcp/effect"]["state"],
            json!({"kind":"stateless"})
        );
        if name.starts_with("issue.") {
            assert_eq!(
                tool["inputSchema"]["properties"]["project"]["type"],
                json!("string")
            );
        }
    }
    let health = harness.call_tool(3, "system.health", json!({}))?;
    assert_tool_ok(&health);
    assert_eq!(
        tool_content(&health)["default_project"]["project_root"],
        json!(project_root.display().to_string())
    );
    let empty = harness.call_tool(4, "issue.list", json!({}))?;
    assert_tool_ok(&empty);
    assert_eq!(tool_content(&empty)["count"], json!(0));
    assert!(
        !tool_content(&empty)
            .as_object()
            .is_some_and(|fields| fields.contains_key("project_root"))
    );
    assert!(!state_path_for(&state_home, &project_root).exists());

    assert_tool_ok(&harness.call_tool(
        5,
        "issue.save",
        json!({
            "category":"bug", "slug":"same", "body":"default"
        }),
    )?);
    let save = harness.call_tool(
        6,
        "issue.save",
        json!({
            "project":marker, "category":"bug", "slug":"same", "body":"override"
        }),
    )?;
    assert_tool_ok(&save);
    assert_eq!(
        tool_content(&save)["project_root"],
        json!(other_root.display().to_string())
    );

    let read = harness.call_tool_full(
        7,
        "issue.read",
        json!({
            "project":other_nested, "category":"bug", "slug":"same"
        }),
    )?;
    assert_tool_ok(&read);
    assert_eq!(tool_content(&read)["body"], json!("override"));
    assert_eq!(
        tool_content(&read)["project_root"],
        json!(other_root.display().to_string())
    );
    let porcelain = harness.call_tool(8, "issue.read", json!({
        "project":other_root.display().to_string(), "category":"bug", "slug":"same", "render":"porcelain", "detail":"full"
    }))?;
    assert_tool_ok(&porcelain);
    assert!(porcelain["result"].get("structuredContent").is_none());
    assert_eq!(read["result"]["content"], json!([]));
    assert_eq!(
        porcelain["result"]["content"][0]["text"],
        json!(libmcp::render_json_porcelain(tool_content(&read)))
    );

    let list = harness.call_tool(
        9,
        "issue.list",
        json!({
            "project":other_root.display().to_string(), "view":"epics"
        }),
    )?;
    assert_tool_ok(&list);
    assert_eq!(
        tool_content(&list)["project_root"],
        json!(other_root.display().to_string())
    );
    let delete = harness.call_tool(
        10,
        "issue.delete",
        json!({
            "project":other_root.display().to_string(), "category":"bug", "slug":"same"
        }),
    )?;
    assert_tool_ok(&delete);
    assert_eq!(
        tool_content(&delete)["project_root"],
        json!(other_root.display().to_string())
    );
    let default_read = harness.call_tool(
        11,
        "issue.read",
        json!({
            "category":"bug", "slug":"same"
        }),
    )?;
    assert_tool_ok(&default_read);
    assert_eq!(tool_content(&default_read)["body"], json!("default"));
    let health = harness.call_tool(12, "system.health", json!({}))?;
    assert_eq!(
        tool_content(&health)["default_project"]["project_root"],
        json!(project_root.display().to_string())
    );
    Ok(())
}

#[test]
fn linked_worktree_calls_use_git_common_dir_identity() -> TestResult {
    let primary_root = temp_project_root("linked_worktree")?;
    let linked_root = primary_root.join(".worktrees/touching-demands");
    let state_home = primary_root.join("state-home");
    must(fs::create_dir_all(&state_home), "create state home")?;
    install_linked_worktree_gitdir(&primary_root, &linked_root)?;
    let legacy_issue_path =
        state_path_for(&state_home, &linked_root).join("issues/feature/legacy-worktree.md");
    must_some(legacy_issue_path.parent(), "legacy issue parent")
        .and_then(|parent| must(fs::create_dir_all(parent), "create legacy issue parent"))?;
    must(
        fs::write(&legacy_issue_path, "legacy linked-worktree body"),
        "write legacy issue",
    )?;

    let mut harness = McpHarness::spawn(Some(&primary_root), &state_home, &[])?;
    let _ = harness.initialize()?;
    harness.notify_initialized()?;
    let primary_state_root = state_path_for(&state_home, &primary_root);
    assert_tool_ok(&harness.call_tool(
        3,
        "issue.save",
        json!({
            "category":"bug", "slug":"common-dir", "body":"linked worktrees share notes"
        }),
    )?);
    let list = harness.call_tool(4, "issue.list", json!({"project":linked_root}))?;
    assert_tool_ok(&list);
    assert_eq!(tool_content(&list)["count"], json!(2));
    assert!(tool_content(&list).get("project_root").is_none());
    assert!(!legacy_issue_path.exists());
    assert_eq!(
        must(
            fs::read_to_string(primary_state_root.join("issues/feature/legacy-worktree.md")),
            "read migrated issue"
        )?,
        "legacy linked-worktree body"
    );
    let read = harness.call_tool(
        5,
        "issue.read",
        json!({
            "category":"feature", "slug":"legacy-worktree"
        }),
    )?;
    assert_tool_ok(&read);
    assert_eq!(
        tool_content(&read)["body"],
        json!("legacy linked-worktree body")
    );
    Ok(())
}

#[test]
fn save_read_delete_roundtrip_through_state_backed_issue_dir() -> TestResult {
    let project_root = temp_project_root("roundtrip")?;
    let state_home = project_root.join("state-home");
    must(fs::create_dir_all(&state_home), "create state home")?;
    let mut harness = McpHarness::spawn(None, &state_home, &[])?;
    let _ = harness.initialize()?;
    harness.notify_initialized()?;

    let state_root = state_path_for(&state_home, &project_root);

    let body = "# Feral Machine\n\nMake note parking brutally small.";
    let save = harness.call_tool(
        3,
        "issue.save",
        json!({
            "category": "feature",
            "slug": "feral-machine",
            "body": body,
        }),
    )?;
    assert_tool_ok(&save);
    assert_eq!(tool_content(&save)["category"].as_str(), Some("feature"));
    assert_eq!(
        tool_content(&save)["path"].as_str(),
        Some("issues/feature/feral-machine.md")
    );

    let saved_path = state_root
        .join("issues")
        .join("feature")
        .join("feral-machine.md");
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
    assert_eq!(
        tool_content(&list)["issues"][0]["category"].as_str(),
        Some("feature")
    );
    assert!(tool_content(&list)["issues"][0].get("body").is_none());

    let read = harness.call_tool_full(
        5,
        "issue.read",
        json!({
            "category": "feature",
            "slug": "feral-machine",
        }),
    )?;
    assert_tool_ok(&read);
    assert_eq!(tool_content(&read)["category"].as_str(), Some("feature"));
    assert_eq!(tool_content(&read)["body"].as_str(), Some(body));
    assert_eq!(
        tool_content(&read)["path"].as_str(),
        Some("issues/feature/feral-machine.md")
    );

    // Exercise the actual wire boundary: choosing text must neither select
    // fewer facts nor duplicate the structured payload.
    let porcelain = harness.request(&json!({
        "jsonrpc": "2.0", "id": 50, "method": "tools/call",
        "params": {"name": "issue.read", "arguments": {
            "category": "feature", "slug": "feral-machine", "detail": "full"
        }}
    }))?;
    assert_tool_ok(&porcelain);
    assert!(porcelain["result"].get("structuredContent").is_none());
    assert_eq!(read["result"]["content"], json!([]));
    assert_eq!(
        porcelain["result"]["content"][0]["text"],
        json!(libmcp::render_json_porcelain(tool_content(&read)))
    );

    let delete = harness.call_tool_full(
        6,
        "issue.delete",
        json!({
            "category": "feature",
            "slug": "feral-machine",
        }),
    )?;
    assert_tool_ok(&delete);
    assert_eq!(tool_content(&delete)["category"].as_str(), Some("feature"));
    assert_eq!(tool_content(&delete)["status"].as_str(), Some("deleted"));
    assert_eq!(
        tool_content(&delete)["path"].as_str(),
        Some("issues/feature/feral-machine.md")
    );
    assert_eq!(
        tool_content(&delete)["bytes"].as_u64(),
        Some(body.len() as u64)
    );
    assert!(!saved_path.exists());

    let drained = harness.call_tool(7, "issue.list", json!({}))?;
    assert_tool_ok(&drained);
    assert_eq!(tool_content(&drained)["count"].as_u64(), Some(0));

    let missing = harness.call_tool(
        8,
        "issue.read",
        json!({
            "category": "feature",
            "slug": "feral-machine",
        }),
    )?;
    assert_tool_error(&missing);
    assert_eq!(tool_content(&missing)["code"], json!("invalid_input"));
    assert!(error_message(&missing).contains("feature/feral-machine"));
    assert_eq!(missing["result"]["content"], json!([]));
    let porcelain_error = harness.call_tool(
        80,
        "issue.read",
        json!({
            "category":"feature", "slug":"feral-machine", "render":"porcelain"
        }),
    )?;
    assert_tool_error(&porcelain_error);
    assert!(porcelain_error["result"].get("structuredContent").is_none());
    assert_eq!(
        porcelain_error["result"]["content"][0]["text"],
        json!(libmcp::render_json_porcelain(tool_content(&missing)))
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
            .any(|event| event["event"] == "tool_call" && event["tool_name"] == "issue.delete"),
        "expected issue.delete tool_call event: {events:#?}"
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
fn legacy_uncategorized_issue_files_do_not_poison_listing() -> TestResult {
    let project_root = temp_project_root("legacy_uncategorized")?;
    let state_home = project_root.join("state-home");
    must(fs::create_dir_all(&state_home), "create state home")?;
    let mut harness = McpHarness::spawn(None, &state_home, &[])?;
    let _ = harness.initialize()?;
    harness.notify_initialized()?;

    let state_root = state_path_for(&state_home, &project_root);
    must(
        fs::create_dir_all(state_root.join("issues")),
        "create legacy issues directory",
    )?;
    let legacy_path = state_root.join("issues").join("legacy-root-file.md");
    must(
        fs::write(&legacy_path, "old uncategorized body"),
        "write legacy root issue",
    )?;

    let list = harness.call_tool(4, "issue.list", json!({}))?;
    assert_tool_ok(&list);
    assert_eq!(tool_content(&list)["count"].as_u64(), Some(0));
    Ok(())
}

#[test]
fn issue_category_is_closed_and_mandatory() -> TestResult {
    let project_root = temp_project_root("category_contract")?;
    let state_home = project_root.join("state-home");
    must(fs::create_dir_all(&state_home), "create state home")?;
    let mut harness = McpHarness::spawn(None, &state_home, &[])?;
    let _ = harness.initialize()?;
    harness.notify_initialized()?;

    let missing_category = harness.call_tool(
        3,
        "issue.save",
        json!({
            "slug": "missing-category",
            "body": "body",
        }),
    )?;
    assert_tool_error(&missing_category);
    assert!(error_message(&missing_category).contains("missing field `category`"));

    let invalid_category = harness.call_tool(
        4,
        "issue.save",
        json!({
            "category": "chore",
            "slug": "wrong-category",
            "body": "body",
        }),
    )?;
    assert_tool_error(&invalid_category);
    assert!(error_message(&invalid_category).contains("expected `feature` or `bug`"));
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
            "category": "bug",
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
fn uncertain_issue_save_is_never_replayed() -> TestResult {
    let project_root = temp_project_root("never_replay")?;
    let state_home = project_root.join("state-home");
    must(fs::create_dir_all(&state_home), "create state home")?;
    let mut harness = McpHarness::spawn(
        Some(&project_root),
        &state_home,
        &[(
            "JIRA_AT_HOME_MCP_TEST_HOST_CRASH_ONCE_KEY",
            "tools/call:issue.save",
        )],
    )?;
    let _ = harness.initialize()?;
    harness.notify_initialized()?;

    let state_root = state_path_for(&state_home, &project_root);

    let save = harness.call_tool(
        3,
        "issue.save",
        json!({
            "category": "bug",
            "slug": "uncertain-effect",
            "body": "this write must never be dispatched twice",
        }),
    )?;
    assert_tool_error(&save);
    assert_eq!(tool_content(&save)["code"], json!("ambiguous_outcome"));

    let telemetry = harness.call_tool_full(4, "system.telemetry", json!({}))?;
    assert_tool_ok(&telemetry);
    let totals = &tool_content(&telemetry)["telemetry"]["totals"];
    assert_eq!(totals["error_count"].as_u64(), Some(1));
    assert_eq!(totals["response_error_count"].as_u64(), Some(0));
    assert_eq!(totals["recovery_error_count"].as_u64(), Some(1));
    assert_eq!(totals["recovery_fault_count"].as_u64(), Some(1));
    assert_eq!(totals["retry_count"].as_u64(), Some(0));
    assert_eq!(
        tool_content(&telemetry)["telemetry"]["restart_count"].as_u64(),
        Some(1)
    );

    let list = harness.call_tool(5, "issue.list", json!({}))?;
    assert_tool_ok(&list);
    assert!(
        tool_content(&list)["count"]
            .as_u64()
            .is_some_and(|count| count <= 1),
        "an uncertain save must not create duplicate effects: {list:#}"
    );

    let events = must(
        read_json_lines::<Value>(&state_root.join("mcp").join("telemetry.jsonl")),
        "read telemetry log",
    )?;
    let save_event = must_some(
        events
            .iter()
            .find(|event| event["event"] == "tool_call" && event["tool_name"] == "issue.save"),
        "ambiguous issue.save telemetry event",
    )?;
    assert_eq!(save_event["replay_attempts"].as_u64(), Some(0));
    assert_eq!(save_event["outcome"].as_str(), Some("error"));
    assert_eq!(save_event["error_kind"].as_str(), Some("ambiguous_outcome"));
    Ok(())
}

#[test]
fn host_rollout_reexec_preserves_session() -> TestResult {
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
            "category": "feature",
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
    assert_eq!(
        tool_content(&health)["default_project"]["project_root"],
        json!(project_root.display().to_string())
    );

    let read = harness.call_tool(
        5,
        "issue.read",
        json!({
            "category": "feature",
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

#[test]
fn saving_a_slug_under_a_new_epic_relocates_it_and_list_filters_by_epic() -> TestResult {
    let project_root = temp_project_root("epic_relocation")?;
    let state_home = project_root.join("state-home");
    must(fs::create_dir_all(&state_home), "create state home")?;
    let mut harness = McpHarness::spawn(None, &state_home, &[])?;
    let _ = harness.initialize()?;
    harness.notify_initialized()?;

    let state_root = state_path_for(&state_home, &project_root);
    let issues_root = state_root.join("issues").join("feature");

    let first_save = harness.call_tool(
        3,
        "issue.save",
        json!({
            "category": "feature",
            "slug": "shared-slug",
            "epic": "alpha",
            "body": "alpha body",
        }),
    )?;
    assert_tool_ok(&first_save);
    assert_eq!(
        tool_content(&first_save)["status"].as_str(),
        Some("created")
    );
    assert!(issues_root.join("alpha").join("shared-slug.md").is_file());

    // Re-saving the same category/slug under a different epic must move the
    // file (not duplicate it) and prune the now-empty `alpha` directory.
    let moved_save = harness.call_tool(
        4,
        "issue.save",
        json!({
            "category": "feature",
            "slug": "shared-slug",
            "epic": "beta",
            "body": "beta body",
        }),
    )?;
    assert_tool_ok(&moved_save);
    assert_eq!(tool_content(&moved_save)["status"].as_str(), Some("moved"));
    assert_eq!(tool_content(&moved_save)["epic"].as_str(), Some("beta"));
    assert!(!issues_root.join("alpha").exists());
    assert!(issues_root.join("beta").join("shared-slug.md").is_file());

    let plain_save = harness.call_tool(
        5,
        "issue.save",
        json!({
            "category": "feature",
            "slug": "plain",
            "body": "no epic here",
        }),
    )?;
    assert_tool_ok(&plain_save);
    assert!(tool_content(&plain_save)["epic"].is_null());

    // issue.read resolves the slug without being told which epic it lives in.
    let read = harness.call_tool(
        6,
        "issue.read",
        json!({ "category": "feature", "slug": "shared-slug" }),
    )?;
    assert_tool_ok(&read);
    assert_eq!(tool_content(&read)["epic"].as_str(), Some("beta"));
    assert_eq!(tool_content(&read)["body"].as_str(), Some("beta body"));

    let unfiltered = harness.call_tool(7, "issue.list", json!({"limit":1}))?;
    assert_tool_ok(&unfiltered);
    assert_eq!(tool_content(&unfiltered)["count"].as_u64(), Some(2));
    assert_eq!(tool_content(&unfiltered)["next_offset"], json!(1));
    assert_eq!(
        tool_content(&unfiltered)["issues"].as_array().map(Vec::len),
        Some(1)
    );
    let second = harness.call_tool(70, "issue.list", json!({"limit":1,"offset":1}))?;
    assert!(tool_content(&second)["next_offset"].is_null());
    assert_eq!(
        tool_content(&second)["issues"][0]["slug"],
        json!("shared-slug")
    );
    let epics = harness.call_tool(71, "issue.list", json!({"view":"epics"}))?;
    assert_eq!(tool_content(&epics)["epics"], json!(["beta"]));

    let named_filter = harness.call_tool(8, "issue.list", json!({ "epic": "beta" }))?;
    assert_tool_ok(&named_filter);
    assert_eq!(tool_content(&named_filter)["count"].as_u64(), Some(1));
    assert_eq!(
        tool_content(&named_filter)["issues"][0]["slug"].as_str(),
        Some("shared-slug")
    );

    let no_epic_filter = harness.call_tool(9, "issue.list", json!({ "epic": "" }))?;
    assert_tool_ok(&no_epic_filter);
    assert_eq!(tool_content(&no_epic_filter)["count"].as_u64(), Some(1));
    assert_eq!(
        tool_content(&no_epic_filter)["issues"][0]["slug"].as_str(),
        Some("plain")
    );

    // issue.delete also resolves without an epic argument, and prunes the
    // epic directory it emptied.
    let delete = harness.call_tool(
        10,
        "issue.delete",
        json!({ "category": "feature", "slug": "shared-slug" }),
    )?;
    assert_tool_ok(&delete);
    assert!(!issues_root.join("beta").exists());

    let after_delete = harness.call_tool(11, "issue.list", json!({}))?;
    assert_tool_ok(&after_delete);
    assert_eq!(tool_content(&after_delete)["count"].as_u64(), Some(1));
    let epics = harness.call_tool(12, "issue.list", json!({"view":"epics"}))?;
    assert_eq!(tool_content(&epics)["epics"], json!([]));
    Ok(())
}

fn save_feature(harness: &mut McpHarness, id: u64, slug: &str, body: &str) -> TestResult<Value> {
    harness.call_tool(
        id,
        "issue.save",
        json!({ "category": "feature", "slug": slug, "body": body }),
    )
}

#[test]
fn front_matter_dependencies_are_validated_and_released_on_delete() -> TestResult {
    let project_root = temp_project_root("front_matter")?;
    let state_home = project_root.join("state-home");
    must(fs::create_dir_all(&state_home), "create state home")?;
    let mut harness = McpHarness::spawn(None, &state_home, &[])?;
    let _ = harness.initialize()?;
    harness.notify_initialized()?;

    let base = save_feature(&mut harness, 3, "base", "+++\neffort = 2\n+++\nfoundation")?;
    assert_tool_ok(&base);
    assert_eq!(tool_content(&base)["effort"], json!(2));
    let top_body = "+++\ndepends_on = [\"feature/base\"]\neffort = 4\n+++\nbuilt on base";
    let top = save_feature(&mut harness, 4, "top", top_body)?;
    assert_tool_ok(&top);
    assert_eq!(tool_content(&top)["dependencies"], json!(1));

    for (id, slug, body) in [
        (5, "base", "+++\ndepends_on = [\"feature/top\"]\n+++\ncycle"),
        (
            6,
            "orphan",
            "+++\ndepends_on = [\"feature/missing\"]\n+++\nunknown",
        ),
        (
            7,
            "itself",
            "+++\ndepends_on = [\"feature/itself\"]\n+++\nself",
        ),
        (8, "odd", "+++\neffort = 3\n+++\noff the ladder"),
        (9, "typo", "+++\ndepend_on = []\n+++\nunknown key"),
        (10, "open", "+++\neffort = 1\nno closing fence"),
    ] {
        assert_tool_error(&save_feature(&mut harness, id, slug, body)?);
    }

    let list = harness.call_tool(11, "issue.list", json!({}))?;
    assert_eq!(tool_content(&list)["count"], json!(2));
    assert_eq!(tool_content(&list)["issues"][0]["effort"], json!(2));
    assert_eq!(tool_content(&list)["issues"][1]["dependencies"], json!(1));
    let read = harness.call_tool(
        12,
        "issue.read",
        json!({ "category": "feature", "slug": "base" }),
    )?;
    assert_eq!(tool_content(&read)["dependents"], json!(["feature/top"]));

    // Deleting means done: the dependency leaves the dependent's front matter.
    let delete = harness.call_tool(
        13,
        "issue.delete",
        json!({ "category": "feature", "slug": "base" }),
    )?;
    assert_eq!(tool_content(&delete)["dependents"], json!(["feature/top"]));
    let released = harness.call_tool(
        14,
        "issue.read",
        json!({ "category": "feature", "slug": "top" }),
    )?;
    assert_eq!(
        tool_content(&released)["body"],
        json!("+++\neffort = 4\n+++\nbuilt on base")
    );
    Ok(())
}
