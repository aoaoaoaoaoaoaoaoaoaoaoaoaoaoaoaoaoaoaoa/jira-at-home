use std::io::{self, BufRead, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use libmcp::{
    FramedMessage, Generation, HostSessionKernel, ReplayContract, RequestId, RolloutState,
    TelemetryLog, ToolOutcome, load_snapshot_file_from_env, remove_snapshot_file,
    write_snapshot_file,
};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::mcp::catalog::{DispatchTarget, tool_definitions, tool_spec};
use crate::mcp::fault::{FaultRecord, FaultStage};
use crate::mcp::host::binary::BinaryRuntime;
use crate::mcp::host::process::{ProjectBinding, WorkerSupervisor};
use crate::mcp::output::{
    ToolOutput, fallback_detailed_tool_output, split_presentation, tool_success,
};
use crate::mcp::protocol::{
    CRASH_ONCE_ENV, FORCE_ROLLOUT_ENV, HOST_STATE_ENV, HostRequestId, HostStateSeed,
    PROTOCOL_VERSION, ProjectBindingSeed, SERVER_NAME, WorkerOperation, WorkerSpawnConfig,
};
use crate::mcp::telemetry::ServerTelemetry;
use crate::store::IssueStore;

pub(crate) fn run_host(initial_project: Option<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    let mut host = HostRuntime::new(initial_project)?;

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let maybe_response = host.handle_line(&line);
        if let Some(response) = maybe_response {
            write_message(&mut stdout, &response)?;
        }
        host.maybe_roll_forward()?;
    }

    Ok(())
}

struct HostRuntime {
    initial_project: Option<PathBuf>,
    binding: Option<ProjectBinding>,
    session_kernel: HostSessionKernel,
    telemetry: ServerTelemetry,
    telemetry_log: Option<TelemetryLog>,
    next_request_id: u64,
    worker: WorkerSupervisor,
    binary: BinaryRuntime,
    force_rollout_key: Option<String>,
    force_rollout_consumed: bool,
    rollout_requested: bool,
    crash_once_key: Option<String>,
    crash_once_consumed: bool,
}

impl HostRuntime {
    fn new(initial_project: Option<PathBuf>) -> Result<Self, Box<dyn std::error::Error>> {
        let executable = std::env::current_exe()?;
        let binary = BinaryRuntime::new(executable.clone())?;
        let restored = restore_host_state()?;
        let session_kernel = restored
            .as_ref()
            .map(|seed| seed.session_kernel.clone().restore())
            .transpose()?
            .map_or_else(HostSessionKernel::cold, HostSessionKernel::from_restored);
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
        if let Some(project_root) = binding.as_ref().map(|binding| binding.project_root.clone()) {
            worker.rebind(project_root);
        }

        Ok(Self {
            initial_project,
            binding,
            session_kernel,
            telemetry,
            telemetry_log,
            next_request_id,
            worker,
            binary,
            force_rollout_key: std::env::var(FORCE_ROLLOUT_ENV).ok(),
            force_rollout_consumed,
            rollout_requested: false,
            crash_once_key: std::env::var(CRASH_ONCE_ENV).ok(),
            crash_once_consumed,
        })
    }

    fn handle_line(&mut self, line: &str) -> Option<Value> {
        let frame = match FramedMessage::parse(line.as_bytes().to_vec()) {
            Ok(frame) => frame,
            Err(error) => {
                return Some(jsonrpc_error(
                    Value::Null,
                    FaultRecord::invalid_input(
                        self.worker.generation(),
                        FaultStage::Protocol,
                        "jsonrpc.parse",
                        format!("parse error: {error}"),
                    ),
                ));
            }
        };
        self.handle_frame(frame)
    }

    fn handle_frame(&mut self, frame: FramedMessage) -> Option<Value> {
        self.session_kernel.observe_client_frame(&frame);
        let Some(object) = frame.value.as_object() else {
            return Some(jsonrpc_error(
                Value::Null,
                FaultRecord::invalid_input(
                    self.worker.generation(),
                    FaultStage::Protocol,
                    "jsonrpc.message",
                    "invalid request: expected JSON object",
                ),
            ));
        };
        let method = object.get("method").and_then(Value::as_str)?;
        let id = object.get("id").cloned();
        let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
        let operation_key = operation_key(method, &params);
        let started_at = Instant::now();

        self.telemetry.record_request(&operation_key);
        let response = match self.dispatch(&frame, method, params, id.clone()) {
            Ok(Some(result)) => {
                let latency_ms = elapsed_ms(started_at.elapsed());
                self.telemetry.record_success(
                    &operation_key,
                    latency_ms,
                    self.worker.generation(),
                    self.worker.is_alive(),
                );
                id.map(|id| jsonrpc_result(id, result))
            }
            Ok(None) => {
                let latency_ms = elapsed_ms(started_at.elapsed());
                self.telemetry.record_success(
                    &operation_key,
                    latency_ms,
                    self.worker.generation(),
                    self.worker.is_alive(),
                );
                None
            }
            Err(fault) => {
                let latency_ms = elapsed_ms(started_at.elapsed());
                self.telemetry.record_error(
                    &operation_key,
                    &fault,
                    latency_ms,
                    self.worker.generation(),
                );
                Some(match id {
                    Some(id) if method == "tools/call" => {
                        jsonrpc_result(id, fault.into_tool_result())
                    }
                    Some(id) => jsonrpc_error(id, fault),
                    None => jsonrpc_error(Value::Null, fault),
                })
            }
        };

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
                "instructions": "Bind the session with project.bind, then use issue.save to park ideas in issues/<slug>.md. issue.list enumerates every existing issue file because there is no closed state."
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
            DispatchTarget::Host => {
                let started_at = Instant::now();
                let request_id = request_id_from_frame(request_frame);
                let result = self.handle_host_tool(&envelope.name, envelope.arguments);
                self.record_host_tool_completion(
                    request_frame,
                    request_id.as_ref(),
                    elapsed_ms(started_at.elapsed()),
                    result.as_ref().err(),
                );
                result
            }
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
        self.worker.rebind(binding.project_root.clone());

        if self.should_crash_worker_once(&operation) {
            self.worker.arm_crash_once();
        }

        self.session_kernel
            .record_forwarded_request(request_frame, replay);
        let forwarded_request_id = request_id_from_frame(request_frame);
        let host_request_id = self.allocate_request_id();
        let started_at = Instant::now();
        let mut replay_attempts = 0;

        let outcome = match self
            .worker
            .execute(host_request_id, worker_operation.clone())
        {
            Ok(result) => Ok(result),
            Err(fault) => {
                if replay == ReplayContract::Convergent && fault.retryable {
                    replay_attempts = 1;
                    self.telemetry.record_retry(&operation);
                    self.worker
                        .restart()
                        .map_err(|restart_fault| restart_fault.mark_retried())?;
                    self.telemetry
                        .record_worker_restart(self.worker.generation());
                    self.worker
                        .execute(host_request_id, worker_operation)
                        .map_err(FaultRecord::mark_retried)
                } else {
                    Err(fault)
                }
            }
        };

        let completed = forwarded_request_id
            .as_ref()
            .and_then(|request_id| self.session_kernel.take_completed_request(request_id));
        self.record_worker_tool_completion(
            forwarded_request_id.as_ref(),
            completed.as_ref(),
            elapsed_ms(started_at.elapsed()),
            replay_attempts,
            outcome.as_ref().err(),
        );
        outcome
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
                    .refresh_binding(resolved.binding.project_root.clone());
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
                let rollout = if self.binary.rollout_pending().map_err(|error| {
                    FaultRecord::rollout(generation, &operation, error.to_string())
                })? {
                    RolloutState::Pending
                } else {
                    RolloutState::Stable
                };
                let health = self.telemetry.health_snapshot(rollout);
                tool_success(
                    system_health_output(
                        &health,
                        self.binding.as_ref(),
                        self.worker.is_alive(),
                        self.binary.launch_path_stable,
                        generation,
                    )?,
                    presentation,
                    generation,
                    FaultStage::Host,
                    &operation,
                )
            }
            "system.telemetry" => {
                let snapshot = self.telemetry.telemetry_snapshot();
                tool_success(
                    system_telemetry_output(&snapshot, self.telemetry.host_rollouts(), generation)?,
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
        self.session_kernel
            .initialization_seed()
            .is_some_and(|seed| seed.initialized_notification.is_some())
    }

    fn seed_captured(&self) -> bool {
        self.session_kernel.initialization_seed().is_some()
    }

    fn allocate_request_id(&mut self) -> HostRequestId {
        let id = HostRequestId(self.next_request_id);
        self.next_request_id += 1;
        id
    }

    fn maybe_roll_forward(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let binary_pending = self.binary.rollout_pending()?;
        if !self.rollout_requested && !binary_pending {
            return Ok(());
        }
        if binary_pending && !self.rollout_requested {
            self.telemetry.record_rollout();
        }
        self.roll_forward()
    }

    fn roll_forward(&mut self) -> Result<(), Box<dyn std::error::Error>> {
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
        let state_path = write_snapshot_file("jira-at-home-mcp-host-reexec", &state)?;
        let mut command = Command::new(&self.binary.path);
        let _ = command.arg("mcp").arg("serve");
        if let Some(project) = self.initial_project.as_ref() {
            let _ = command.arg("--project").arg(project);
        }
        let _ = command.env(HOST_STATE_ENV, &state_path);
        #[cfg(unix)]
        {
            let error = command.exec();
            let _ = remove_snapshot_file(&state_path);
            Err(Box::new(error))
        }
        #[cfg(not(unix))]
        {
            let _ = remove_snapshot_file(&state_path);
            Err(Box::new(io::Error::new(
                io::ErrorKind::Unsupported,
                "host rollout requires unix exec support",
            )))
        }
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

    fn record_host_tool_completion(
        &mut self,
        request_frame: &FramedMessage,
        request_id: Option<&RequestId>,
        latency_ms: u64,
        fault: Option<&FaultRecord>,
    ) {
        let Some(request_id) = request_id else {
            return;
        };
        let Some(tool_meta) = libmcp::parse_tool_call_meta(request_frame, "tools/call") else {
            return;
        };
        self.record_tool_completion(request_id, &tool_meta, latency_ms, 0, fault);
    }

    fn record_worker_tool_completion(
        &mut self,
        request_id: Option<&RequestId>,
        completed: Option<&libmcp::CompletedPendingRequest>,
        latency_ms: u64,
        replay_attempts: u8,
        fault: Option<&FaultRecord>,
    ) {
        let Some(request_id) = request_id else {
            return;
        };
        let Some(completed) = completed else {
            return;
        };
        let Some(tool_meta) = completed.request.tool_call_meta.as_ref() else {
            return;
        };
        self.record_tool_completion(request_id, tool_meta, latency_ms, replay_attempts, fault);
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
            issues_root: layout.issues_root.clone(),
            state_root: layout.state_root.clone(),
        },
        status: ProjectBindStatus {
            requested_path: requested_path.display().to_string(),
            project_root: layout.project_root.display().to_string(),
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
    Ok(load_snapshot_file_from_env(HOST_STATE_ENV)?)
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
    if status.requested_path != status.project_root {
        let _ = concise.insert("requested_path".to_owned(), json!(status.requested_path));
    }
    fallback_detailed_tool_output(
        &Value::Object(concise),
        status,
        [
            format!("bound project {}", status.project_root),
            format!("issues: {}", status.issues_root),
            format!("state: {}", status.state_root),
            format!("issues tracked: {}", status.issue_count),
        ]
        .join("\n"),
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
    }
    let full = json!({
        "health": health,
        "binding": binding.map(|binding| json!({
            "requested_path": binding.requested_path.display().to_string(),
            "project_root": binding.project_root.display().to_string(),
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
    host_rollouts: u64,
    generation: Generation,
) -> Result<ToolOutput, FaultRecord> {
    let hot_methods = telemetry.methods.iter().take(6).collect::<Vec<_>>();
    let concise = json!({
        "requests": telemetry.totals.request_count,
        "successes": telemetry.totals.success_count,
        "response_errors": telemetry.totals.response_error_count,
        "transport_faults": telemetry.totals.transport_fault_count,
        "retries": telemetry.totals.retry_count,
        "worker_restarts": telemetry.restart_count,
        "host_rollouts": host_rollouts,
        "hot_methods": hot_methods.iter().map(|method| json!({
            "method": method.method,
            "requests": method.request_count,
            "response_errors": method.response_error_count,
            "transport_faults": method.transport_fault_count,
            "retries": method.retry_count,
        })).collect::<Vec<_>>(),
    });
    let full = json!({
        "telemetry": telemetry,
        "host_rollouts": host_rollouts,
    });
    let mut lines = vec![format!(
        "requests={} success={} response_error={} transport_fault={} retry={}",
        telemetry.totals.request_count,
        telemetry.totals.success_count,
        telemetry.totals.response_error_count,
        telemetry.totals.transport_fault_count,
        telemetry.totals.retry_count
    )];
    lines.push(format!(
        "worker_restarts={} host_rollouts={host_rollouts}",
        telemetry.restart_count,
    ));
    if !hot_methods.is_empty() {
        lines.push("hot methods:".to_owned());
        for method in hot_methods {
            lines.push(format!(
                "{} req={} err={} transport={} retry={}",
                method.method,
                method.request_count,
                method.response_error_count,
                method.transport_fault_count,
                method.retry_count,
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

fn request_id_from_frame(frame: &FramedMessage) -> Option<RequestId> {
    match frame.classify() {
        libmcp::RpcEnvelopeKind::Request { id, .. } => Some(id),
        libmcp::RpcEnvelopeKind::Notification { .. }
        | libmcp::RpcEnvelopeKind::Response { .. }
        | libmcp::RpcEnvelopeKind::Unknown => None,
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
    serde_json::to_writer(&mut *stdout, message)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

fn elapsed_ms(duration: std::time::Duration) -> u64 {
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
        }
    }
}
