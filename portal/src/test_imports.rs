// Test file to check if imports work correctly
#[allow(unused_imports)]
use crate::config::PortalConfig;
#[allow(unused_imports)]
use crate::protocol::{JsonRpcRequest, JsonRpcResponse, JsonRpcError};
#[allow(unused_imports)]
use crate::tools::ToolHost;

// This function is never called, just tests if imports compile
#[allow(dead_code)]
fn test_imports() {
    let _config = PortalConfig::default();
    let _tool_host = ToolHost::new(&_config);
}