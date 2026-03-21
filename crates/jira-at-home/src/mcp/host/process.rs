use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use libmcp::Generation;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::mcp::fault::{FaultRecord, FaultStage};
use crate::mcp::protocol::{
    HostRequestId, WorkerOperation, WorkerOutcome, WorkerRequest, WorkerResponse, WorkerSpawnConfig,
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct ProjectBinding {
    pub(super) requested_path: PathBuf,
    pub(super) project_root: PathBuf,
    pub(super) issues_root: PathBuf,
    pub(super) state_root: PathBuf,
}

pub(super) struct WorkerSupervisor {
    config: WorkerSpawnConfig,
    generation: Generation,
    has_spawned: bool,
    crash_before_reply_once: bool,
    bound_project_root: Option<PathBuf>,
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
            bound_project_root: None,
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

    pub(super) fn rebind(&mut self, project_root: PathBuf) {
        if self
            .bound_project_root
            .as_ref()
            .is_some_and(|current| current == &project_root)
        {
            return;
        }
        self.kill_current_worker();
        self.bound_project_root = Some(project_root);
    }

    pub(super) fn refresh_binding(&mut self, project_root: PathBuf) {
        self.kill_current_worker();
        self.bound_project_root = Some(project_root);
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
        serde_json::to_writer(&mut *stdin, &request).map_err(|error| {
            FaultRecord::transport(
                self.generation,
                FaultStage::Transport,
                "worker.write",
                format!("failed to encode worker request: {error}"),
            )
        })?;
        stdin.write_all(b"\n").map_err(|error| {
            FaultRecord::transport(
                self.generation,
                FaultStage::Transport,
                "worker.write",
                format!("failed to frame worker request: {error}"),
            )
        })?;
        stdin.flush().map_err(|error| {
            FaultRecord::transport(
                self.generation,
                FaultStage::Transport,
                "worker.write",
                format!("failed to flush worker request: {error}"),
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
        let mut line = String::new();
        let bytes = stdout.read_line(&mut line).map_err(|error| {
            FaultRecord::transport(
                self.generation,
                FaultStage::Transport,
                "worker.read",
                format!("failed to read worker response: {error}"),
            )
        })?;
        if bytes == 0 {
            self.kill_current_worker();
            return Err(FaultRecord::transport(
                self.generation,
                FaultStage::Transport,
                "worker.read",
                "worker exited before replying",
            ));
        }
        let response = serde_json::from_str::<WorkerResponse>(&line).map_err(|error| {
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
        let Some(project_root) = self.bound_project_root.as_ref() else {
            return Err(FaultRecord::unavailable(
                self.generation,
                FaultStage::Host,
                "worker.spawn",
                "project is not bound; call project.bind before using issue tools",
            ));
        };
        let generation = if self.has_spawned {
            self.generation.next()
        } else {
            self.generation
        };
        let mut child = Command::new(&self.config.executable)
            .arg("mcp")
            .arg("worker")
            .arg("--project")
            .arg(project_root)
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
