use libmcp::{
    DetailLevel, FallbackJsonProjection, JsonPorcelainConfig, ProjectionError, RenderMode,
    SurfaceKind, ToolProjection, render_json_porcelain, with_presentation_properties,
};
use serde::Serialize;
use serde_json::{Value, json};

use crate::mcp::fault::{FaultRecord, FaultStage};

const FULL_PORCELAIN_MAX_LINES: usize = 40;
const FULL_PORCELAIN_MAX_INLINE_CHARS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Presentation {
    pub(crate) render: RenderMode,
    pub(crate) detail: DetailLevel,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolOutput {
    concise: Value,
    full: Value,
    concise_text: String,
    full_text: Option<String>,
}

impl ToolOutput {
    pub(crate) fn from_values(
        concise: Value,
        full: Value,
        concise_text: impl Into<String>,
        full_text: Option<String>,
    ) -> Self {
        Self {
            concise,
            full,
            concise_text: concise_text.into(),
            full_text,
        }
    }

    fn structured(&self, detail: DetailLevel) -> &Value {
        match detail {
            DetailLevel::Concise => &self.concise,
            DetailLevel::Full => &self.full,
        }
    }

    fn porcelain_text(&self, detail: DetailLevel) -> String {
        match detail {
            DetailLevel::Concise => self.concise_text.clone(),
            DetailLevel::Full => self
                .full_text
                .clone()
                .unwrap_or_else(|| render_json_porcelain(&self.full, full_porcelain_config())),
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

pub(crate) fn projected_tool_output(
    projection: &impl ToolProjection,
    concise_text: impl Into<String>,
    full_text: Option<String>,
    generation: libmcp::Generation,
    stage: FaultStage,
    operation: &str,
) -> Result<ToolOutput, FaultRecord> {
    let concise = projection
        .concise_projection()
        .map_err(|error| projection_fault(error, generation, stage, operation))?;
    let full = projection
        .full_projection()
        .map_err(|error| projection_fault(error, generation, stage, operation))?;
    Ok(ToolOutput::from_values(
        concise,
        full,
        concise_text,
        full_text,
    ))
}

pub(crate) fn fallback_detailed_tool_output(
    concise: &impl Serialize,
    full: &impl Serialize,
    concise_text: impl Into<String>,
    full_text: Option<String>,
    kind: SurfaceKind,
    generation: libmcp::Generation,
    stage: FaultStage,
    operation: &str,
) -> Result<ToolOutput, FaultRecord> {
    let projection = FallbackJsonProjection::new(concise, full, kind)
        .map_err(|error| projection_fault(error, generation, stage, operation))?;
    projected_tool_output(
        &projection,
        concise_text,
        full_text,
        generation,
        stage,
        operation,
    )
}

pub(crate) fn tool_success(
    output: ToolOutput,
    presentation: Presentation,
    generation: libmcp::Generation,
    stage: FaultStage,
    operation: &str,
) -> Result<Value, FaultRecord> {
    let structured = output.structured(presentation.detail).clone();
    let text = match presentation.render {
        RenderMode::Porcelain => output.porcelain_text(presentation.detail),
        RenderMode::Json => serde_json::to_string_pretty(&structured).map_err(|error| {
            FaultRecord::internal(generation, stage, operation, error.to_string())
        })?,
    };
    Ok(json!({
        "content": [{
            "type": "text",
            "text": text,
        }],
        "structuredContent": structured,
        "isError": false,
    }))
}

pub(crate) fn with_common_presentation(schema: Value) -> Value {
    with_presentation_properties(schema)
}

fn projection_fault(
    error: ProjectionError,
    generation: libmcp::Generation,
    stage: FaultStage,
    operation: &str,
) -> FaultRecord {
    FaultRecord::internal(generation, stage, operation, error.to_string())
}

const fn full_porcelain_config() -> JsonPorcelainConfig {
    JsonPorcelainConfig {
        max_lines: FULL_PORCELAIN_MAX_LINES,
        max_inline_chars: FULL_PORCELAIN_MAX_INLINE_CHARS,
    }
}
