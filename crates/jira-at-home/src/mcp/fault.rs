use libmcp::{
    Fault, FaultClass, FaultCode, Generation, HostRejection, RecoveryHint, ToolErrorDetail,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum FaultStage {
    Host,
    Worker,
    Store,
    Transport,
    Protocol,
    Rollout,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct FaultRecord {
    pub(crate) fault: Fault,
    pub(crate) stage: FaultStage,
    pub(crate) operation: String,
    pub(crate) jsonrpc_code: i64,
    pub(crate) retryable: bool,
    pub(crate) retried: bool,
}

impl FaultRecord {
    pub(crate) fn parse_error(
        generation: Generation,
        operation: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(
            generation,
            FaultClass::Protocol,
            "parse_error",
            None,
            FaultStage::Protocol,
            operation,
            detail,
            -32700,
        )
    }

    pub(crate) fn invalid_request(
        generation: Generation,
        operation: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(
            generation,
            FaultClass::Protocol,
            "invalid_request",
            None,
            FaultStage::Protocol,
            operation,
            detail,
            -32600,
        )
    }

    pub(crate) fn host_rejection(
        generation: Generation,
        stage: FaultStage,
        operation: impl Into<String>,
        rejection: HostRejection,
    ) -> Self {
        let (class, code) = match rejection {
            HostRejection::QueueOverflow => (FaultClass::Resource, "queue_overflow"),
            HostRejection::ReplayBudgetExhausted => (FaultClass::Replay, "replay_budget_exhausted"),
            HostRejection::DuplicateRequestId => (FaultClass::Protocol, "duplicate_request_id"),
            HostRejection::PendingCapacityExhausted => {
                (FaultClass::Resource, "pending_capacity_exhausted")
            }
            HostRejection::InvalidRequestFrame => (FaultClass::Protocol, "invalid_request_frame"),
            HostRejection::AmbiguousOutcome => (FaultClass::AmbiguousOutcome, "ambiguous_outcome"),
            HostRejection::RequestNotPending => (FaultClass::Invariant, "request_not_pending"),
            HostRejection::InvalidExecutionState => {
                (FaultClass::Invariant, "invalid_execution_state")
            }
        };
        Self::new(
            generation,
            class,
            code,
            None,
            stage,
            operation,
            rejection.message(),
            rejection.code(),
        )
    }

    pub(crate) fn invalid_input(
        generation: Generation,
        stage: FaultStage,
        operation: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(
            generation,
            FaultClass::Protocol,
            "invalid_input",
            None,
            stage,
            operation,
            detail,
            -32602,
        )
    }

    pub(crate) fn not_initialized(
        generation: Generation,
        stage: FaultStage,
        operation: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(
            generation,
            FaultClass::Protocol,
            "not_initialized",
            None,
            stage,
            operation,
            detail,
            -32002,
        )
    }

    pub(crate) fn unavailable(
        generation: Generation,
        stage: FaultStage,
        operation: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(
            generation,
            FaultClass::Resource,
            "unavailable",
            None,
            stage,
            operation,
            detail,
            -32004,
        )
    }

    pub(crate) fn transport(
        generation: Generation,
        stage: FaultStage,
        operation: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(
            generation,
            FaultClass::Transport,
            "transport_failure",
            Some(RecoveryHint::ReplaceWorker),
            stage,
            operation,
            detail,
            -32603,
        )
    }

    pub(crate) fn process(
        generation: Generation,
        stage: FaultStage,
        operation: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(
            generation,
            FaultClass::Process,
            "process_failure",
            Some(RecoveryHint::ReplaceWorker),
            stage,
            operation,
            detail,
            -32603,
        )
    }

    pub(crate) fn internal(
        generation: Generation,
        stage: FaultStage,
        operation: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(
            generation,
            FaultClass::Invariant,
            "internal_failure",
            None,
            stage,
            operation,
            detail,
            -32603,
        )
    }

    pub(crate) fn rollout(
        generation: Generation,
        operation: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self::new(
            generation,
            FaultClass::Rollout,
            "rollout_failure",
            Some(RecoveryHint::RollForward),
            FaultStage::Rollout,
            operation,
            detail,
            -32603,
        )
    }

    pub(crate) fn mark_retried(mut self) -> Self {
        self.retried = true;
        self
    }

    pub(crate) fn message(&self) -> &str {
        self.fault.detail.as_str()
    }

    pub(crate) fn error_detail(&self) -> ToolErrorDetail {
        ToolErrorDetail {
            code: Some(self.jsonrpc_code),
            kind: Some(self.fault.code.as_str().to_owned()),
            message: Some(self.message().to_owned()),
        }
    }

    pub(crate) fn into_jsonrpc_error(self) -> Value {
        json!({
            "code": self.jsonrpc_code,
            "message": self.message(),
            "data": self,
        })
    }

    pub(crate) fn into_tool_result(self) -> Value {
        json!({
            "content": [{
                "type": "text",
                "text": self.message(),
            }],
            "structuredContent": self,
            "isError": true,
        })
    }

    fn new(
        generation: Generation,
        class: FaultClass,
        code: &'static str,
        recovery_hint: Option<RecoveryHint>,
        stage: FaultStage,
        operation: impl Into<String>,
        detail: impl Into<String>,
        jsonrpc_code: i64,
    ) -> Self {
        let retryable = matches!(recovery_hint, Some(RecoveryHint::ReplaceWorker));
        let fault = Fault::new(generation, class, fault_code(code), recovery_hint, detail);
        Self {
            retryable,
            fault,
            stage,
            operation: operation.into(),
            jsonrpc_code,
            retried: false,
        }
    }
}

fn fault_code(code: &'static str) -> FaultCode {
    match FaultCode::try_new(code.to_owned()) {
        Ok(value) => value,
        Err(_) => std::process::abort(),
    }
}
