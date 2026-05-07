use libmcp::ReplayContract;
use serde_json::{Map, Value, json};

use crate::mcp::output::with_common_presentation;
use crate::store::IssueCategory;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DispatchTarget {
    Host,
    Worker,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ToolSpec {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) dispatch: DispatchTarget,
    pub(crate) replay: ReplayContract,
}

impl ToolSpec {
    fn annotation_json(self) -> Value {
        json!({
            "title": self.name,
            "readOnlyHint": self.replay == ReplayContract::Convergent,
            "destructiveHint": self.replay == ReplayContract::NeverReplay,
            "jiraAtHome": {
                "dispatch": match self.dispatch {
                    DispatchTarget::Host => "host",
                    DispatchTarget::Worker => "worker",
                },
                "replayContract": match self.replay {
                    ReplayContract::Convergent => "convergent",
                    ReplayContract::ProbeRequired => "probe_required",
                    ReplayContract::NeverReplay => "never_replay",
                },
            }
        })
    }
}

const TOOL_SPECS: &[ToolSpec] = &[
    ToolSpec {
        name: "project.bind",
        description: "Bind this MCP session to a project root or a nested path inside one.",
        dispatch: DispatchTarget::Host,
        replay: ReplayContract::NeverReplay,
    },
    ToolSpec {
        name: "issue.save",
        description: "Create or overwrite one categorized issue note at `issues/<category>/<slug>.md` under the bound project's external state root.",
        dispatch: DispatchTarget::Worker,
        replay: ReplayContract::NeverReplay,
    },
    ToolSpec {
        name: "issue.delete",
        description: "Delete one issue note by category and slug from the bound project's external state root.",
        dispatch: DispatchTarget::Worker,
        replay: ReplayContract::NeverReplay,
    },
    ToolSpec {
        name: "issue.list",
        description: "List the currently parked issues across the closed category set `feature | bug`. There is no close state; deleting an issue removes its file entirely.",
        dispatch: DispatchTarget::Worker,
        replay: ReplayContract::Convergent,
    },
    ToolSpec {
        name: "issue.read",
        description: "Read one issue note by category and slug.",
        dispatch: DispatchTarget::Worker,
        replay: ReplayContract::Convergent,
    },
    ToolSpec {
        name: "system.health",
        description: "Read MCP host health, binding state, worker generation, and rollout state.",
        dispatch: DispatchTarget::Host,
        replay: ReplayContract::Convergent,
    },
    ToolSpec {
        name: "system.telemetry",
        description: "Read aggregate MCP host telemetry and top hot methods for this session.",
        dispatch: DispatchTarget::Host,
        replay: ReplayContract::Convergent,
    },
];

pub(crate) fn tool_spec(name: &str) -> Option<ToolSpec> {
    TOOL_SPECS.iter().copied().find(|spec| spec.name == name)
}

pub(crate) fn tool_definitions() -> Vec<Value> {
    TOOL_SPECS
        .iter()
        .map(|spec| {
            json!({
                "name": spec.name,
                "description": spec.description,
                "inputSchema": tool_schema(spec.name),
                "annotations": spec.annotation_json(),
            })
        })
        .collect()
}

fn tool_schema(name: &str) -> Value {
    match name {
        "project.bind" => with_common_presentation(json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Project root or any nested path inside the target project."
                }
            },
            "required": ["path"]
        })),
        "issue.save" => with_common_presentation(json!({
            "type": "object",
            "properties": issue_save_properties(),
            "required": ["category", "slug", "body"]
        })),
        "issue.delete" => issue_key_schema("Issue slug to delete within the selected category."),
        "issue.read" => issue_key_schema("Issue slug to read within the selected category."),
        "issue.list" | "system.health" | "system.telemetry" => with_common_presentation(json!({
            "type": "object",
            "properties": {}
        })),
        _ => Value::Null,
    }
}

fn issue_save_properties() -> Map<String, Value> {
    let mut properties = issue_key_properties(
        "Stable slug. Stored at `issues/<category>/<slug>.md` under the bound project's external state root.",
    );
    let _ = properties.insert(
        "body".to_owned(),
        json!({
            "type": "string",
            "description": "Freeform issue body. Markdown is fine."
        }),
    );
    properties
}

fn issue_key_schema(slug_description: &'static str) -> Value {
    with_common_presentation(json!({
        "type": "object",
        "properties": issue_key_properties(slug_description),
        "required": ["category", "slug"]
    }))
}

fn issue_key_properties(slug_description: &'static str) -> Map<String, Value> {
    let mut properties = Map::new();
    let _ = properties.insert("category".to_owned(), issue_category_schema());
    let _ = properties.insert(
        "slug".to_owned(),
        json!({
            "type": "string",
            "description": slug_description
        }),
    );
    properties
}

fn issue_category_schema() -> Value {
    json!({
        "type": "string",
        "description": "Mandatory issue category.",
        "enum": IssueCategory::all().map(IssueCategory::as_str)
    })
}
