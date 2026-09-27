//! `hearthd tui`: a `crossterm`-based terminal app that is just another HTTP+SSE client of the
//! daemon, built on `hearth-cli`'s `ManagerClient`.
pub mod actions;
pub mod client;
pub mod run;
pub mod screen;
pub mod state;
pub mod text_utils;

pub use actions::{keyboard_action, TuiAction};
pub use client::{append_sse_chunk, is_sse_comment, next_sse_frame, parse_sse, refresh_selected_log, EventReplay, ManagerTuiClient, SseFrame, WatchEvent, LOG_TAIL_BYTES, MAX_SSE_FRAME_BYTES};
pub use run::{run_tui, RunTuiOptions};
pub use screen::{ScreenUpdate, ServiceScreen, Viewport};
pub use state::{bounded_tail, display_state, no_service_kind, service_from_lifecycle, LogCursor, LogSlice, Service, ServiceKindLookup, ServiceSelection, TuiFence, TuiState};
pub use text_utils::{sanitize_terminal_text, truncate_to_width, visible_width};
