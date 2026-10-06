//! `hearth tui`. [`run_shell`] is the supported workspace UI (Ratatui).
//! [`run_tui`] is a deprecated ANSI single-project leftover kept for reference; the binary
//! only calls [`run_shell`].
pub mod actions;
pub mod client;
pub mod desk;
mod profile;
pub mod run;
mod schedule;
pub mod screen;
pub mod shell;
pub mod state;
pub mod text_utils;

pub use shell::{run_shell, ShellOptions, SpawnHook};

pub use actions::{keyboard_action, TuiAction};
pub use client::{
    append_sse_chunk, is_sse_comment, next_sse_frame, parse_sse, refresh_selected_log, EventReplay,
    ManagerTuiClient, SseFrame, WatchEvent, LOG_TAIL_BYTES, MAX_SSE_FRAME_BYTES,
};
#[allow(deprecated)]
pub use run::{run_tui, RunTuiOptions};
pub use screen::{ScreenUpdate, ServiceScreen, Viewport};
pub use state::{
    bounded_tail, display_state, no_service_kind, service_from_lifecycle, LogCursor, LogSlice,
    Service, ServiceKindLookup, ServiceSelection, TuiFence, TuiState,
};
pub use text_utils::{sanitize_terminal_text, truncate_to_width, visible_width};
