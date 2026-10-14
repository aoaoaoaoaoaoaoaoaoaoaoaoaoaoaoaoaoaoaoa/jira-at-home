use std::io::{BufReader, BufWriter};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use libmcp::{FrameLimit, FrameReadOutcome, Generation, read_frame_blocking};
use serde_json::Value;

use crate::mcp::fault::{FaultRecord, FaultStage};
use crate::mcp::protocol::{
    HostRequestId, WorkerOperation, WorkerOutcome, WorkerRequest, WorkerResponse,
    WorkerSpawnConfig, write_sync_json_frame,
};

pub(super) struct WorkerSupervisor {
    config: WorkerSpawnConfig,
    generation: Generation,
    has_spawned: bool,
    crash_before_reply_once: bool,
    child: Option<Child>,
    stdin: Option<BufWriter<ChildStdin>>,
    stdout: Option<BufReader<ChildStdout>>,
}

impl WorkerSupervisor {
    pub(super) fn new(
        config: WorkerSpawnConfig,
        generation: Generation,
        has_spawned: bool,
    ) -> Self {
        Self {
            config,
            generation,
            has_spawned,
            crash_before_reply_once: false,
            child: None,
            stdin: None,
            stdout: None,
        }
    }

    pub(super) fn generation(&self) -> Generation {
        self.generation
    }

    pub(super) fn has_spawned(&self) -> bool {
        self.has_spawned
    }

    pub(super) fn execute(
        &mut self,
        request_id: HostRequestId,
        operation: WorkerOperation,
    ) -> Result<Value, FaultRecord> {
        self.ensure_worker()?;
        let request = WorkerRequest::Execute {
            id: request_id,
            operation,
        };
        let stdin = self.stdin.as_mut().ok_or_else(|| {
            FaultRecord::transport(
                self.generation,
                FaultStage::Transport,
                "worker.stdin",
                "worker stdin is not available",
            )
        })?;
        write_sync_json_frame(stdin, &request, FrameLimit::DEFAULT).map_err(|error| {
            FaultRecord::transport(
                self.generation,
                FaultStage::Transport,
                "worker.write",
                format!("failed to write worker request: {error}"),
            )
        })?;

        if self.crash_before_reply_once {
            self.crash_before_reply_once = false;
            self.kill_current_worker();
            return Err(FaultRecord::transport(
                self.generation,
                FaultStage::Transport,
                "worker.read",
                "worker crashed before replying",
            ));
        }

        let stdout = self.stdout.as_mut().ok_or_else(|| {
            FaultRecord::transport(
                self.generation,
                FaultStage::Transport,
                "worker.stdout",
                "worker stdout is not available",
            )
        })?;
        let frame = read_frame_blocking(stdout, FrameLimit::DEFAULT).map_err(|error| {
            FaultRecord::transport(
                self.generation,
                FaultStage::Transport,
                "worker.read",
                format!("failed to read worker response: {error}"),
            )
        })?;
        let FrameReadOutcome::Frame(payload) = frame else {
            self.kill_current_worker();
            return Err(FaultRecord::transport(
                self.generation,
                FaultStage::Transport,
                "worker.read",
                "worker exited before replying",
            ));
        };
        let response = serde_json::from_slice::<WorkerResponse>(&payload).map_err(|error| {
            FaultRecord::transport(
                self.generation,
                FaultStage::Transport,
                "worker.read",
                format!("invalid worker response: {error}"),
            )
        })?;
        match response.outcome {
            WorkerOutcome::Success { result } => Ok(result),
            WorkerOutcome::Fault { fault } => Err(fault),
        }
    }

    pub(super) fn ensure_ready(&mut self) -> Result<(), FaultRecord> {
        self.ensure_worker()
    }

    pub(super) fn restart(&mut self) -> Result<(), FaultRecord> {
        self.kill_current_worker();
        self.ensure_worker()
    }

    pub(super) fn is_alive(&mut self) -> bool {
        let Some(child) = self.child.as_mut() else {
            return false;
        };
        if let Ok(None) = child.try_wait() {
            true
        } else {
            self.child = None;
            self.stdin = None;
            self.stdout = None;
            false
        }
    }

    pub(super) fn arm_crash_once(&mut self) {
        self.crash_before_reply_once = true;
    }

    fn ensure_worker(&mut self) -> Result<(), FaultRecord> {
        if self.is_alive() {
            return Ok(());
        }
        let generation = if self.has_spawned {
            self.generation.next().map_err(|error| {
                FaultRecord::internal(
                    self.generation,
                    FaultStage::Host,
                    "worker.spawn",
                    error.to_string(),
                )
            })?
        } else {
            self.generation
        };
        let mut child = Command::new(&self.config.executable)
            .arg("mcp")
            .arg("worker")
            .arg("--generation")
            .arg(generation.get().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| {
                FaultRecord::process(
                    generation,
                    FaultStage::Transport,
                    "worker.spawn",
                    format!("failed to spawn worker: {error}"),
                )
            })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            FaultRecord::internal(
                generation,
                FaultStage::Transport,
                "worker.spawn",
                "worker stdin pipe was not created",
            )
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            FaultRecord::internal(
                generation,
                FaultStage::Transport,
                "worker.spawn",
                "worker stdout pipe was not created",
            )
        })?;
        self.generation = generation;
        self.has_spawned = true;
        self.child = Some(child);
        self.stdin = Some(BufWriter::new(stdin));
        self.stdout = Some(BufReader::new(stdout));
        Ok(())
    }

    fn kill_current_worker(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child = None;
        self.stdin = None;
        self.stdout = None;
    }
}
