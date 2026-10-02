//! `hearth mcp`: MCP tools (`status`, `logs`, `trace`, `events`, `manage`, daemon lifecycle and
//! shared services) over the daemon's HTTP API, built on `rmcp`. See `server.rs` for why it
//! hand-implements `ServerHandler`.
pub mod client;
pub mod server;

pub use client::{EventsArguments, HearthMcpClient, LogsArguments, ManageArguments, ManagerApiClient, StatusArguments, TraceArguments};
pub use server::{create_hearth_mcp_server, CreateHearthMcpServerOptions, HearthMcpServer};
