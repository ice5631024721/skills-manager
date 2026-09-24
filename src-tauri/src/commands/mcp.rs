//! Read-only MCP inventory command. Design of record: `docs/adr/0005` — v1
//! only detects which MCP servers exist in which supported agents; it never
//! writes to an agent config file.

use crate::core::mcp_inventory::{self, McpInventoryReport};

/// Scan supported agents (OpenCode, DeepSeek Harness) for configured MCP
/// servers. Never fails: per-agent read/parse problems are reported inside the
/// payload so one broken config cannot hide the others.
#[tauri::command]
pub async fn get_mcp_inventory() -> McpInventoryReport {
    mcp_inventory::scan_mcp_inventory()
}