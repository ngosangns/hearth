//! `hearth tui`. [`run_shell`] is the workspace UI (Ratatui).
pub mod client;
pub mod desk;
mod profile;
mod schedule;
pub mod shell;
pub mod state;
pub mod text_utils;

pub use shell::{run_shell, ShellOptions, SpawnHook};

pub use client::{
    EventReplay, ManagerTuiClient, WatchEvent, LOG_TAIL_BYTES, MAX_SSE_FRAME_BYTES,
};
pub use state::{
    bounded_tail, display_state, no_service_kind, service_from_lifecycle, LogCursor, LogSlice,
    Service, ServiceKindLookup, ServiceSelection, TuiFence, TuiState,
};
pub use text_utils::{sanitize_terminal_text, visible_width};
