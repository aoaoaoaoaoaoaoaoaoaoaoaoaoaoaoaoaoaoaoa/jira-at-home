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
    fn effect_json(self) -> Value {
        let state = if self.name == "project.bind" {
            json!({"kind": "journaled", "key": "project"})
        } else {
            json!({"kind": "stateless"})
        };
        json!({
            "recovery": {"kind": match self.replay {
                ReplayContract::Convergent => "replay_safe",
                ReplayContract::ProbeRequired => "probe_required",
                ReplayContract::NeverReplay => "at_most_once",
            }},
            "state": state
        })
    }

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
        description: "Bind to a project root or a nested path. Full detail adds storage metadata.",
        dispatch: DispatchTarget::Host,
        replay: ReplayContract::Convergent,
    },
    ToolSpec {
        name: "issue.save",
        description: "Create or overwrite a Markdown issue. Changing its epic moves the existing note.",
        dispatch: DispatchTarget::Worker,
        replay: ReplayContract::NeverReplay,
    },
    ToolSpec {
        name: "issue.delete",
        description: "Delete an issue permanently; it leaves every depends_on that listed it.",
        dispatch: DispatchTarget::Worker,
        replay: ReplayContract::NeverReplay,
    },
    ToolSpec {
        name: "issue.list",
        description: "Page issue selectors with effort and dependency count, or epic names. Full detail adds depends_on, timestamps and paths; neither view includes bodies. Pages reflect current state, not a retained snapshot.",
        dispatch: DispatchTarget::Worker,
        replay: ReplayContract::Convergent,
    },
    ToolSpec {
        name: "issue.read",
        description: "Read a complete issue body and its dependents. Full detail adds path and byte count.",
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
        description: "Session totals and a page of methods ranked by request count. Full detail adds timings and fault context; it does not expand the page.",
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
                "_meta": {"io.libmcp/effect": spec.effect_json()},
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
        "issue.list" => with_common_presentation(json!({
            "type": "object",
            "properties": {
                "epic": {
                    "type": "string",
                    "description": "Issues only: omit for all, \"\" for no epic, or an exact epic name."
                },
                "view": { "type": "string", "enum": ["issues", "epics"], "default": "issues" },
                "limit": page_limit(50),
                "offset": page_offset()
            }
        })),
        "system.health" => with_common_presentation(json!({
            "type": "object",
            "properties": {}
        })),
        "system.telemetry" => with_common_presentation(json!({
            "type": "object",
            "properties": {
                "method": { "type": "string", "description": "Exact method name; totals remain session-wide." },
                "limit": page_limit(6),
                "offset": page_offset()
            }
        })),
        _ => Value::Null,
    }
}

fn page_limit(default: usize) -> Value {
    json!({"type": "integer", "minimum": 1, "maximum": 200, "default": default})
}

fn page_offset() -> Value {
    json!({"type": "integer", "minimum": 0, "default": 0,
        "description": "Continue with next_offset and the same view/filter. Live changes can shift rows."})
}

fn issue_save_properties() -> Map<String, Value> {
    let mut properties =
        issue_key_properties("Stable slug, unique within its category across all epics.");
    let _ = properties.insert(
        "epic".to_owned(),
        json!({
            "type": "string",
            "description": "Epic subdirectory, shaped like a slug. Omit to store without an epic."
        }),
    );
    let _ = properties.insert(
        "body".to_owned(),
        json!({
            "type": "string",
            "description": "Markdown body. It may open with TOML front matter between +++ lines: depends_on = [\"bug/slug\"] names existing issues to finish first; effort is 1, 2, 4, 8, 16 or 32 units."
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
