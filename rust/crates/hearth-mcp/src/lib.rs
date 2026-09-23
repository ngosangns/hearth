//! Port of `src/mcp/mcp-server.ts` (Phase 7 of the Rust-rewrite plan) — five read-mostly MCP tools
//! (`status`, `logs`, `trace`, `events`, `manage`) over the daemon's HTTP API, built on `rmcp`. See
//! `server.rs`'s module doc comment for why this hand-implements `ServerHandler` rather than using
//! `rmcp`'s declarative `#[tool]`/`#[tool_router]` macros.
pub mod client;
pub mod server;

pub use client::{EventsArguments, HearthMcpClient, LogsArguments, ManageAction, ManageArguments, ManagerApiClient, StatusArguments, TraceArguments};
pub use server::{create_hearth_mcp_server, CreateHearthMcpServerOptions, HearthMcpServer};

pub use hearth_core::state::PROTOCOL_VERSION as MCP_PROTOCOL_VERSION;
