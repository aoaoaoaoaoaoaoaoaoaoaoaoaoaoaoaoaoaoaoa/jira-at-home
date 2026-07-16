use std::io::{self, Write};
use std::path::PathBuf;

use libmcp::{FrameLimit, Generation, HostSessionKernelSnapshot};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::mcp::telemetry::ServerTelemetry;

pub(crate) const PROTOCOL_VERSION: &str = "2025-11-25";
pub(crate) const SERVER_NAME: &str = "jira-at-home";
pub(crate) const HOST_STATE_ENV: &str = "JIRA_AT_HOME_MCP_HOST_STATE";
pub(crate) const FORCE_ROLLOUT_ENV: &str = "JIRA_AT_HOME_MCP_TEST_FORCE_ROLLOUT_KEY";
pub(crate) const CRASH_ONCE_ENV: &str = "JIRA_AT_HOME_MCP_TEST_HOST_CRASH_ONCE_KEY";

pub(crate) fn write_sync_json_frame(
    writer: &mut impl Write,
    value: &impl Serialize,
    limit: FrameLimit,
) -> io::Result<()> {
    let payload = serde_json::to_vec(value).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to encode frame: {error}"),
        )
    })?;
    libmcp::write_frame_blocking(writer, &payload, limit)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct HostStateSeed {
    pub(crate) session_kernel: HostSessionKernelSnapshot,
    pub(crate) telemetry: ServerTelemetry,
    pub(crate) next_request_id: u64,
    pub(crate) binding: Option<ProjectBindingSeed>,
    pub(crate) worker_generation: Generation,
    pub(crate) worker_spawned: bool,
    pub(crate) force_rollout_consumed: bool,
    pub(crate) crash_once_consumed: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ProjectBindingSeed {
    pub(crate) requested_path: PathBuf,
    pub(crate) project_root: PathBuf,
    #[serde(default)]
    pub(crate) worktree_root: PathBuf,
    #[serde(default)]
    pub(crate) state_identity: PathBuf,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct HostRequestId(pub(crate) u64);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum WorkerRequest {
    Execute {
        id: HostRequestId,
        operation: WorkerOperation,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum WorkerOperation {
    CallTool { name: String, arguments: Value },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct WorkerResponse {
    pub(crate) id: HostRequestId,
    pub(crate) outcome: WorkerOutcome,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum WorkerOutcome {
    Success {
        result: Value,
    },
    Fault {
        fault: crate::mcp::fault::FaultRecord,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct BinaryFingerprint {
    pub(crate) length_bytes: u64,
    pub(crate) modified_unix_nanos: u128,
}

#[derive(Clone, Debug)]
pub(crate) struct WorkerSpawnConfig {
    pub(crate) executable: PathBuf,
}
