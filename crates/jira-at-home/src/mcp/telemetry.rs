use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use libmcp::{
    Fault, Generation, HealthSnapshot, LifecycleState, MethodTelemetry, RolloutState,
    TelemetrySnapshot, TelemetryTotals,
};
use serde::{Deserialize, Serialize};

use crate::mcp::fault::FaultRecord;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
struct MethodStats {
    request_count: u64,
    success_count: u64,
    response_error_count: u64,
    transport_fault_count: u64,
    retry_count: u64,
    total_latency_ms: u128,
    max_latency_ms: u64,
    last_latency_ms: Option<u64>,
    last_error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ServerTelemetry {
    started_unix_ms: u64,
    state: LifecycleState,
    generation: Generation,
    consecutive_failures: u32,
    restart_count: u64,
    host_rollouts: u64,
    totals: TelemetryTotals,
    methods: BTreeMap<String, MethodStats>,
    last_fault: Option<Fault>,
}

impl Default for ServerTelemetry {
    fn default() -> Self {
        Self {
            started_unix_ms: unix_ms_now(),
            state: LifecycleState::Cold,
            generation: Generation::genesis(),
            consecutive_failures: 0,
            restart_count: 0,
            host_rollouts: 0,
            totals: TelemetryTotals {
                request_count: 0,
                success_count: 0,
                response_error_count: 0,
                transport_fault_count: 0,
                retry_count: 0,
            },
            methods: BTreeMap::new(),
            last_fault: None,
        }
    }
}

impl ServerTelemetry {
    pub(crate) fn record_request(&mut self, operation: &str) {
        self.totals.request_count += 1;
        self.methods
            .entry(operation.to_owned())
            .or_default()
            .request_count += 1;
    }

    pub(crate) fn record_success(
        &mut self,
        operation: &str,
        latency_ms: u64,
        generation: Generation,
        worker_alive: bool,
    ) {
        self.generation = generation;
        self.state = if worker_alive {
            LifecycleState::Ready
        } else {
            LifecycleState::Cold
        };
        self.consecutive_failures = 0;
        self.last_fault = None;
        self.totals.success_count += 1;
        let entry = self.methods.entry(operation.to_owned()).or_default();
        entry.success_count += 1;
        entry.total_latency_ms = entry
            .total_latency_ms
            .saturating_add(u128::from(latency_ms));
        entry.max_latency_ms = entry.max_latency_ms.max(latency_ms);
        entry.last_latency_ms = Some(latency_ms);
        entry.last_error = None;
    }

    pub(crate) fn record_error(
        &mut self,
        operation: &str,
        fault: &FaultRecord,
        latency_ms: u64,
        generation: Generation,
    ) {
        self.generation = generation;
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.last_fault = Some(fault.fault.clone());
        let transportish = matches!(
            fault.fault.class,
            libmcp::FaultClass::Transport
                | libmcp::FaultClass::Process
                | libmcp::FaultClass::Timeout
                | libmcp::FaultClass::Resource
                | libmcp::FaultClass::Replay
                | libmcp::FaultClass::Rollout
        );
        if transportish {
            self.state = LifecycleState::Recovering;
            self.totals.transport_fault_count += 1;
        } else {
            self.totals.response_error_count += 1;
        }
        let entry = self.methods.entry(operation.to_owned()).or_default();
        if transportish {
            entry.transport_fault_count += 1;
        } else {
            entry.response_error_count += 1;
        }
        entry.total_latency_ms = entry
            .total_latency_ms
            .saturating_add(u128::from(latency_ms));
        entry.max_latency_ms = entry.max_latency_ms.max(latency_ms);
        entry.last_latency_ms = Some(latency_ms);
        entry.last_error = Some(fault.message().to_owned());
    }

    pub(crate) fn record_retry(&mut self, operation: &str) {
        self.totals.retry_count += 1;
        self.methods
            .entry(operation.to_owned())
            .or_default()
            .retry_count += 1;
    }

    pub(crate) fn record_worker_restart(&mut self, generation: Generation) {
        self.generation = generation;
        self.restart_count += 1;
        self.state = LifecycleState::Recovering;
    }

    pub(crate) fn record_rollout(&mut self) {
        self.host_rollouts += 1;
    }

    pub(crate) fn host_rollouts(&self) -> u64 {
        self.host_rollouts
    }

    pub(crate) fn health_snapshot(&self, rollout: RolloutState) -> HealthSnapshot {
        HealthSnapshot {
            state: self.state,
            generation: self.generation,
            uptime_ms: self.uptime_ms(),
            consecutive_failures: self.consecutive_failures,
            restart_count: self.restart_count,
            rollout: Some(rollout),
            last_fault: self.last_fault.clone(),
        }
    }

    pub(crate) fn telemetry_snapshot(&self) -> TelemetrySnapshot {
        TelemetrySnapshot {
            uptime_ms: self.uptime_ms(),
            state: self.state,
            generation: self.generation,
            consecutive_failures: self.consecutive_failures,
            restart_count: self.restart_count,
            totals: self.totals.clone(),
            methods: self.ranked_methods(),
            last_fault: self.last_fault.clone(),
        }
    }

    pub(crate) fn ranked_methods(&self) -> Vec<MethodTelemetry> {
        let mut methods = self
            .methods
            .iter()
            .map(|(method, stats)| MethodTelemetry {
                method: method.clone(),
                request_count: stats.request_count,
                success_count: stats.success_count,
                response_error_count: stats.response_error_count,
                transport_fault_count: stats.transport_fault_count,
                retry_count: stats.retry_count,
                last_latency_ms: stats.last_latency_ms,
                max_latency_ms: stats.max_latency_ms,
                avg_latency_ms: average_latency_ms(stats),
                last_error: stats.last_error.clone(),
            })
            .collect::<Vec<_>>();
        methods.sort_by(|left, right| {
            right
                .request_count
                .cmp(&left.request_count)
                .then_with(|| right.transport_fault_count.cmp(&left.transport_fault_count))
                .then_with(|| right.response_error_count.cmp(&left.response_error_count))
                .then_with(|| left.method.cmp(&right.method))
        });
        methods
    }

    fn uptime_ms(&self) -> u64 {
        unix_ms_now().saturating_sub(self.started_unix_ms)
    }
}

fn average_latency_ms(stats: &MethodStats) -> u64 {
    if stats.request_count == 0 {
        return 0;
    }
    let average = stats.total_latency_ms / u128::from(stats.request_count);
    u64::try_from(average).unwrap_or(u64::MAX)
}

fn unix_ms_now() -> u64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    let millis = duration.as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}
