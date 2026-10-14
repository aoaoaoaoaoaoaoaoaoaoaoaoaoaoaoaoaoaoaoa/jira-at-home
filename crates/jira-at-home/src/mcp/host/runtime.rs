use std::io::{self, Write};
use std::time::{Duration, Instant};

use libmcp::{
    DispatchQueueOutcome, FrameLimit, FrameParseError, FramedMessage, Generation, HandoffOutcome,
    HostRejection, HostSessionKernel, ReleaseRuntime, ReplayBudget, ReplayContract, RequestId,
    RolloutState, RpcEnvelopeKind, SessionPhase, SnapshotLimits, TelemetryFlushPolicy,
    TelemetryLog, TimedFrameReadOutcome, TimedFrameReader, ToolOutcome,
    load_snapshot_file_from_env, write_snapshot_file,
};
use serde_json::{Map, Value, json};

use crate::mcp::catalog::{DispatchTarget, tool_definitions, tool_spec};
use crate::mcp::fault::{FaultRecord, FaultStage};
use crate::mcp::host::process::WorkerSupervisor;
use crate::mcp::output::{
    ToolOutput, fallback_detailed_tool_output, split_presentation, tool_success,
};
use crate::mcp::protocol::{
    CRASH_ONCE_ENV, FORCE_ROLLOUT_ENV, HOST_STATE_ENV, HostRequestId, HostStateSeed,
    PROTOCOL_VERSION, SERVER_NAME, WorkerOperation, WorkerSpawnConfig, write_sync_json_frame,
};
use crate::mcp::selection::PageArgs;
use crate::mcp::telemetry::ServerTelemetry;
use crate::store::ProjectLayout;

const HOST_SNAPSHOT_MAX_BYTES: usize = 16 * 1024 * 1024;
const PENDING_CAPACITY: usize = 1;
const RECOVERY_QUEUE_CAPACITY: usize = 1;
const MAX_REPLAY_ATTEMPTS: u8 = 1;
const HOST_CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(250);
const HOST_HANDOFF_TIMEOUT: Duration = Duration::from_secs(15);
const HOST_ROLLOUT_RETRY_DELAY: Duration = Duration::from_secs(5);

pub(crate) fn run_host() -> Result<(), Box<dyn std::error::Error>> {
    let stdin = io::stdin();
    let mut stdin = TimedFrameReader::new(stdin.lock(), FrameLimit::DEFAULT);
    let mut stdout = io::stdout().lock();
    let mut host = HostRuntime::new()?;
    host.release.admit_successor()?;

    loop {
        match stdin.read_frame(HOST_CONTROL_POLL_INTERVAL)? {
            TimedFrameReadOutcome::Frame(payload) => {
                if let Some(response) = host.handle_payload(payload) {
                    write_message(&mut stdout, &response)?;
                }
            }
            TimedFrameReadOutcome::EndOfStream => return Ok(()),
            TimedFrameReadOutcome::TimedOut => {}
        }
        if !stdin.has_buffered_input() && host.maybe_roll_forward() {
            return Ok(());
        }
    }
}

struct HostRuntime {
    default_project: ProjectLayout,
    session_kernel: HostSessionKernel,
    telemetry: ServerTelemetry,
    telemetry_log: Option<TelemetryLog>,
    next_request_id: u64,
    worker: WorkerSupervisor,
    release: ReleaseRuntime,
    force_rollout_key: Option<String>,
    force_rollout_consumed: bool,
    rollout_requested: bool,
    crash_once_key: Option<String>,
    crash_once_consumed: bool,
    rollout_retry_not_before: Option<Instant>,
}

impl HostRuntime {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let executable = std::env::current_exe()?;
        let release = ReleaseRuntime::discover(SERVER_NAME)?;
        let restored = restore_host_state()?;
        let session_kernel = if let Some(seed) = restored.as_ref() {
            let limits = SnapshotLimits::try_new(
                PENDING_CAPACITY,
                0,
                FrameLimit::DEFAULT.get(),
                MAX_REPLAY_ATTEMPTS,
            )?;
            seed.session_kernel.clone().restore(limits)?
        } else {
            HostSessionKernel::cold()
        };
        let telemetry = restored
            .as_ref()
            .map_or_else(ServerTelemetry::default, |seed| seed.telemetry.clone());
        let next_request_id = restored
            .as_ref()
            .map_or(1, |seed| seed.next_request_id.max(1));
        let worker_generation = restored
            .as_ref()
            .map_or(Generation::genesis(), |seed| seed.worker_generation);
        let worker_spawned = restored.as_ref().is_some_and(|seed| seed.worker_spawned);
        let force_rollout_consumed = restored
            .as_ref()
            .is_some_and(|seed| seed.force_rollout_consumed);
        let crash_once_consumed = restored
            .as_ref()
            .is_some_and(|seed| seed.crash_once_consumed);
        let default_project = ProjectLayout::resolve(std::env::current_dir()?)?;
        let telemetry_log = if default_project.state_root.exists() {
            Some(open_telemetry_log(&default_project)?)
        } else {
            None
        };

        let worker = WorkerSupervisor::new(
            WorkerSpawnConfig {
                executable: executable.clone(),
            },
            worker_generation,
            worker_spawned,
        );
        Ok(Self {
            default_project,
            session_kernel,
            telemetry,
            telemetry_log,
            next_request_id,
            worker,
            release,
            force_rollout_key: std::env::var(FORCE_ROLLOUT_ENV).ok(),
            force_rollout_consumed,
            rollout_requested: false,
            crash_once_key: std::env::var(CRASH_ONCE_ENV).ok(),
            crash_once_consumed,
            rollout_retry_not_before: None,
        })
    }

    fn handle_payload(&mut self, payload: Vec<u8>) -> Option<Value> {
        let frame = match FramedMessage::parse(payload) {
            Ok(frame) => frame,
            Err(FrameParseError::InvalidJson(error)) => {
                return Some(jsonrpc_error(
                    &Value::Null,
                    FaultRecord::parse_error(
                        self.worker.generation(),
                        "jsonrpc.parse",
                        format!("parse error: {error}"),
                    ),
                ));
            }
            Err(error) => {
                return Some(jsonrpc_error(
                    &Value::Null,
                    FaultRecord::invalid_request(
                        self.worker.generation(),
                        "jsonrpc.request",
                        error.to_string(),
                    ),
                ));
            }
        };
        self.handle_frame(&frame)
    }

    fn handle_frame(&mut self, frame: &FramedMessage) -> Option<Value> {
        let (request_id, method) = match frame.classify() {
            RpcEnvelopeKind::Request { id, method } => (Some(id), method),
            RpcEnvelopeKind::Notification { method } => (None, method),
            RpcEnvelopeKind::Response { .. } => return None,
        };
        let object = frame.value().as_object()?;
        let id = request_id.as_ref().map(RequestId::to_json_value);
        let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
        // A valid render survives any later validation or dispatch failure.
        let error_render = params
            .pointer("/arguments/render")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default();
        let operation_key = operation_key(method.as_str(), &params);
        let started_at = Instant::now();

        self.telemetry.record_request(&operation_key);
        let admission = self
            .session_kernel
            .observe_client_frame(frame)
            .map_err(|rejection| {
                FaultRecord::host_rejection(
                    self.worker.generation(),
                    FaultStage::Protocol,
                    &operation_key,
                    rejection,
                )
            })
            .and_then(|()| {
                if request_id.is_some() && !worker_dispatch(method.as_str(), &params) {
                    self.session_kernel
                        .begin_request_dispatch(
                            frame,
                            replay_contract(method.as_str(), &params),
                            PENDING_CAPACITY,
                        )
                        .map(|_| ())
                        .map_err(|rejection| {
                            FaultRecord::host_rejection(
                                self.worker.generation(),
                                FaultStage::Host,
                                &operation_key,
                                rejection,
                            )
                        })
                } else {
                    Ok(())
                }
            });
        let dispatched =
            admission.and_then(|()| self.dispatch(frame, method.as_str(), params, id.clone()));
        let response = match dispatched {
            Ok(Some(result)) => {
                let latency_ms = elapsed_ms(started_at.elapsed());
                self.telemetry.record_success(&operation_key, latency_ms);
                self.record_tool_completion_from_frame(
                    frame,
                    request_id.as_ref(),
                    latency_ms,
                    None,
                );
                id.map(|id| jsonrpc_result(&id, &result))
            }
            Ok(None) => {
                let latency_ms = elapsed_ms(started_at.elapsed());
                self.telemetry.record_success(&operation_key, latency_ms);
                self.record_tool_completion_from_frame(
                    frame,
                    request_id.as_ref(),
                    latency_ms,
                    None,
                );
                None
            }
            Err(fault) => {
                let latency_ms = elapsed_ms(started_at.elapsed());
                self.telemetry
                    .record_error(&operation_key, &fault, latency_ms);
                self.record_tool_completion_from_frame(
                    frame,
                    request_id.as_ref(),
                    latency_ms,
                    Some(&fault),
                );
                id.map(|id| match method.as_str() {
                    "tools/call" => jsonrpc_result(&id, &fault.into_tool_result(error_render)),
                    _ => jsonrpc_error(&id, fault),
                })
            }
        };

        if let (Some(request_id), Some(response)) = (request_id.as_ref(), response.as_ref())
            && self.session_kernel.pending_request(request_id).is_some()
        {
            complete_public_response(&mut self.session_kernel, response);
        }

        if self.should_force_rollout(&operation_key) {
            self.force_rollout_consumed = true;
            self.telemetry.record_rollout();
            self.rollout_requested = true;
        }

        response
    }

    fn dispatch(
        &mut self,
        request_frame: &FramedMessage,
        method: &str,
        params: Value,
        request_id: Option<Value>,
    ) -> Result<Option<Value>, FaultRecord> {
        match method {
            "initialize" => Ok(Some(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {
                    "tools": { "listChanged": false }
                },
                "serverInfo": {
                    "name": SERVER_NAME,
                    "version": env!("CARGO_PKG_VERSION")
                },
                "instructions": "Calls act on the project containing your working directory; pass project to reach another. Use issue.save to park categorized notes in its state directory under issues/<category>/<slug>.md, optionally inside an epic subdirectory (issues/<category>/<epic>/<slug>.md). The mandatory closed category set is `feature` or `bug`; issue.read and issue.delete require both category and slug and resolve the epic automatically, while issue.list enumerates everything still parked and can filter by epic."
            }))),
            "notifications/initialized" => {
                if !self.seed_captured() {
                    return Err(FaultRecord::not_initialized(
                        self.worker.generation(),
                        FaultStage::Host,
                        "notifications/initialized",
                        "received initialized notification before initialize",
                    ));
                }
                Ok(None)
            }
            "notifications/cancelled" => Ok(None),
            "ping" => Ok(Some(json!({}))),
            other => {
                self.require_initialized(other)?;
                match other {
                    "tools/list" => Ok(Some(json!({ "tools": tool_definitions() }))),
                    "tools/call" => Ok(Some(self.dispatch_tool_call(
                        request_frame,
                        params,
                        request_id,
                    )?)),
                    _ => Err(FaultRecord::invalid_input(
                        self.worker.generation(),
                        FaultStage::Protocol,
                        other,
                        format!("method `{other}` is not implemented"),
                    )),
                }
            }
        }
    }

    fn dispatch_tool_call(
        &mut self,
        request_frame: &FramedMessage,
        params: Value,
        _request_id: Option<Value>,
    ) -> Result<Value, FaultRecord> {
        let envelope =
            deserialize::<ToolCallEnvelope>(params, "tools/call", self.worker.generation())?;
        let spec = tool_spec(&envelope.name).ok_or_else(|| {
            FaultRecord::invalid_input(
                self.worker.generation(),
                FaultStage::Host,
                format!("tools/call:{}", envelope.name),
                format!("unknown tool `{}`", envelope.name),
            )
        })?;
        match spec.dispatch {
            DispatchTarget::Host => self.handle_host_tool(&envelope.name, envelope.arguments),
            DispatchTarget::Worker => {
                self.dispatch_worker_tool(request_frame, spec, envelope.arguments)
            }
        }
    }

    fn dispatch_worker_tool(
        &mut self,
        request_frame: &FramedMessage,
        spec: crate::mcp::catalog::ToolSpec,
        arguments: Value,
    ) -> Result<Value, FaultRecord> {
        let operation = format!("tools/call:{}", spec.name);
        let project_root = arguments
            .get("project")
            .and_then(Value::as_str)
            .map(ProjectLayout::resolve)
            .transpose()
            .map_err(|error| {
                FaultRecord::invalid_input(
                    self.worker.generation(),
                    FaultStage::Host,
                    &operation,
                    error.to_string(),
                )
            })?
            .map(|project| project.project_root)
            .filter(|root| root != &self.default_project.project_root);
        self.dispatch_worker_operation(
            request_frame,
            &operation,
            spec.replay,
            WorkerOperation::CallTool {
                name: spec.name.to_owned(),
                arguments,
            },
        )
        .map_err(|fault| fault.with_project(project_root))
    }

    fn dispatch_worker_operation(
        &mut self,
        request_frame: &FramedMessage,
        operation: &str,
        replay: ReplayContract,
        worker_operation: WorkerOperation,
    ) -> Result<Value, FaultRecord> {
        if self.telemetry_log.is_none()
            && matches!(
                operation,
                "tools/call:issue.save" | "tools/call:issue.delete"
            )
        {
            self.telemetry_log =
                Some(open_telemetry_log(&self.default_project).map_err(|error| {
                    FaultRecord::internal(
                        self.worker.generation(),
                        FaultStage::Host,
                        operation,
                        error.to_string(),
                    )
                })?);
        }

        if self.should_crash_worker_once(operation) {
            self.worker.arm_crash_once();
        }

        let generation_before_spawn = self.worker.generation();
        self.worker.ensure_ready()?;
        if self.worker.generation() > generation_before_spawn {
            self.telemetry.replace_worker(self.worker.generation());
        }
        let _request_id = self
            .session_kernel
            .begin_request_dispatch(request_frame, replay, PENDING_CAPACITY)
            .map_err(|rejection| {
                FaultRecord::host_rejection(
                    self.worker.generation(),
                    FaultStage::Host,
                    operation,
                    rejection,
                )
            })?;
        let host_request_id = self.allocate_request_id(operation)?;
        match self
            .worker
            .execute(host_request_id, worker_operation.clone())
        {
            Ok(result) => Ok(result),
            Err(fault) => {
                if replay == ReplayContract::Convergent && fault.retryable {
                    self.telemetry.record_recovery_fault(operation, &fault);
                    let recovery = self
                        .session_kernel
                        .requeue_pending_for_replay(ReplayBudget {
                            max_attempts: MAX_REPLAY_ATTEMPTS,
                            queue_capacity: RECOVERY_QUEUE_CAPACITY,
                        });
                    if let Some(rejected) = recovery.rejected.into_iter().next() {
                        return Err(FaultRecord::host_rejection(
                            self.worker.generation(),
                            FaultStage::Host,
                            operation,
                            rejected.reason,
                        ));
                    }
                    self.worker.restart().map_err(FaultRecord::mark_retried)?;
                    self.telemetry.replace_worker(self.worker.generation());
                    let dispatch =
                        self.session_kernel
                            .pop_next_dispatch()
                            .map_err(|rejection| {
                                FaultRecord::host_rejection(
                                    self.worker.generation(),
                                    FaultStage::Host,
                                    operation,
                                    rejection,
                                )
                            })?;
                    let DispatchQueueOutcome::Replay(replay_frame) = dispatch else {
                        return Err(FaultRecord::internal(
                            self.worker.generation(),
                            FaultStage::Host,
                            operation,
                            "recovery kernel did not authorize the scheduled replay",
                        ));
                    };
                    if replay_frame.payload() != request_frame.payload() {
                        return Err(FaultRecord::internal(
                            self.worker.generation(),
                            FaultStage::Host,
                            operation,
                            "recovery kernel returned a divergent replay frame",
                        ));
                    }
                    self.telemetry.record_replay(operation);
                    match self.worker.execute(host_request_id, worker_operation) {
                        Err(replay_fault) if replay_fault.retryable => {
                            self.telemetry
                                .record_recovery_fault(operation, &replay_fault);
                            let recovery =
                                self.session_kernel
                                    .requeue_pending_for_replay(ReplayBudget {
                                        max_attempts: MAX_REPLAY_ATTEMPTS,
                                        queue_capacity: RECOVERY_QUEUE_CAPACITY,
                                    });
                            let Some(rejected) = recovery.rejected.into_iter().next() else {
                                return Err(FaultRecord::internal(
                                    self.worker.generation(),
                                    FaultStage::Host,
                                    operation,
                                    "recovery kernel accepted a replay beyond the attempt budget",
                                ));
                            };
                            Err(FaultRecord::host_rejection(
                                self.worker.generation(),
                                FaultStage::Host,
                                operation,
                                rejected.reason,
                            )
                            .mark_retried())
                        }
                        Err(replay_fault) => Err(replay_fault.mark_retried()),
                        Ok(result) => Ok(result),
                    }
                } else if fault.retryable {
                    self.telemetry.record_recovery_fault(operation, &fault);
                    let recovery = self
                        .session_kernel
                        .requeue_pending_for_replay(ReplayBudget {
                            max_attempts: MAX_REPLAY_ATTEMPTS,
                            queue_capacity: RECOVERY_QUEUE_CAPACITY,
                        });
                    let rejection = recovery
                        .rejected
                        .into_iter()
                        .next()
                        .map_or(HostRejection::AmbiguousOutcome, |rejected| rejected.reason);
                    match self.worker.restart() {
                        Ok(()) => self.telemetry.replace_worker(self.worker.generation()),
                        Err(restart_fault) => self
                            .telemetry
                            .record_recovery_fault(operation, &restart_fault),
                    }
                    Err(FaultRecord::host_rejection(
                        self.worker.generation(),
                        FaultStage::Host,
                        operation,
                        rejection,
                    ))
                } else {
                    Err(fault)
                }
            }
        }
    }

    fn handle_host_tool(&mut self, name: &str, arguments: Value) -> Result<Value, FaultRecord> {
        let operation = format!("tools/call:{name}");
        let generation = self.worker.generation();
        let (presentation, arguments) =
            split_presentation(arguments, &operation, generation, FaultStage::Host)?;
        match name {
            "system.health" => {
                let rollout = if self
                    .release
                    .observe()
                    .map_err(|error| {
                        FaultRecord::rollout(generation, &operation, error.to_string())
                    })?
                    .rollout_pending()
                {
                    RolloutState::Pending
                } else {
                    RolloutState::Stable
                };
                let worker_alive = self.worker.is_alive();
                let health = self.telemetry.health_snapshot(rollout, worker_alive);
                let output = system_health_output(
                    &health,
                    &self.default_project,
                    worker_alive,
                    self.release.launch_path_stable(),
                    generation,
                )?;
                Ok(tool_success(&output, presentation))
            }
            "system.telemetry" => {
                let args = deserialize::<TelemetryArgs>(arguments, &operation, generation)?;
                let worker_alive = self.worker.is_alive();
                let snapshot = self.telemetry.telemetry_snapshot(worker_alive);
                let mut hot_methods = self.telemetry.ranked_methods(worker_alive);
                if let Some(method) = &args.method {
                    hot_methods.retain(|row| row.method().as_str() == method);
                }
                let output = system_telemetry_output(
                    &snapshot,
                    &hot_methods,
                    &args.page,
                    self.telemetry.host_rollouts(),
                    generation,
                )?;
                Ok(tool_success(&output, presentation))
            }
            other => Err(FaultRecord::invalid_input(
                generation,
                FaultStage::Host,
                format!("tools/call:{other}"),
                format!("unknown host tool `{other}`"),
            )),
        }
    }

    fn require_initialized(&self, operation: &str) -> Result<(), FaultRecord> {
        if self.session_initialized() {
            return Ok(());
        }
        Err(FaultRecord::not_initialized(
            self.worker.generation(),
            FaultStage::Host,
            operation,
            "client must call initialize and notifications/initialized before normal operations",
        ))
    }

    fn session_initialized(&self) -> bool {
        self.session_kernel.session_phase() == SessionPhase::Live
    }

    fn seed_captured(&self) -> bool {
        self.session_kernel.initialization_seed().is_some()
    }

    fn allocate_request_id(&mut self, operation: &str) -> Result<HostRequestId, FaultRecord> {
        let id = HostRequestId(self.next_request_id);
        self.next_request_id = self.next_request_id.checked_add(1).ok_or_else(|| {
            FaultRecord::internal(
                self.worker.generation(),
                FaultStage::Host,
                operation,
                "private worker request identifier space is exhausted",
            )
        })?;
        Ok(id)
    }

    fn maybe_roll_forward(&mut self) -> bool {
        if self
            .rollout_retry_not_before
            .is_some_and(|deadline| Instant::now() < deadline)
        {
            return false;
        }
        self.rollout_retry_not_before = None;
        let observation = match self.release.observe() {
            Ok(observation) => observation,
            Err(error) => {
                self.defer_rollout(error);
                return false;
            }
        };
        if self.rollout_requested
            && !observation.rollout_ready()
            && let Err(error) = self.release.arm_current_relaunch()
        {
            self.defer_rollout(error);
            return false;
        }
        if !self.rollout_requested && !observation.rollout_ready() {
            return false;
        }
        if observation.rollout_ready() && !self.rollout_requested {
            self.telemetry.record_rollout();
        }
        match self.roll_forward() {
            Ok(HandoffOutcome::Relinquish) => true,
            Ok(HandoffOutcome::Retained) => false,
            Err(error) => {
                self.defer_rollout(error);
                false
            }
        }
    }

    fn roll_forward(&self) -> Result<HandoffOutcome, Box<dyn std::error::Error>> {
        let state = HostStateSeed {
            session_kernel: self.session_kernel.snapshot(),
            telemetry: self.telemetry.clone(),
            next_request_id: self.next_request_id,
            worker_generation: self.worker.generation(),
            worker_spawned: self.worker.has_spawned(),
            force_rollout_consumed: self.force_rollout_consumed,
            crash_once_consumed: self.crash_once_consumed,
        };
        let state_capsule = write_snapshot_file("jira-at-home-mcp-host-reexec", &state)?;
        Ok(self
            .release
            .handoff(HOST_STATE_ENV, state_capsule.path(), HOST_HANDOFF_TIMEOUT)?)
    }

    fn defer_rollout(&mut self, error: impl std::fmt::Display) {
        self.rollout_retry_not_before = Instant::now().checked_add(HOST_ROLLOUT_RETRY_DELAY);
        eprintln!("jira-at-home MCP rollout retained incumbent: {error}");
    }

    fn should_force_rollout(&self, operation: &str) -> bool {
        self.force_rollout_key
            .as_deref()
            .is_some_and(|key| key == operation)
            && !self.force_rollout_consumed
    }

    fn should_crash_worker_once(&mut self, operation: &str) -> bool {
        let should_crash = self
            .crash_once_key
            .as_deref()
            .is_some_and(|key| key == operation)
            && !self.crash_once_consumed;
        if should_crash {
            self.crash_once_consumed = true;
        }
        should_crash
    }

    fn record_tool_completion_from_frame(
        &mut self,
        request_frame: &FramedMessage,
        request_id: Option<&RequestId>,
        latency_ms: u64,
        fault: Option<&FaultRecord>,
    ) {
        let Some(request_id) = request_id else {
            return;
        };
        let Some(tool_meta) =
            libmcp::parse_tool_call_meta(request_frame, &libmcp::RpcMethod::tools_call())
        else {
            return;
        };
        let replay_attempts = self
            .session_kernel
            .pending_request(request_id)
            .map_or(0, libmcp::PendingRequest::replay_attempts);
        self.record_tool_completion(request_id, &tool_meta, latency_ms, replay_attempts, fault);
    }

    fn record_tool_completion(
        &mut self,
        request_id: &RequestId,
        tool_meta: &libmcp::ToolCallMeta,
        latency_ms: u64,
        replay_attempts: u8,
        fault: Option<&FaultRecord>,
    ) {
        let Some(log) = self.telemetry_log.as_mut() else {
            return;
        };
        let result = log.record_tool_completion(
            request_id,
            tool_meta,
            latency_ms,
            replay_attempts,
            if fault.is_some() {
                ToolOutcome::Error
            } else {
                ToolOutcome::Ok
            },
            fault.map_or_else(libmcp::ToolErrorDetail::default, FaultRecord::error_detail),
        );
        if let Err(error) = result {
            eprintln!("jira_at_home telemetry write failed: {error}");
        }
    }
}

fn restore_host_state() -> Result<Option<HostStateSeed>, Box<dyn std::error::Error>> {
    Ok(load_snapshot_file_from_env(
        HOST_STATE_ENV,
        HOST_SNAPSHOT_MAX_BYTES,
    )?)
}

fn open_telemetry_log(project: &ProjectLayout) -> io::Result<TelemetryLog> {
    TelemetryLog::new(
        project
            .state_root
            .join("mcp")
            .join("telemetry.jsonl")
            .as_path(),
        project.project_root.as_path(),
        1,
        TelemetryFlushPolicy::PageCache,
    )
}

fn system_health_output(
    health: &libmcp::HealthSnapshot,
    default_project: &ProjectLayout,
    worker_alive: bool,
    launch_path_stable: bool,
    generation: Generation,
) -> Result<ToolOutput, FaultRecord> {
    let mut project = Map::new();
    let _ = project.insert(
        "project_root".to_owned(),
        json!(default_project.project_root),
    );
    let _ = project.insert("issues_root".to_owned(), json!(default_project.issues_root));
    if default_project.worktree_root != default_project.project_root {
        let _ = project.insert(
            "worktree_root".to_owned(),
            json!(default_project.worktree_root),
        );
    }
    let concise = json!({
        "ready": matches!(health.state, libmcp::LifecycleState::Ready),
        "default_project": project,
        "worker_generation": health.generation.get(),
        "worker_alive": worker_alive,
        "rollout_pending": matches!(health.rollout, Some(RolloutState::Pending)),
        "launch_path_stable": launch_path_stable,
    });
    let full = json!({
        "health": health,
        "default_project": default_project,
        "worker_alive": worker_alive,
        "launch_path_stable": launch_path_stable,
    });
    fallback_detailed_tool_output(
        &concise,
        &full,
        libmcp::SurfaceKind::Ops,
        generation,
        FaultStage::Host,
        "tools/call:system.health",
    )
}

fn system_telemetry_output(
    telemetry: &libmcp::TelemetrySnapshot,
    ranked_methods: &[libmcp::MethodTelemetry],
    args: &PageArgs,
    host_rollouts: u64,
    generation: Generation,
) -> Result<ToolOutput, FaultRecord> {
    let page = args.select(ranked_methods, 6);
    let concise = json!({
        "requests": telemetry.totals.request_count(),
        "successes": telemetry.totals.success_count(),
        "errors": telemetry.totals.error_count(),
        "response_errors": telemetry.totals.response_error_count(),
        "recovery_errors": telemetry.totals.recovery_error_count(),
        "recovery_faults": telemetry.totals.recovery_fault_count(),
        "retries": telemetry.totals.retry_count(),
        "worker_restarts": telemetry.restart_count,
        "host_rollouts": host_rollouts,
        "method_page": page.metadata(),
        "hot_methods": page.items.iter().map(|method| json!({
            "method": method.method(),
            "requests": method.request_count(),
            "errors": method.error_count(),
            "response_errors": method.response_error_count(),
            "recovery_errors": method.recovery_error_count(),
            "recovery_faults": method.recovery_fault_count(),
            "retries": method.retry_count(),
        })).collect::<Vec<_>>(),
    });
    let full = json!({
        "telemetry": libmcp::TelemetrySnapshot { methods: page.items.to_vec(), ..telemetry.clone() },
        "host_rollouts": host_rollouts,
        "method_page": page.metadata(),
    });
    fallback_detailed_tool_output(
        &concise,
        &full,
        libmcp::SurfaceKind::Ops,
        generation,
        FaultStage::Host,
        "tools/call:system.telemetry",
    )
}

fn deserialize<T: for<'de> serde::Deserialize<'de>>(
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

fn operation_key(method: &str, params: &Value) -> String {
    match method {
        "tools/call" => params.get("name").and_then(Value::as_str).map_or_else(
            || "tools/call".to_owned(),
            |name| format!("tools/call:{name}"),
        ),
        other => other.to_owned(),
    }
}

fn worker_dispatch(method: &str, params: &Value) -> bool {
    method == "tools/call"
        && params
            .get("name")
            .and_then(Value::as_str)
            .and_then(tool_spec)
            .is_some_and(|spec| spec.dispatch == DispatchTarget::Worker)
}

fn replay_contract(method: &str, params: &Value) -> ReplayContract {
    if method != "tools/call" {
        return ReplayContract::Convergent;
    }
    params
        .get("name")
        .and_then(Value::as_str)
        .and_then(tool_spec)
        .map_or(ReplayContract::NeverReplay, |spec| spec.replay)
}

fn complete_public_response(kernel: &mut HostSessionKernel, response: &Value) {
    let Ok(payload) = serde_json::to_vec(response) else {
        std::process::abort();
    };
    let Ok(frame) = FramedMessage::parse(payload) else {
        std::process::abort();
    };
    if kernel.complete_response(&frame).is_err() {
        std::process::abort();
    }
}

fn jsonrpc_result(id: &Value, result: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn jsonrpc_error(id: &Value, fault: FaultRecord) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": fault.into_jsonrpc_error(),
    })
}

fn write_message(stdout: &mut impl Write, message: &Value) -> io::Result<()> {
    write_sync_json_frame(stdout, message, FrameLimit::DEFAULT)
}

fn elapsed_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[derive(Debug, serde::Deserialize)]
struct ToolCallEnvelope {
    name: String,
    #[serde(default = "empty_json_object")]
    arguments: Value,
}

fn empty_json_object() -> Value {
    json!({})
}

#[derive(serde::Deserialize)]
struct TelemetryArgs {
    #[serde(flatten)]
    page: PageArgs,
    method: Option<String>,
}
