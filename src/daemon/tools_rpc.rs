//! The `tools.*` requests: what the daemon answers about its helper programs.

use crate::daemon::core::DaemonCore;
use crate::ipc::protocol::{RpcError, ToolInfo, ToolRollbackParams, ToolUpdateParams};
use crate::tools::ToolStore;
use serde_json::Value;
use std::sync::Arc;

impl DaemonCore {
    /// The tools folder, if this daemon keeps one.
    pub fn tools(&self) -> Option<&Arc<ToolStore>> {
        self.tools.as_ref()
    }

    /// Use `store` for the `tools.*` requests instead of the default one, which
    /// fetches from GitHub. For tests.
    pub fn with_tools(mut self, store: Arc<ToolStore>) -> Self {
        self.tools = Some(store);
        self
    }

    fn tool_store(&self) -> Result<&Arc<ToolStore>, RpcError> {
        self.tools
            .as_ref()
            .ok_or_else(|| RpcError::tool_failed("This daemon has no tools folder."))
    }

    pub fn tools_status(&self) -> Result<Vec<ToolInfo>, RpcError> {
        Ok(self.tool_store()?.status())
    }

    /// `params` may be missing, which means every installed tool.
    pub fn tools_update(&self, params: Option<Value>) -> Result<Vec<ToolInfo>, RpcError> {
        let params: ToolUpdateParams = match params {
            None | Some(Value::Null) => ToolUpdateParams::default(),
            Some(value) => serde_json::from_value(value)
                .map_err(|e| RpcError::invalid_params(e.to_string()))?,
        };
        Ok(self.tool_store()?.start_update(params.tool.map(Into::into)))
    }

    pub fn tools_rollback(&self, params: ToolRollbackParams) -> Result<ToolInfo, RpcError> {
        self.tool_store()?
            .rollback(params.tool.into())
            .map_err(|e| RpcError::tool_failed(e.to_string()))
    }
}
