use libmcp::{Fault, FaultClass, FaultCode, Generation, RecoveryDirective, ToolErrorDetail};
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
            RecoveryDirective::AbortRequest,
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
            RecoveryDirective::AbortRequest,
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
            RecoveryDirective::AbortRequest,
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
            RecoveryDirective::RestartAndReplay,
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
            RecoveryDirective::RestartAndReplay,
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
            RecoveryDirective::AbortRequest,
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
            RecoveryDirective::RestartAndReplay,
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
        directive: RecoveryDirective,
        stage: FaultStage,
        operation: impl Into<String>,
        detail: impl Into<String>,
        jsonrpc_code: i64,
    ) -> Self {
        let fault = Fault::new(generation, class, fault_code(code), directive, detail);
        Self {
            retryable: directive != RecoveryDirective::AbortRequest,
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
