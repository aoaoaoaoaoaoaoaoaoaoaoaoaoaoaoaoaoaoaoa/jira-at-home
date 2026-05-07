use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use libmcp::{Generation, SurfaceKind};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::mcp::fault::{FaultRecord, FaultStage};
use crate::mcp::output::{
    ToolOutput, fallback_detailed_tool_output, split_presentation, tool_success,
};
use crate::store::{
    DeleteReceipt, IssueBody, IssueCategory, IssueKey, IssueRecord, IssueSlug, IssueStore,
    SaveReceipt, StoreError, format_timestamp,
};

pub(crate) fn run_worker(
    project_root: PathBuf,
    generation: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let generation = generation_from_wire(generation);
    let store = IssueStore::bind(project_root)?;
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    let mut service = WorkerService::new(store, generation);

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request = serde_json::from_str::<crate::mcp::protocol::WorkerRequest>(&line)?;
        let response = match request {
            crate::mcp::protocol::WorkerRequest::Execute { id, operation } => {
                let outcome = match service.execute(operation) {
                    Ok(result) => crate::mcp::protocol::WorkerOutcome::Success { result },
                    Err(fault) => crate::mcp::protocol::WorkerOutcome::Fault { fault },
                };
                crate::mcp::protocol::WorkerResponse { id, outcome }
            }
        };
        serde_json::to_writer(&mut stdout, &response)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }

    Ok(())
}

struct WorkerService {
    store: IssueStore,
    generation: Generation,
}

impl WorkerService {
    fn new(store: IssueStore, generation: Generation) -> Self {
        Self { store, generation }
    }

    fn execute(
        &mut self,
        operation: crate::mcp::protocol::WorkerOperation,
    ) -> Result<Value, FaultRecord> {
        match operation {
            crate::mcp::protocol::WorkerOperation::CallTool { name, arguments } => {
                self.call_tool(&name, arguments)
            }
        }
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value, FaultRecord> {
        let operation = format!("tools/call:{name}");
        let (presentation, arguments) =
            split_presentation(arguments, &operation, self.generation, FaultStage::Worker)?;
        let output = match name {
            "issue.save" => {
                let args = deserialize::<IssueSaveArgs>(arguments, &operation, self.generation)?;
                let key = parse_key(args.category, args.slug)
                    .map_err(store_fault(self.generation, &operation))?;
                let body = IssueBody::parse(args.body)
                    .map_err(store_fault(self.generation, &operation))?;
                let receipt = self
                    .store
                    .save(key, body)
                    .map_err(store_fault(self.generation, &operation))?;
                issue_save_output(
                    &receipt,
                    self.store.layout().state_root.as_path(),
                    self.generation,
                    &operation,
                )?
            }
            "issue.delete" => {
                let args = deserialize::<IssueKeyArgs>(arguments, &operation, self.generation)?;
                let key = parse_key(args.category, args.slug)
                    .map_err(store_fault(self.generation, &operation))?;
                let receipt = self
                    .store
                    .delete(key)
                    .map_err(store_fault(self.generation, &operation))?;
                issue_delete_output(
                    &receipt,
                    self.store.layout().state_root.as_path(),
                    self.generation,
                    &operation,
                )?
            }
            "issue.list" => {
                let issues = self
                    .store
                    .list()
                    .map_err(store_fault(self.generation, &operation))?;
                issue_list_output(
                    &issues,
                    self.store.layout().state_root.as_path(),
                    self.generation,
                    &operation,
                )?
            }
            "issue.read" => {
                let args = deserialize::<IssueKeyArgs>(arguments, &operation, self.generation)?;
                let key = parse_key(args.category, args.slug)
                    .map_err(store_fault(self.generation, &operation))?;
                let record = self
                    .store
                    .read(key)
                    .map_err(store_fault(self.generation, &operation))?;
                issue_read_output(
                    &record,
                    self.store.layout().state_root.as_path(),
                    self.generation,
                    &operation,
                )?
            }
            other => {
                return Err(FaultRecord::invalid_input(
                    self.generation,
                    FaultStage::Worker,
                    &operation,
                    format!("unknown worker tool `{other}`"),
                ));
            }
        };
        tool_success(
            output,
            presentation,
            self.generation,
            FaultStage::Worker,
            &operation,
        )
    }
}

#[derive(Debug, Deserialize)]
struct IssueSaveArgs {
    category: String,
    slug: String,
    body: String,
}

#[derive(Debug, Deserialize)]
struct IssueKeyArgs {
    category: String,
    slug: String,
}

fn deserialize<T: for<'de> Deserialize<'de>>(
    value: Value,
    operation: &str,
    generation: Generation,
) -> Result<T, FaultRecord> {
    serde_json::from_value(value).map_err(|error| {
        FaultRecord::invalid_input(
            generation,
            FaultStage::Protocol,
            operation,
            format!("invalid params: {error}"),
        )
    })
}

fn parse_key(category: String, slug: String) -> Result<IssueKey, StoreError> {
    Ok(IssueKey::new(
        IssueCategory::parse(category)?,
        IssueSlug::parse(slug)?,
    ))
}

fn store_fault(
    generation: Generation,
    operation: &str,
) -> impl FnOnce(StoreError) -> FaultRecord + '_ {
    move |error| {
        let stage = if matches!(error, StoreError::Io(_)) {
            FaultStage::Store
        } else {
            FaultStage::Worker
        };
        match error {
            StoreError::InvalidCategory(_)
            | StoreError::InvalidSlug(_)
            | StoreError::EmptyIssueBody
            | StoreError::IssueNotFound(_)
            | StoreError::MalformedIssueEntry(_, _)
            | StoreError::MissingProjectPath(_)
            | StoreError::ProjectPathNotDirectory(_) => {
                FaultRecord::invalid_input(generation, stage, operation, error.to_string())
            }
            StoreError::Io(_) => {
                FaultRecord::internal(generation, stage, operation, error.to_string())
            }
        }
    }
}

fn issue_save_output(
    receipt: &SaveReceipt,
    state_root: &Path,
    generation: Generation,
    operation: &str,
) -> Result<ToolOutput, FaultRecord> {
    let relative_path = relative_issue_path(&receipt.path, state_root);
    let status = if receipt.created {
        "created"
    } else {
        "updated"
    };
    let updated_at = format_timestamp(receipt.updated_at);
    let concise = issue_key_payload(
        &receipt.key,
        [
            ("status", json!(status)),
            ("path", json!(relative_path)),
            ("updated_at", json!(updated_at)),
        ],
    );
    let full = issue_key_payload(
        &receipt.key,
        [
            ("status", json!(status)),
            (
                "path",
                json!(relative_issue_path(&receipt.path, state_root)),
            ),
            ("updated_at", json!(format_timestamp(receipt.updated_at))),
            ("bytes", json!(receipt.bytes)),
        ],
    );
    fallback_detailed_tool_output(
        &concise,
        &full,
        [
            format!("saved issue {}", receipt.key),
            format!("status: {status}"),
            format!("path: {}", relative_issue_path(&receipt.path, state_root)),
            format!("updated: {}", format_timestamp(receipt.updated_at)),
        ]
        .join("\n"),
        None,
        SurfaceKind::Mutation,
        generation,
        FaultStage::Worker,
        operation,
    )
}

fn issue_delete_output(
    receipt: &DeleteReceipt,
    state_root: &Path,
    generation: Generation,
    operation: &str,
) -> Result<ToolOutput, FaultRecord> {
    let relative_path = relative_issue_path(&receipt.path, state_root);
    let deleted_at = format_timestamp(receipt.deleted_at);
    let concise = issue_key_payload(
        &receipt.key,
        [
            ("status", json!("deleted")),
            ("path", json!(relative_path)),
            ("deleted_at", json!(deleted_at.clone())),
        ],
    );
    let full = issue_key_payload(
        &receipt.key,
        [
            ("status", json!("deleted")),
            (
                "path",
                json!(relative_issue_path(&receipt.path, state_root)),
            ),
            ("deleted_at", json!(format_timestamp(receipt.deleted_at))),
            ("bytes", json!(receipt.bytes)),
        ],
    );
    fallback_detailed_tool_output(
        &concise,
        &full,
        [
            format!("deleted issue {}", receipt.key),
            format!("path: {}", relative_issue_path(&receipt.path, state_root)),
            format!("deleted: {deleted_at}"),
        ]
        .join("\n"),
        None,
        SurfaceKind::Mutation,
        generation,
        FaultStage::Worker,
        operation,
    )
}

fn issue_list_output(
    issues: &[crate::store::IssueSummary],
    state_root: &Path,
    generation: Generation,
    operation: &str,
) -> Result<ToolOutput, FaultRecord> {
    let concise_items = issues
        .iter()
        .map(|issue| {
            issue_key_payload(
                &issue.key,
                [("updated_at", json!(format_timestamp(issue.updated_at)))],
            )
        })
        .collect::<Vec<_>>();
    let full_items = issues
        .iter()
        .map(|issue| {
            issue_key_payload(
                &issue.key,
                [
                    ("path", json!(relative_issue_path(&issue.path, state_root))),
                    ("updated_at", json!(format_timestamp(issue.updated_at))),
                ],
            )
        })
        .collect::<Vec<_>>();
    let mut lines = vec![format!("{} issue(s)", issues.len())];
    lines.extend(issues.iter().map(|issue| issue.key.to_string()));
    fallback_detailed_tool_output(
        &json!({ "count": issues.len(), "issues": concise_items }),
        &json!({ "count": issues.len(), "issues": full_items }),
        lines.join("\n"),
        None,
        SurfaceKind::List,
        generation,
        FaultStage::Worker,
        operation,
    )
}

fn issue_read_output(
    record: &IssueRecord,
    state_root: &Path,
    generation: Generation,
    operation: &str,
) -> Result<ToolOutput, FaultRecord> {
    let relative_path = relative_issue_path(&record.path, state_root);
    let concise = issue_key_payload(
        &record.key,
        [
            ("updated_at", json!(format_timestamp(record.updated_at))),
            ("body", json!(&record.body)),
        ],
    );
    let full = issue_key_payload(
        &record.key,
        [
            ("path", json!(relative_path)),
            ("updated_at", json!(format_timestamp(record.updated_at))),
            ("bytes", json!(record.bytes)),
            ("body", json!(&record.body)),
        ],
    );
    let concise_text = format!(
        "issue {}\nupdated: {}\n\n{}",
        record.key,
        format_timestamp(record.updated_at),
        record.body,
    );
    let full_text = Some(format!(
        "issue {}\npath: {}\nupdated: {}\nbytes: {}\n\n{}",
        record.key,
        relative_issue_path(&record.path, state_root),
        format_timestamp(record.updated_at),
        record.bytes,
        record.body,
    ));
    fallback_detailed_tool_output(
        &concise,
        &full,
        concise_text,
        full_text,
        SurfaceKind::Read,
        generation,
        FaultStage::Worker,
        operation,
    )
}

fn issue_key_payload(
    key: &IssueKey,
    fields: impl IntoIterator<Item = (&'static str, Value)>,
) -> Value {
    let mut payload = Map::new();
    let _ = payload.insert("category".to_owned(), json!(key.category));
    let _ = payload.insert("slug".to_owned(), json!(&key.slug));
    payload.extend(
        fields
            .into_iter()
            .map(|(field, value)| (field.to_owned(), value)),
    );
    Value::Object(payload)
}

fn relative_issue_path(path: &Path, project_root: &Path) -> String {
    path.strip_prefix(project_root).map_or_else(
        |_| path.display().to_string(),
        |relative| relative.display().to_string(),
    )
}

fn generation_from_wire(raw: u64) -> Generation {
    let mut generation = Generation::genesis();
    for _ in 1..raw {
        generation = generation.next();
    }
    generation
}
