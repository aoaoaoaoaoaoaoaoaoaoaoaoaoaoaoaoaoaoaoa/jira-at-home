use std::time::{Duration, SystemTime, UNIX_EPOCH};

use libmcp::{
    Generation, HealthSnapshot, LifecycleState, MethodTelemetry, OperationalLedger, RolloutState,
    RpcMethod, TelemetrySnapshot, WorkerHandshakePhase,
};
use serde::{Deserialize, Serialize};

use crate::mcp::fault::{FaultRecord, FaultStage};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ServerTelemetry {
    started_unix_ms: u64,
    host_rollouts: u64,
    ledger: OperationalLedger,
}

impl Default for ServerTelemetry {
    fn default() -> Self {
        Self {
            started_unix_ms: unix_ms_now(),
            host_rollouts: 0,
            ledger: OperationalLedger::new(Generation::genesis()),
        }
    }
}

impl ServerTelemetry {
    pub(crate) fn record_request(&mut self, operation: &str) {
        exact(&self.ledger.record_request(method(operation)));
    }

    pub(crate) fn record_success(&mut self, operation: &str, latency_ms: u64) {
        exact(&self.ledger.record_success(&method(operation), latency_ms));
    }

    pub(crate) fn record_error(&mut self, operation: &str, fault: &FaultRecord, latency_ms: u64) {
        let method = method(operation);
        let result = if recovery_error(fault) {
            self.ledger.record_recovery_error(&method, fault.message())
        } else if matches!(fault.stage, FaultStage::Worker | FaultStage::Store) {
            self.ledger
                .record_response_error(&method, latency_ms, fault.message())
        } else {
            self.ledger
                .record_error(&method, latency_ms, fault.message())
        };
        exact(&result);
    }

    pub(crate) fn record_recovery_fault(&mut self, operation: &str, fault: &FaultRecord) {
        exact(
            &self
                .ledger
                .record_recovery_fault(Some(&method(operation)), fault.fault.clone()),
        );
    }

    pub(crate) fn record_replay(&mut self, operation: &str) {
        exact(&self.ledger.record_replay(&method(operation)));
    }

    pub(crate) fn replace_worker(&mut self, generation: Generation) {
        exact(&self.ledger.replace_worker(generation));
    }

    pub(crate) fn record_rollout(&mut self) {
        self.host_rollouts = self.host_rollouts.checked_add(1).unwrap_or_else(|| {
            std::process::abort();
        });
    }

    pub(crate) const fn host_rollouts(&self) -> u64 {
        self.host_rollouts
    }

    pub(crate) fn health_snapshot(
        &self,
        rollout: RolloutState,
        worker_alive: bool,
    ) -> HealthSnapshot {
        let (state, handshake) = worker_state(worker_alive);
        self.ledger
            .health_snapshot(self.uptime_ms(), state, handshake, Some(rollout))
    }

    pub(crate) fn telemetry_snapshot(&self, worker_alive: bool) -> TelemetrySnapshot {
        let (state, handshake) = worker_state(worker_alive);
        self.ledger
            .telemetry_snapshot(self.uptime_ms(), state, handshake)
    }

    pub(crate) fn ranked_methods(&self, worker_alive: bool) -> Vec<MethodTelemetry> {
        let mut methods = self.telemetry_snapshot(worker_alive).methods;
        methods.sort_by(|left, right| {
            right
                .request_count()
                .cmp(&left.request_count())
                .then_with(|| {
                    right
                        .recovery_fault_count()
                        .cmp(&left.recovery_fault_count())
                })
                .then_with(|| right.error_count().cmp(&left.error_count()))
                .then_with(|| left.method().cmp(right.method()))
        });
        methods
    }

    fn uptime_ms(&self) -> u64 {
        unix_ms_now().saturating_sub(self.started_unix_ms)
    }
}

fn recovery_error(fault: &FaultRecord) -> bool {
    matches!(
        fault.fault.class,
        libmcp::FaultClass::Transport
            | libmcp::FaultClass::Process
            | libmcp::FaultClass::Timeout
            | libmcp::FaultClass::Replay
            | libmcp::FaultClass::Rollout
            | libmcp::FaultClass::AmbiguousOutcome
    )
}

fn worker_state(worker_alive: bool) -> (LifecycleState, WorkerHandshakePhase) {
    if worker_alive {
        (LifecycleState::Ready, WorkerHandshakePhase::Ready)
    } else {
        (LifecycleState::Cold, WorkerHandshakePhase::Absent)
    }
}

fn method(operation: &str) -> RpcMethod {
    RpcMethod::try_new(operation).unwrap_or_else(|_| {
        std::process::abort();
    })
}

fn exact(result: &Result<(), libmcp::OperationalMetricError>) {
    if result.is_err() {
        std::process::abort();
    }
}

fn unix_ms_now() -> u64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let millis = duration.as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}
