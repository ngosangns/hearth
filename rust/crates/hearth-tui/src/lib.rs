//! Port of the `src/tui` subpath (Phase 6 of the Rust-rewrite plan) — a `crossterm`-based terminal
//! app that is just another HTTP+SSE client of the daemon, same as `hearth-cli`. See each module's own
//! doc comment for what was ported 1:1 versus deliberately adapted.
pub mod actions;
pub mod client;
pub mod run;
pub mod screen;
pub mod state;
pub mod text_utils;

pub use actions::{keyboard_action, TuiAction};
pub use client::{append_sse_chunk, parse_sse, refresh_selected_log, EventReplay, ManagerTuiClient, SseFrame, WatchEvent, LOG_TAIL_BYTES, MAX_SSE_FRAME_BYTES};
pub use run::{run_tui, RunTuiOptions};
pub use screen::{ScreenUpdate, ServiceScreen, Viewport};
pub use state::{bounded_tail, no_service_kind, service_from_lifecycle, ActionKind, LogCursor, LogSlice, Service, ServiceKindLookup, ServiceSelection, TuiFence, TuiState};
pub use text_utils::{sanitize_terminal_text, truncate_to_width, visible_width};
