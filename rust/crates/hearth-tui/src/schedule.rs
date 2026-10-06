//! When the shell may fetch and when it may draw.
//!
//! The input loop never waits on these decisions. A fetch either starts now, is folded into the
//! one already running, or waits until `due`. A draw from input happens immediately. A draw from
//! a watch event waits until the frame budget so a stream of log lines paints once per frame.
use std::time::{Duration, Instant};

pub const FRAME_BUDGET: Duration = Duration::from_millis(16);
/// At most one selected-log refetch per this interval while that service keeps logging.
pub const LOG_COALESCE_INTERVAL: Duration = Duration::from_millis(100);

/// At most one fetch in flight. A request that arrives during the flight is remembered and
/// [`FetchCoalescer::end`] reports that exactly one more fetch should start.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FetchCoalescer {
    inflight: bool,
    dirty: bool,
}

impl FetchCoalescer {
    /// Begin a fetch. `false` means one is already running and this request was coalesced.
    pub fn try_begin(&mut self) -> bool {
        if self.inflight {
            self.dirty = true;
            return false;
        }
        self.inflight = true;
        self.dirty = false;
        true
    }

    /// The in-flight fetch finished. `true` means a request arrived during it and another fetch
    /// should be considered.
    pub fn end(&mut self) -> bool {
        self.inflight = false;
        let again = self.dirty;
        self.dirty = false;
        again
    }

    /// Forget a follow-up recorded by [`FetchCoalescer::try_begin`] without touching `inflight`.
    pub fn forget_follow_up(&mut self) {
        self.dirty = false;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogAction {
    /// Nothing to do.
    Idle,
    /// Ask again at this instant. The fetch must not start yet.
    Wait(Instant),
    /// Start one fetch now.
    Start,
}

/// Coalesces selected-log refetches to one in flight and at most one per
/// [`LOG_COALESCE_INTERVAL`], unless the caller forces a selection change through.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogRefresh {
    fetch: FetchCoalescer,
    wanted: bool,
    force: bool,
    next: Option<Instant>,
}

impl LogRefresh {
    pub fn request(&mut self, now: Instant, immediate: bool) -> LogAction {
        self.wanted = true;
        if immediate {
            self.force = true;
        }
        self.poll(now)
    }

    pub fn poll(&mut self, now: Instant) -> LogAction {
        if !self.wanted {
            return LogAction::Idle;
        }
        if !self.force {
            if let Some(next) = self.next {
                if now < next {
                    return LogAction::Wait(next);
                }
            }
        }
        if !self.fetch.try_begin() {
            return LogAction::Idle;
        }
        self.wanted = false;
        self.force = false;
        self.next = Some(now + LOG_COALESCE_INTERVAL);
        LogAction::Start
    }

    /// The fetch started by [`LogAction::Start`] finished.
    pub fn finish(&mut self, now: Instant) -> LogAction {
        if self.fetch.end() {
            self.wanted = true;
        }
        self.poll(now)
    }

    /// Drop a scheduled refetch. An in-flight fetch still completes, and its follow-up is forgotten.
    pub fn cancel(&mut self) {
        self.wanted = false;
        self.force = false;
        self.fetch.forget_follow_up();
    }
}

/// Coalesces non-input redraws to one per [`FRAME_BUDGET`]. Input draws are immediate and start
/// a new budget so a burst of watch events just after a key does not paint again immediately.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FrameScheduler {
    dirty: bool,
    due: Option<Instant>,
    last: Option<Instant>,
}

impl FrameScheduler {
    /// Something changed off the input path. Returns when the caller should draw.
    pub fn mark(&mut self, now: Instant) -> Option<Instant> {
        if self.dirty {
            return self.due;
        }
        self.dirty = true;
        let due = self
            .last
            .map(|last| last + FRAME_BUDGET)
            .unwrap_or(now)
            .max(now);
        self.due = Some(due);
        Some(due)
    }

    pub fn due(&self) -> Option<Instant> {
        self.due
    }

    /// An input draw just happened.
    pub fn input(&mut self, now: Instant) {
        self.dirty = false;
        self.due = None;
        self.last = Some(now);
    }

    /// `true` when a coalesced draw is due and the dirty flag was consumed.
    pub fn poll(&mut self, now: Instant) -> bool {
        if !self.dirty || self.due.is_some_and(|due| now < due) {
            return false;
        }
        self.dirty = false;
        self.due = None;
        self.last = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn fetch_coalescer_collapses_requests_that_arrive_while_one_is_running() {
        let mut fetch = FetchCoalescer::default();
        assert!(fetch.try_begin());
        assert!(!fetch.try_begin());
        assert!(!fetch.try_begin());
        assert!(fetch.end());
        assert!(fetch.try_begin());
        assert!(!fetch.end());
    }

    #[test]
    fn log_refresh_starts_immediately_then_at_most_once_per_interval() {
        let mut refresh = LogRefresh::default();
        let start = t0();
        assert_eq!(refresh.request(start, false), LogAction::Start);
        assert_eq!(
            refresh.request(start + Duration::from_millis(20), false),
            LogAction::Wait(start + LOG_COALESCE_INTERVAL)
        );
        assert_eq!(
            refresh.poll(start + Duration::from_millis(20)),
            LogAction::Wait(start + LOG_COALESCE_INTERVAL)
        );
        // The in-flight fetch finishing does not drop the request that arrived during the interval.
        assert_eq!(
            refresh.finish(start + Duration::from_millis(30)),
            LogAction::Wait(start + LOG_COALESCE_INTERVAL)
        );
        assert_eq!(
            refresh.poll(start + LOG_COALESCE_INTERVAL),
            LogAction::Start
        );
    }

    #[test]
    fn log_refresh_forces_a_selection_change_ahead_of_the_interval() {
        let mut refresh = LogRefresh::default();
        let start = t0();
        assert_eq!(refresh.request(start, false), LogAction::Start);
        assert_eq!(
            refresh.request(start + Duration::from_millis(10), true),
            LogAction::Idle
        );
        assert_eq!(
            refresh.finish(start + Duration::from_millis(12)),
            LogAction::Start
        );
    }

    #[test]
    fn log_refresh_keeps_one_follow_up_when_many_events_arrive_in_flight() {
        let mut refresh = LogRefresh::default();
        let start = t0();
        assert_eq!(refresh.request(start, false), LogAction::Start);
        for step in 1..30 {
            let now = start + Duration::from_millis(step);
            assert_eq!(
                refresh.request(now, false),
                LogAction::Wait(start + LOG_COALESCE_INTERVAL)
            );
        }
        assert_eq!(
            refresh.finish(start + Duration::from_millis(40)),
            LogAction::Wait(start + LOG_COALESCE_INTERVAL)
        );
        assert_eq!(
            refresh.poll(start + LOG_COALESCE_INTERVAL),
            LogAction::Start
        );
        assert_eq!(
            refresh.finish(start + LOG_COALESCE_INTERVAL),
            LogAction::Idle
        );
    }

    #[test]
    fn log_refresh_cancel_drops_a_pending_refetch() {
        let mut refresh = LogRefresh::default();
        let start = t0();
        assert_eq!(refresh.request(start, false), LogAction::Start);
        assert_eq!(
            refresh.request(start + Duration::from_millis(10), true),
            LogAction::Idle
        );
        refresh.cancel();
        assert_eq!(
            refresh.finish(start + Duration::from_millis(12)),
            LogAction::Idle
        );
    }

    #[test]
    fn frame_scheduler_draws_input_immediately_and_coalesces_the_following_events() {
        let mut frames = FrameScheduler::default();
        let start = t0();
        frames.input(start);
        let due = frames.mark(start + Duration::from_millis(1)).unwrap();
        assert_eq!(due, start + FRAME_BUDGET);
        assert!(!frames.poll(start + Duration::from_millis(10)));
        assert!(frames.mark(start + Duration::from_millis(12)).is_some());
        assert_eq!(frames.due(), Some(start + FRAME_BUDGET));
        assert!(frames.poll(start + FRAME_BUDGET));
        assert!(!frames.poll(start + FRAME_BUDGET + Duration::from_millis(1)));
    }
}
