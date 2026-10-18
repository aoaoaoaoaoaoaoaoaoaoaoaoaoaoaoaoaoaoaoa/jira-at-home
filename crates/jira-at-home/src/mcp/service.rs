use std::io::{self, BufReader};
use std::path::{Path, PathBuf};

use libmcp::{FrameLimit, FrameReadOutcome, Generation, SurfaceKind, read_frame_blocking};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::mcp::fault::{FaultRecord, FaultStage};
use crate::mcp::output::{
    ToolOutput, fallback_detailed_tool_output, split_presentation, tool_success,
};
use crate::mcp::protocol::write_sync_json_frame;
use crate::mcp::selection::PageArgs;
use crate::store::{
    DeleteReceipt, Epic, IssueBody, IssueCategory, IssueKey, IssueRecord, IssueSlug, IssueStore,
    IssueSummary, ProjectLayout, SaveReceipt, StoreError, format_timestamp,
};

pub(crate) fn run_worker(generation: u64) -> Result<(), Box<dyn std::error::Error>> {
    let generation = Generation::try_new(generation)?;
    let stdin = io::stdin();
    let mut stdin = BufReader::new(stdin.lock());
    let mut stdout = io::stdout().lock();
    let default_project = std::env::current_dir()
        .map_err(StoreError::from)
        .and_then(ProjectLayout::resolve)
        .map_err(|error| format!("working directory: {error}"));
    let mut service = WorkerService::new(default_project, generation);

    while let FrameReadOutcome::Frame(payload) =
        read_frame_blocking(&mut stdin, FrameLimit::DEFAULT)?
    {
        let request = serde_json::from_slice::<crate::mcp::protocol::WorkerRequest>(&payload)?;
        let response = match request {
            crate::mcp::protocol::WorkerRequest::Execute { id, operation } => {
                let outcome = match service.execute(operation) {
                    Ok(result) => crate::mcp::protocol::WorkerOutcome::Success { result },
                    Err(fault) => crate::mcp::protocol::WorkerOutcome::Fault { fault },
                };
                crate::mcp::protocol::WorkerResponse { id, outcome }
            }
        };
        write_sync_json_frame(&mut stdout, &response, FrameLimit::DEFAULT)?;
    }

    Ok(())
}

struct WorkerService {
    /// The working directory's project, or why it has none.
    default_project: Result<ProjectLayout, String>,
    generation: Generation,
}

impl WorkerService {
    fn new(default_project: Result<ProjectLayout, String>, generation: Generation) -> Self {
        Self {
            default_project,
            generation,
        }
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
        let args = deserialize::<ProjectArgs>(arguments, &operation, self.generation)?;
        let layout = match args.project {
            Some(path) => {
                ProjectLayout::resolve(path).map_err(store_fault(self.generation, &operation))?
            }
            None => self.default_project.clone().map_err(|reason| {
                FaultRecord::invalid_input(
                    self.generation,
                    FaultStage::Worker,
                    &operation,
                    format!("{reason}; pass project"),
                )
            })?,
        };
        let store = IssueStore::open(layout).map_err(store_fault(self.generation, &operation))?;
        let arguments = Value::Object(args.arguments);
        let output = match name {
            "issue.save" => {
                let args = deserialize::<IssueSaveArgs>(arguments, &operation, self.generation)?;
                let key = parse_key(args.category, args.slug)
                    .map_err(store_fault(self.generation, &operation))?;
                let epic =
                    parse_epic(args.epic).map_err(store_fault(self.generation, &operation))?;
                let body = IssueBody::parse(args.body)
                    .map_err(store_fault(self.generation, &operation))?;
                let receipt = store
                    .save(key, epic, body)
                    .map_err(store_fault(self.generation, &operation))?;
                issue_save_output(
                    &receipt,
                    store.layout().state_root.as_path(),
                    self.generation,
                    &operation,
                )?
            }
            "issue.delete" => {
                let args = deserialize::<IssueKeyArgs>(arguments, &operation, self.generation)?;
                let key = parse_key(args.category, args.slug)
                    .map_err(store_fault(self.generation, &operation))?;
                let receipt = store
                    .delete(key)
                    .map_err(store_fault(self.generation, &operation))?;
                issue_delete_output(
                    &receipt,
                    store.layout().state_root.as_path(),
                    self.generation,
                    &operation,
                )?
            }
            "issue.list" => {
                let args = deserialize::<IssueListArgs>(arguments, &operation, self.generation)?;
                if args.view == ListView::Epics && args.epic.is_some() {
                    return Err(FaultRecord::invalid_input(
                        self.generation,
                        FaultStage::Worker,
                        &operation,
                        "epic filters issues; omit it with view=epics",
                    ));
                }
                let epic_filter = EpicFilter::parse(args.epic)
                    .map_err(store_fault(self.generation, &operation))?;
                let all_issues = store
                    .list()
                    .map_err(store_fault(self.generation, &operation))?;
                match args.view {
                    ListView::Issues => {
                        let issues = all_issues
                            .iter()
                            .filter(|issue| epic_filter.matches(issue.epic.as_ref()))
                            .collect::<Vec<_>>();
                        issue_list_output(
                            &issues,
                            &args.page,
                            store.layout().state_root.as_path(),
                            self.generation,
                            &operation,
                        )?
                    }
                    ListView::Epics => {
                        let epics = distinct_epics(&all_issues);
                        let page = args.page.select(&epics, 50);
                        let mut selected = page.metadata();
                        selected["epics"] = json!(page.items);
                        fallback_detailed_tool_output(
                            &selected,
                            &selected,
                            SurfaceKind::List,
                            self.generation,
                            FaultStage::Worker,
                            &operation,
                        )?
                    }
                }
            }
            "issue.read" => {
                let args = deserialize::<IssueKeyArgs>(arguments, &operation, self.generation)?;
                let key = parse_key(args.category, args.slug)
                    .map_err(store_fault(self.generation, &operation))?;
                let record = store
                    .read(key)
                    .map_err(store_fault(self.generation, &operation))?;
                issue_read_output(
                    &record,
                    store.layout().state_root.as_path(),
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
        let output = if self
            .default_project
            .as_ref()
            .is_ok_and(|default| default.project_root == store.layout().project_root)
        {
            output
        } else {
            output.with_project(&store.layout().project_root)
        };
        Ok(tool_success(&output, presentation))
    }
}

#[derive(Deserialize)]
struct ProjectArgs {
    project: Option<PathBuf>,
    #[serde(flatten)]
    arguments: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
struct IssueSaveArgs {
    category: String,
    slug: String,
    #[serde(default)]
    epic: Option<String>,
    body: String,
}

#[derive(Debug, Deserialize)]
struct IssueKeyArgs {
    category: String,
    slug: String,
}

#[derive(Deserialize)]
struct IssueListArgs {
    /// Absent: no filter. Empty string: issues with no epic. Otherwise: that
    /// epic only.
    #[serde(default)]
    epic: Option<String>,
    #[serde(default)]
    view: ListView,
    #[serde(flatten)]
    page: PageArgs,
}

#[derive(Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ListView {
    #[default]
    Issues,
    Epics,
}

fn deserialize<T: for<'de> Deserialize<'de>>(
    value: Value,
    operation: &str,
    generation: Generation,
) -> Result<T, FaultRecord> {
    serde_json::from_value(value).map_err(|error| {
        FaultRecord::invalid_input(
            generation,
            FaultStage::Worker,
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

fn parse_epic(raw: Option<String>) -> Result<Option<Epic>, StoreError> {
    raw.map(Epic::parse).transpose()
}

/// The `issue.list` `epic` argument: absent selects every issue, `""`
/// selects issues with no epic, and any other value selects that epic only.
enum EpicFilter {
    Any,
    NoEpic,
    Named(Epic),
}

impl EpicFilter {
    fn parse(raw: Option<String>) -> Result<Self, StoreError> {
        match raw {
            None => Ok(Self::Any),
            Some(raw) if raw.is_empty() => Ok(Self::NoEpic),
            Some(raw) => Epic::parse(raw).map(Self::Named),
        }
    }

    fn matches(&self, epic: Option<&Epic>) -> bool {
        match self {
            Self::Any => true,
            Self::NoEpic => epic.is_none(),
            Self::Named(wanted) => epic == Some(wanted),
        }
    }
}

fn distinct_epics(issues: &[IssueSummary]) -> Vec<&Epic> {
    let mut epics = issues
        .iter()
        .filter_map(|issue| issue.epic.as_ref())
        .collect::<Vec<_>>();
    epics.sort();
    epics.dedup();
    epics
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
            | StoreError::InvalidEpic(_)
            | StoreError::EmptyIssueBody
            | StoreError::InvalidFrontMatter(_)
            | StoreError::InvalidDependency(_)
            | StoreError::InvalidEffort(_)
            | StoreError::DuplicateDependency(_)
            | StoreError::SelfDependency(_)
            | StoreError::UnknownDependency(_)
            | StoreError::DependencyCycle { .. }
            | StoreError::IssueNotFound(_)
            | StoreError::MalformedIssueEntry(_, _)
            | StoreError::MalformedGitIndirection(_, _)
            | StoreError::LegacyMigrationConflict { .. }
            | StoreError::MissingProjectPath(_)
            | StoreError::ProjectPathNotDirectory(_) => {
                FaultRecord::invalid_input(generation, stage, operation, error.to_string())
            }
            StoreError::IssueTooLarge(_, _) => {
                FaultRecord::unavailable(generation, stage, operation, error.to_string())
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
    let status = receipt.outcome.as_str();
    let updated_at = format_timestamp(receipt.updated_at);
    let concise = issue_key_payload(
        &receipt.key,
        receipt.epic.as_ref(),
        [
            ("status", json!(status)),
            ("path", json!(relative_path)),
            ("updated_at", json!(updated_at)),
            ("effort", json!(receipt.front.effort)),
            ("dependencies", json!(receipt.front.depends_on.len())),
        ],
    );
    let full = issue_key_payload(
        &receipt.key,
        receipt.epic.as_ref(),
        [
            ("status", json!(status)),
            (
                "path",
                json!(relative_issue_path(&receipt.path, state_root)),
            ),
            ("updated_at", json!(format_timestamp(receipt.updated_at))),
            ("bytes", json!(receipt.bytes)),
            ("effort", json!(receipt.front.effort)),
            ("dependencies", json!(receipt.front.depends_on.len())),
            ("depends_on", key_list(&receipt.front.depends_on)),
        ],
    );
    fallback_detailed_tool_output(
        &concise,
        &full,
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
        receipt.epic.as_ref(),
        [
            ("status", json!("deleted")),
            ("path", json!(relative_path)),
            ("deleted_at", json!(deleted_at.clone())),
            ("dependents", key_list(&receipt.dependents)),
        ],
    );
    let full = issue_key_payload(
        &receipt.key,
        receipt.epic.as_ref(),
        [
            ("status", json!("deleted")),
            (
                "path",
                json!(relative_issue_path(&receipt.path, state_root)),
            ),
            ("deleted_at", json!(format_timestamp(receipt.deleted_at))),
            ("bytes", json!(receipt.bytes)),
            ("dependents", key_list(&receipt.dependents)),
        ],
    );
    fallback_detailed_tool_output(
        &concise,
        &full,
        SurfaceKind::Mutation,
        generation,
        FaultStage::Worker,
        operation,
    )
}

fn issue_list_output(
    issues: &[&IssueSummary],
    args: &PageArgs,
    state_root: &Path,
    generation: Generation,
    operation: &str,
) -> Result<ToolOutput, FaultRecord> {
    let page = args.select(issues, 50);
    let concise_items = page
        .items
        .iter()
        .map(|issue| {
            issue_key_payload(
                &issue.key,
                issue.epic.as_ref(),
                [
                    ("effort", json!(issue.front.effort)),
                    ("dependencies", json!(issue.front.depends_on.len())),
                ],
            )
        })
        .collect::<Vec<_>>();
    let full_items = page
        .items
        .iter()
        .map(|issue| {
            issue_key_payload(
                &issue.key,
                issue.epic.as_ref(),
                [
                    ("effort", json!(issue.front.effort)),
                    ("dependencies", json!(issue.front.depends_on.len())),
                    ("depends_on", key_list(&issue.front.depends_on)),
                    ("path", json!(relative_issue_path(&issue.path, state_root))),
                    ("updated_at", json!(format_timestamp(issue.updated_at))),
                ],
            )
        })
        .collect::<Vec<_>>();
    let mut concise = page.metadata();
    concise["issues"] = json!(concise_items);
    let mut full = page.metadata();
    full["issues"] = json!(full_items);
    fallback_detailed_tool_output(
        &concise,
        &full,
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
        record.epic.as_ref(),
        [
            ("updated_at", json!(format_timestamp(record.updated_at))),
            ("dependents", key_list(&record.dependents)),
            ("body", json!(&record.body)),
        ],
    );
    let full = issue_key_payload(
        &record.key,
        record.epic.as_ref(),
        [
            ("path", json!(relative_path)),
            ("updated_at", json!(format_timestamp(record.updated_at))),
            ("bytes", json!(record.bytes)),
            ("dependents", key_list(&record.dependents)),
            ("body", json!(&record.body)),
        ],
    );
    fallback_detailed_tool_output(
        &concise,
        &full,
        SurfaceKind::Read,
        generation,
        FaultStage::Worker,
        operation,
    )
}

fn issue_key_payload(
    key: &IssueKey,
    epic: Option<&Epic>,
    fields: impl IntoIterator<Item = (&'static str, Value)>,
) -> Value {
    let mut payload = Map::new();
    payload.insert("category".to_owned(), json!(key.category));
    payload.insert("slug".to_owned(), json!(&key.slug));
    payload.insert("epic".to_owned(), json!(epic));
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

/// Issue keys in the `category/slug` form that front matter uses.
fn key_list(keys: &[IssueKey]) -> Value {
    json!(keys.iter().map(ToString::to_string).collect::<Vec<_>>())
}
