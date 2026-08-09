use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use libmcp::{
    DispatchQueueOutcome, FrameLimit, FrameParseError, FramedMessage, Generation, HandoffOutcome,
    HostRejection, HostSessionKernel, ReleaseRuntime, ReplayBudget, ReplayContract, RequestId,
    RolloutState, RpcEnvelopeKind, SessionPhase, SnapshotLimits, TelemetryFlushPolicy,
    TelemetryLog, TimedFrameReadOutcome, TimedFrameReader, ToolOutcome,
    load_snapshot_file_from_env, write_snapshot_file,
};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::mcp::catalog::{DispatchTarget, tool_definitions, tool_spec};
use crate::mcp::fault::{FaultRecord, FaultStage};
use crate::mcp::host::process::{ProjectBinding, WorkerSupervisor};
use crate::mcp::output::{
    ToolOutput, fallback_detailed_tool_output, split_presentation, tool_success,
};
use crate::mcp::protocol::{
    CRASH_ONCE_ENV, FORCE_ROLLOUT_ENV, HOST_STATE_ENV, HostRequestId, HostStateSeed,
    PROTOCOL_VERSION, ProjectBindingSeed, SERVER_NAME, WorkerOperation, WorkerSpawnConfig,
    write_sync_json_frame,
};
use crate::mcp::telemetry::ServerTelemetry;
use crate::store::IssueStore;

const HOST_SNAPSHOT_MAX_BYTES: usize = 16 * 1024 * 1024;
const PENDING_CAPACITY: usize = 1;
const RECOVERY_QUEUE_CAPACITY: usize = 1;
const MAX_REPLAY_ATTEMPTS: u8 = 1;
const HOST_CONTROL_POLL_INTERVAL: Duration = Duration::from_millis(250);
const HOST_HANDOFF_TIMEOUT: Duration = Duration::from_secs(15);
const HOST_ROLLOUT_RETRY_DELAY: Duration = Duration::from_secs(5);

pub(crate) fn run_host(initial_project: Option<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    let stdin = io::stdin();
    let mut stdin = TimedFrameReader::new(stdin.lock(), FrameLimit::DEFAULT);
    let mut stdout = io::stdout().lock();
    let mut host = HostRuntime::new(initial_project)?;
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
    binding: Option<ProjectBinding>,
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
    fn new(initial_project: Option<PathBuf>) -> Result<Self, Box<dyn std::error::Error>> {
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
        let binding = if let Some(seed) = restored.as_ref().and_then(|seed| seed.binding.clone()) {
            Some(restore_binding(seed)?)
        } else if let Some(path) = initial_project.clone() {
            Some(resolve_project_binding(path)?.binding)
        } else {
            None
        };
        let telemetry_log = binding.as_ref().map(open_telemetry_log).transpose()?;

        let mut worker = WorkerSupervisor::new(
            WorkerSpawnConfig {
                executable: executable.clone(),
            },
            worker_generation,
            worker_spawned,
        );
        if let Some(worktree_root) = binding
            .as_ref()
            .map(|binding| binding.worktree_root.clone())
        {
            worker.rebind(worktree_root);
        }

        Ok(Self {
            binding,
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
                    Value::Null,
                    FaultRecord::parse_error(
                        self.worker.generation(),
                        "jsonrpc.parse",
                        format!("parse error: {error}"),
                    ),
                ));
            }
            Err(error) => {
                return Some(jsonrpc_error(
                    Value::Null,
                    FaultRecord::invalid_request(
                        self.worker.generation(),
                        "jsonrpc.request",
                        error.to_string(),
                    ),
                ));
            }
        };
        self.handle_frame(frame)
    }

    fn handle_frame(&mut self, frame: FramedMessage) -> Option<Value> {
        let (request_id, method) = match frame.classify() {
            RpcEnvelopeKind::Request { id, method } => (Some(id), method),
            RpcEnvelopeKind::Notification { method } => (None, method),
            RpcEnvelopeKind::Response { .. } => return None,
        };
        let object = frame.value().as_object()?;
        let id = request_id.as_ref().map(RequestId::to_json_value);
        let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
        let operation_key = operation_key(method.as_str(), &params);
        let started_at = Instant::now();

        self.telemetry.record_request(&operation_key);
        let admission = self
            .session_kernel
            .observe_client_frame(&frame)
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
                            &frame,
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
            admission.and_then(|()| self.dispatch(&frame, method.as_str(), params, id.clone()));
        let response = match dispatched {
            Ok(Some(result)) => {
                let latency_ms = elapsed_ms(started_at.elapsed());
                self.telemetry.record_success(&operation_key, latency_ms);
                self.record_tool_completion_from_frame(
                    &frame,
                    request_id.as_ref(),
                    latency_ms,
                    None,
                );
                id.map(|id| jsonrpc_result(id, result))
            }
            Ok(None) => {
                let latency_ms = elapsed_ms(started_at.elapsed());
                self.telemetry.record_success(&operation_key, latency_ms);
                self.record_tool_completion_from_frame(
                    &frame,
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
                    &frame,
                    request_id.as_ref(),
                    latency_ms,
                    Some(&fault),
                );
                id.map(|id| match method.as_str() {
                    "tools/call" => jsonrpc_result(id, fault.into_tool_result()),
                    _ => jsonrpc_error(id, fault),
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
                "instructions": "Bind the session with project.bind, then use issue.save to park categorized notes in the bound state directory under issues/<category>/<slug>.md. The mandatory closed category set is `feature` or `bug`; issue.read and issue.delete require both category and slug, while issue.list enumerates everything still parked."
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
        self.dispatch_worker_operation(
            request_frame,
            operation,
            spec.replay,
            WorkerOperation::CallTool {
                name: spec.name.to_owned(),
                arguments,
            },
        )
    }

    fn dispatch_worker_operation(
        &mut self,
        request_frame: &FramedMessage,
        operation: String,
        replay: ReplayContract,
        worker_operation: WorkerOperation,
    ) -> Result<Value, FaultRecord> {
        let binding = self.require_bound_project(&operation)?;
        self.worker.rebind(binding.worktree_root.clone());

        if self.should_crash_worker_once(&operation) {
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
                    &operation,
                    rejection,
                )
            })?;
        let host_request_id = self.allocate_request_id(&operation)?;
        match self
            .worker
            .execute(host_request_id, worker_operation.clone())
        {
            Ok(result) => Ok(result),
            Err(fault) => {
                if replay == ReplayContract::Convergent && fault.retryable {
                    self.telemetry.record_recovery_fault(&operation, &fault);
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
                            &operation,
                            rejected.reason,
                        ));
                    }
                    self.worker
                        .restart()
                        .map_err(|restart_fault| restart_fault.mark_retried())?;
                    self.telemetry.replace_worker(self.worker.generation());
                    let dispatch =
                        self.session_kernel
                            .pop_next_dispatch()
                            .map_err(|rejection| {
                                FaultRecord::host_rejection(
                                    self.worker.generation(),
                                    FaultStage::Host,
                                    &operation,
                                    rejection,
                                )
                            })?;
                    let DispatchQueueOutcome::Replay(replay_frame) = dispatch else {
                        return Err(FaultRecord::internal(
                            self.worker.generation(),
                            FaultStage::Host,
                            &operation,
                            "recovery kernel did not authorize the scheduled replay",
                        ));
                    };
                    if replay_frame.payload() != request_frame.payload() {
                        return Err(FaultRecord::internal(
                            self.worker.generation(),
                            FaultStage::Host,
                            &operation,
                            "recovery kernel returned a divergent replay frame",
                        ));
                    }
                    self.telemetry.record_replay(&operation);
                    match self.worker.execute(host_request_id, worker_operation) {
                        Err(replay_fault) if replay_fault.retryable => {
                            self.telemetry
                                .record_recovery_fault(&operation, &replay_fault);
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
                                    &operation,
                                    "recovery kernel accepted a replay beyond the attempt budget",
                                ));
                            };
                            Err(FaultRecord::host_rejection(
                                self.worker.generation(),
                                FaultStage::Host,
                                &operation,
                                rejected.reason,
                            )
                            .mark_retried())
                        }
                        Err(replay_fault) => Err(replay_fault.mark_retried()),
                        Ok(result) => Ok(result),
                    }
                } else if fault.retryable {
                    self.telemetry.record_recovery_fault(&operation, &fault);
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
                            .record_recovery_fault(&operation, &restart_fault),
                    }
                    Err(FaultRecord::host_rejection(
                        self.worker.generation(),
                        FaultStage::Host,
                        &operation,
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
            "project.bind" => {
                let args = deserialize::<ProjectBindArgs>(
                    arguments,
                    "tools/call:project.bind",
                    generation,
                )?;
                let resolved =
                    resolve_project_binding(PathBuf::from(args.path)).map_err(|error| {
                        FaultRecord::invalid_input(
                            generation,
                            FaultStage::Host,
                            "tools/call:project.bind",
                            error.to_string(),
                        )
                    })?;
                self.worker
                    .refresh_binding(resolved.binding.worktree_root.clone());
                self.telemetry_log =
                    Some(open_telemetry_log(&resolved.binding).map_err(|error| {
                        FaultRecord::internal(
                            generation,
                            FaultStage::Host,
                            "tools/call:project.bind",
                            error.to_string(),
                        )
                    })?);
                self.binding = Some(resolved.binding);
                tool_success(
                    project_bind_output(&resolved.status, generation)?,
                    presentation,
                    generation,
                    FaultStage::Host,
                    "tools/call:project.bind",
                )
            }
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
                tool_success(
                    system_health_output(
                        &health,
                        self.binding.as_ref(),
                        worker_alive,
                        self.release.launch_path_stable(),
                        generation,
                    )?,
                    presentation,
                    generation,
                    FaultStage::Host,
                    &operation,
                )
            }
            "system.telemetry" => {
                let worker_alive = self.worker.is_alive();
                let snapshot = self.telemetry.telemetry_snapshot(worker_alive);
                let hot_methods = self.telemetry.ranked_methods(worker_alive);
                tool_success(
                    system_telemetry_output(
                        &snapshot,
                        &hot_methods,
                        self.telemetry.host_rollouts(),
                        generation,
                    )?,
                    presentation,
                    generation,
                    FaultStage::Host,
                    &operation,
                )
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

    fn require_bound_project(&self, operation: &str) -> Result<&ProjectBinding, FaultRecord> {
        self.binding.as_ref().ok_or_else(|| {
            FaultRecord::unavailable(
                self.worker.generation(),
                FaultStage::Host,
                operation,
                "project is not bound; call project.bind with the target project root or a nested path inside it",
            )
        })
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
            binding: self.binding.as_ref().map(ProjectBindingSeed::from),
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

struct ResolvedProjectBinding {
    binding: ProjectBinding,
    status: ProjectBindStatus,
}

#[derive(Debug, Serialize)]
struct ProjectBindStatus {
    requested_path: String,
    project_root: String,
    worktree_root: String,
    state_identity: String,
    issues_root: String,
    state_root: String,
    issue_count: usize,
}

fn resolve_project_binding(
    requested_path: PathBuf,
) -> Result<ResolvedProjectBinding, Box<dyn std::error::Error>> {
    let store = IssueStore::bind(requested_path.clone())?;
    let layout = store.layout().clone();
    let status = store.status()?;
    Ok(ResolvedProjectBinding {
        binding: ProjectBinding {
            requested_path: requested_path.clone(),
            project_root: layout.project_root.clone(),
            worktree_root: layout.worktree_root.clone(),
            state_identity: layout.state_identity.clone(),
            issues_root: layout.issues_root.clone(),
            state_root: layout.state_root.clone(),
        },
        status: ProjectBindStatus {
            requested_path: requested_path.display().to_string(),
            project_root: layout.project_root.display().to_string(),
            worktree_root: layout.worktree_root.display().to_string(),
            state_identity: layout.state_identity.display().to_string(),
            issues_root: layout.issues_root.display().to_string(),
            state_root: layout.state_root.display().to_string(),
            issue_count: status.issue_count,
        },
    })
}

fn restore_binding(seed: ProjectBindingSeed) -> Result<ProjectBinding, Box<dyn std::error::Error>> {
    Ok(resolve_project_binding(seed.requested_path)?.binding)
}

fn restore_host_state() -> Result<Option<HostStateSeed>, Box<dyn std::error::Error>> {
    Ok(load_snapshot_file_from_env(
        HOST_STATE_ENV,
        HOST_SNAPSHOT_MAX_BYTES,
    )?)
}

fn open_telemetry_log(binding: &ProjectBinding) -> io::Result<TelemetryLog> {
    TelemetryLog::new(
        binding
            .state_root
            .join("mcp")
            .join("telemetry.jsonl")
            .as_path(),
        binding.project_root.as_path(),
        1,
        TelemetryFlushPolicy::PageCache,
    )
}

fn project_bind_output(
    status: &ProjectBindStatus,
    generation: Generation,
) -> Result<ToolOutput, FaultRecord> {
    let mut concise = Map::new();
    let _ = concise.insert("project_root".to_owned(), json!(status.project_root));
    let _ = concise.insert("issues_root".to_owned(), json!(status.issues_root));
    let _ = concise.insert("state_root".to_owned(), json!(status.state_root));
    let _ = concise.insert("issue_count".to_owned(), json!(status.issue_count));
    if status.worktree_root != status.project_root {
        let _ = concise.insert("worktree_root".to_owned(), json!(status.worktree_root));
    }
    if status.state_identity != status.project_root {
        let _ = concise.insert("state_identity".to_owned(), json!(status.state_identity));
    }
    if status.requested_path != status.project_root {
        let _ = concise.insert("requested_path".to_owned(), json!(status.requested_path));
    }
    let mut lines = vec![format!("bound project {}", status.project_root)];
    if status.worktree_root != status.project_root {
        lines.push(format!("worktree: {}", status.worktree_root));
    }
    if status.state_identity != status.project_root {
        lines.push(format!("identity: {}", status.state_identity));
    }
    lines.extend([
        format!("issues: {}", status.issues_root),
        format!("state: {}", status.state_root),
        format!("issues tracked: {}", status.issue_count),
    ]);
    fallback_detailed_tool_output(
        &Value::Object(concise),
        status,
        lines.join("\n"),
        None,
        libmcp::SurfaceKind::Mutation,
        generation,
        FaultStage::Host,
        "tools/call:project.bind",
    )
}

fn system_health_output(
    health: &libmcp::HealthSnapshot,
    binding: Option<&ProjectBinding>,
    worker_alive: bool,
    launch_path_stable: bool,
    generation: Generation,
) -> Result<ToolOutput, FaultRecord> {
    let rollout_pending = matches!(health.rollout, Some(RolloutState::Pending));
    let mut concise = Map::new();
    let _ = concise.insert(
        "ready".to_owned(),
        json!(matches!(health.state, libmcp::LifecycleState::Ready)),
    );
    let _ = concise.insert("bound".to_owned(), json!(binding.is_some()));
    let _ = concise.insert(
        "worker_generation".to_owned(),
        json!(health.generation.get()),
    );
    let _ = concise.insert("worker_alive".to_owned(), json!(worker_alive));
    let _ = concise.insert("rollout_pending".to_owned(), json!(rollout_pending));
    let _ = concise.insert("launch_path_stable".to_owned(), json!(launch_path_stable));
    if let Some(binding) = binding {
        let _ = concise.insert(
            "project_root".to_owned(),
            json!(binding.project_root.display().to_string()),
        );
        let _ = concise.insert(
            "issues_root".to_owned(),
            json!(binding.issues_root.display().to_string()),
        );
        if binding.worktree_root != binding.project_root {
            let _ = concise.insert(
                "worktree_root".to_owned(),
                json!(binding.worktree_root.display().to_string()),
            );
        }
    }
    let full = json!({
        "health": health,
        "binding": binding.map(|binding| json!({
            "requested_path": binding.requested_path.display().to_string(),
            "project_root": binding.project_root.display().to_string(),
            "worktree_root": binding.worktree_root.display().to_string(),
            "state_identity": binding.state_identity.display().to_string(),
            "issues_root": binding.issues_root.display().to_string(),
            "state_root": binding.state_root.display().to_string(),
        })),
        "worker_alive": worker_alive,
        "launch_path_stable": launch_path_stable,
    });
    let mut lines = vec![format!(
        "{} | {}",
        if matches!(health.state, libmcp::LifecycleState::Ready) {
            "ready"
        } else {
            "not-ready"
        },
        if binding.is_some() {
            "bound"
        } else {
            "unbound"
        }
    )];
    if let Some(binding) = binding {
        lines.push(format!("project: {}", binding.project_root.display()));
        if binding.worktree_root != binding.project_root {
            lines.push(format!("worktree: {}", binding.worktree_root.display()));
        }
        lines.push(format!("issues: {}", binding.issues_root.display()));
    }
    lines.push(format!(
        "worker: gen {} {}",
        health.generation.get(),
        if worker_alive { "alive" } else { "dead" }
    ));
    lines.push(format!(
        "binary: {}{}",
        if launch_path_stable {
            "stable"
        } else {
            "unstable"
        },
        if rollout_pending {
            " rollout-pending"
        } else {
            ""
        }
    ));
    fallback_detailed_tool_output(
        &Value::Object(concise),
        &full,
        lines.join("\n"),
        None,
        libmcp::SurfaceKind::Ops,
        generation,
        FaultStage::Host,
        "tools/call:system.health",
    )
}

fn system_telemetry_output(
    telemetry: &libmcp::TelemetrySnapshot,
    ranked_methods: &[libmcp::MethodTelemetry],
    host_rollouts: u64,
    generation: Generation,
) -> Result<ToolOutput, FaultRecord> {
    let hot_methods = ranked_methods.iter().take(6).collect::<Vec<_>>();
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
        "hot_methods": hot_methods.iter().map(|method| json!({
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
        "telemetry": telemetry,
        "host_rollouts": host_rollouts,
    });
    let mut lines = vec![format!(
        "requests={} success={} error={} response_error={} recovery_error={} recovery_fault={} retry={}",
        telemetry.totals.request_count(),
        telemetry.totals.success_count(),
        telemetry.totals.error_count(),
        telemetry.totals.response_error_count(),
        telemetry.totals.recovery_error_count(),
        telemetry.totals.recovery_fault_count(),
        telemetry.totals.retry_count()
    )];
    lines.push(format!(
        "worker_restarts={} host_rollouts={host_rollouts}",
        telemetry.restart_count,
    ));
    if !hot_methods.is_empty() {
        lines.push("hot methods:".to_owned());
        for method in hot_methods {
            lines.push(format!(
                "{} req={} err={} recovery={} retry={}",
                method.method(),
                method.request_count(),
                method.error_count(),
                method.recovery_fault_count(),
                method.retry_count(),
            ));
        }
    }
    fallback_detailed_tool_output(
        &concise,
        &full,
        lines.join("\n"),
        None,
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

fn jsonrpc_result(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn jsonrpc_error(id: Value, fault: FaultRecord) -> Value {
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

#[derive(Debug, serde::Deserialize)]
struct ProjectBindArgs {
    path: String,
}

impl From<&ProjectBinding> for ProjectBindingSeed {
    fn from(value: &ProjectBinding) -> Self {
        Self {
            requested_path: value.requested_path.clone(),
            project_root: value.project_root.clone(),
            worktree_root: value.worktree_root.clone(),
            state_identity: value.state_identity.clone(),
        }
    }
}
