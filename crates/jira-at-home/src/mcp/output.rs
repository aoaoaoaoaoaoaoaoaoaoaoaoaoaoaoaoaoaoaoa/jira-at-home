use libmcp::{
    DetailLevel, FallbackJsonProjection, ProjectionError, RenderMode, SurfaceKind, ToolProjection,
    render_json_porcelain, with_presentation_properties,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::mcp::fault::{FaultRecord, FaultStage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Presentation {
    pub(crate) render: RenderMode,
    pub(crate) detail: DetailLevel,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolOutput {
    concise: Value,
    full: Value,
}

impl ToolOutput {
    pub(crate) fn with_project(mut self, project: &std::path::Path) -> Self {
        self.concise["project_root"] = json!(project);
        self.full["project_root"] = json!(project);
        self
    }

    fn structured(&self, detail: DetailLevel) -> &Value {
        match detail {
            DetailLevel::Concise => &self.concise,
            DetailLevel::Full => &self.full,
        }
    }
}

impl Default for Presentation {
    fn default() -> Self {
        Self {
            render: RenderMode::Porcelain,
            detail: DetailLevel::Concise,
        }
    }
}

pub(crate) fn split_presentation(
    arguments: Value,
    operation: &str,
    generation: libmcp::Generation,
    stage: FaultStage,
) -> Result<(Presentation, Value), FaultRecord> {
    let Value::Object(mut object) = arguments else {
        return Ok((Presentation::default(), arguments));
    };
    let render = object
        .remove("render")
        .map(|value| {
            serde_json::from_value::<RenderMode>(value).map_err(|error| {
                FaultRecord::invalid_input(
                    generation,
                    stage,
                    operation,
                    format!("invalid render mode: {error}"),
                )
            })
        })
        .transpose()?
        .unwrap_or(RenderMode::Porcelain);
    let detail = object
        .remove("detail")
        .map(|value| {
            serde_json::from_value::<DetailLevel>(value).map_err(|error| {
                FaultRecord::invalid_input(
                    generation,
                    stage,
                    operation,
                    format!("invalid detail level: {error}"),
                )
            })
        })
        .transpose()?
        .unwrap_or(DetailLevel::Concise);
    Ok((Presentation { render, detail }, Value::Object(object)))
}

fn projected_tool_output(
    projection: &impl ToolProjection,
    generation: libmcp::Generation,
    stage: FaultStage,
    operation: &str,
) -> Result<ToolOutput, FaultRecord> {
    let concise = projection
        .structured_projection(DetailLevel::Concise)
        .map_err(|error| projection_fault(&error, generation, stage, operation))?;
    let full = projection
        .structured_projection(DetailLevel::Full)
        .map_err(|error| projection_fault(&error, generation, stage, operation))?;
    Ok(ToolOutput { concise, full })
}

pub(crate) fn fallback_detailed_tool_output(
    concise: &impl Serialize,
    full: &impl Serialize,
    kind: SurfaceKind,
    generation: libmcp::Generation,
    stage: FaultStage,
    operation: &str,
) -> Result<ToolOutput, FaultRecord> {
    let projection = FallbackJsonProjection::new(concise, full, kind)
        .map_err(|error| projection_fault(&error, generation, stage, operation))?;
    projected_tool_output(&projection, generation, stage, operation)
}

pub(crate) fn tool_success(output: &ToolOutput, presentation: Presentation) -> Value {
    tool_result(
        output.structured(presentation.detail),
        presentation.render,
        false,
    )
}

pub(crate) fn tool_result(selected: &Value, render: RenderMode, is_error: bool) -> Value {
    match render {
        RenderMode::Porcelain => json!({
            "content": [{
                "type": "text",
                "text": render_json_porcelain(selected),
            }],
            "isError": is_error,
        }),
        RenderMode::Json => json!({
            "content": [],
            "structuredContent": selected,
            "isError": is_error,
        }),
    }
}

pub(crate) fn with_common_presentation(schema: Value) -> Value {
    with_presentation_properties(schema)
}

fn projection_fault(
    error: &ProjectionError,
    generation: libmcp::Generation,
    stage: FaultStage,
    operation: &str,
) -> FaultRecord {
    FaultRecord::internal(generation, stage, operation, error.to_string())
}
