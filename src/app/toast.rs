//! Transient messages surfaced as bottom-right toasts.
//!
//! Multiple toasts can be live at once: up to `MAX_ACTIVE` are visible
//! (stacked) at any time, and overflow is buffered in `pending`. As an
//! active toast ages out (or a fatal is dismissed), the next pending
//! one is promoted and its TTL starts ticking from that moment — that
//! way a burst of messages plays back sequentially instead of dropping
//! everything that doesn't fit on screen at once.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Severity of a toast. Drives foreground color and lifetime: `Info`,
/// `Warn`, and `Error` all auto-expire after the standard TTL; `Fatal`
/// is sticky and only goes away when the user dismisses with `Esc`.
/// `Warn` is exposed for callers but not used in-tree yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    #[allow(dead_code)]
    Warn,
    Error,
    Fatal,
}

pub struct Toast {
    text: String,
    level: Level,
    shown_at: Instant,
}

impl Toast {
    pub fn info(s: impl Into<String>) -> Self {
        Self::new(s, Level::Info)
    }
    #[allow(dead_code)]
    pub fn warn(s: impl Into<String>) -> Self {
        Self::new(s, Level::Warn)
    }
    pub fn error(s: impl Into<String>) -> Self {
        Self::new(s, Level::Error)
    }
    #[allow(dead_code)]
    pub fn fatal(s: impl Into<String>) -> Self {
        Self::new(s, Level::Fatal)
    }
    fn new(s: impl Into<String>, level: Level) -> Self {
        Self {
            text: s.into(),
            level,
            shown_at: Instant::now(),
        }
    }
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn level(&self) -> Level {
        self.level
    }
    pub fn shown_at(&self) -> Instant {
        self.shown_at
    }
    /// Re-stamp `shown_at` to now. Called when promoting a toast from
    /// `pending` to `active` so its TTL starts when it first becomes
    /// visible, not when it was originally queued.
    fn reset_shown_at(&mut self) {
        self.shown_at = Instant::now();
    }
}

/// Hard cap on simultaneously-visible toasts. Anything past this gets
/// buffered until a slot frees up.
const MAX_ACTIVE: usize = 3;
/// Hard cap on the pending buffer. A flood of error messages from e.g.
/// a misbehaving LSP shouldn't grow this without bound; drop the
/// oldest pending entries past this.
const MAX_PENDING: usize = 32;
/// Lifetime for non-fatal toasts. Fatal toasts ignore this and stay
/// until the user hits `Esc`.
pub(crate) const TTL: Duration = Duration::from_secs(3);
/// Placeholder lifetime reported for fatal toasts. Long enough that
/// the main loop's `recv_timeout` effectively waits forever, short
/// enough to fit in `Duration` arithmetic without surprises.
const FATAL_REMAINING: Duration = Duration::from_secs(3600);

#[derive(Default)]
pub struct ToastQueue {
    active: Vec<Toast>,
    pending: VecDeque<Toast>,
}

impl ToastQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a toast. Empty-text toasts are dropped (callers use them
    /// as no-op placeholders). Goes straight to `active` while there's
    /// room; otherwise waits in `pending`.
    ///
    /// Consecutive duplicates collapse: if the most recently queued
    /// toast (pending tail, or active tail when pending is empty) has
    /// the same level + text as `t`, we just re-stamp its `shown_at`
    /// instead of stacking another copy. Otherwise mashing a key that
    /// pushes the same error (e.g. holding `.` with no last change)
    /// would fill `pending` past `MAX_ACTIVE` and take far longer than
    /// `TTL` to drain back to empty.
    pub fn push(&mut self, t: Toast) {
        if t.text().is_empty() {
            return;
        }
        let last = self.pending.back_mut().or_else(|| self.active.last_mut());
        if let Some(prev) = last
            && prev.level() == t.level()
            && prev.text() == t.text()
        {
            prev.reset_shown_at();
            return;
        }
        if self.active.len() < MAX_ACTIVE {
            self.active.push(t);
        } else {
            if self.pending.len() >= MAX_PENDING {
                self.pending.pop_front();
            }
            self.pending.push_back(t);
        }
    }

    /// Drop expired non-fatal actives and promote pending ones into
    /// the freed slots. The main loop calls this once per iteration
    /// before draw + `remaining`, so the renderer and timeout logic
    /// both see a fresh view.
    pub fn tick(&mut self) {
        self.active
            .retain(|t| t.level() == Level::Fatal || t.shown_at().elapsed() < TTL);
        while self.active.len() < MAX_ACTIVE {
            match self.pending.pop_front() {
                Some(mut next) => {
                    next.reset_shown_at();
                    self.active.push(next);
                }
                None => break,
            }
        }
    }

    /// Active toasts. The renderer iterates this; oldest first so the
    /// stack visually grows upward from the bottom-right corner. Call
    /// [`tick`](Self::tick) first if a stale view would matter.
    pub fn active(&self) -> &[Toast] {
        &self.active
    }

    /// Time until the next state change — either the soonest non-fatal
    /// TTL expiry or `FATAL_REMAINING` if only fatal toasts are live.
    /// `None` means nothing is on screen and the main loop can block
    /// without a timeout.
    pub fn remaining(&self) -> Option<Duration> {
        if self.active.is_empty() {
            return None;
        }
        let mut min: Option<Duration> = None;
        let mut has_fatal = false;
        for t in &self.active {
            if t.level() == Level::Fatal {
                has_fatal = true;
                continue;
            }
            let elapsed = t.shown_at().elapsed();
            let rem = if elapsed >= TTL {
                Duration::ZERO
            } else {
                TTL - elapsed
            };
            min = Some(match min {
                Some(m) => m.min(rem),
                None => rem,
            });
        }
        match (min, has_fatal) {
            (Some(d), _) => Some(d),
            (None, true) => Some(FATAL_REMAINING),
            (None, false) => None,
        }
    }

    /// Drop the oldest active fatal toast. Bound to `Esc` in Normal
    /// mode. Non-fatal toasts are left alone — they expire on their
    /// own and dismissing them on Esc would surprise the user mid-edit.
    pub fn dismiss_fatal(&mut self) -> bool {
        if let Some(idx) = self.active.iter().position(|t| t.level() == Level::Fatal) {
            self.active.remove(idx);
            true
        } else {
            false
        }
    }

    /// True iff at least one active toast is fatal — Esc handler uses
    /// this to decide whether to consume the keystroke.
    pub fn has_fatal(&self) -> bool {
        self.active.iter().any(|t| t.level() == Level::Fatal)
    }

    /// Wipe everything — active and pending. Exposed for callers that
    /// want to take over the toast slot wholesale; not currently used
    /// in-tree.
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.active.clear();
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_fills_active_then_pending() {
        let mut q = ToastQueue::new();
        for i in 0..5 {
            q.push(Toast::info(format!("t{i}")));
        }
        assert_eq!(q.active.len(), 3);
        assert_eq!(q.pending.len(), 2);
        let texts: Vec<&str> = q.active.iter().map(|t| t.text()).collect();
        assert_eq!(texts, vec!["t0", "t1", "t2"]);
    }

    #[test]
    fn empty_text_is_dropped() {
        let mut q = ToastQueue::new();
        q.push(Toast::info(""));
        assert!(q.active.is_empty());
        assert!(q.pending.is_empty());
    }

    #[test]
    fn tick_promotes_pending_when_active_expires() {
        let mut q = ToastQueue::new();
        // Backdate the first two so they're already expired.
        let mut a = Toast::info("a");
        a.shown_at = Instant::now() - TTL - Duration::from_millis(10);
        let mut b = Toast::info("b");
        b.shown_at = Instant::now() - TTL - Duration::from_millis(10);
        q.active.push(a);
        q.active.push(b);
        q.push(Toast::info("c"));
        q.push(Toast::info("d"));
        q.push(Toast::info("e"));
        // c filled the third active slot; d, e are pending.
        assert_eq!(q.pending.len(), 2);
        q.tick();
        let texts: Vec<&str> = q.active.iter().map(|t| t.text()).collect();
        assert_eq!(texts, vec!["c", "d", "e"]);
        assert!(q.pending.is_empty());
    }

    #[test]
    fn fatal_stays_through_tick() {
        let mut q = ToastQueue::new();
        let mut f = Toast::fatal("stuck");
        f.shown_at = Instant::now() - TTL - Duration::from_secs(10);
        q.active.push(f);
        q.tick();
        assert_eq!(q.active.len(), 1);
        assert_eq!(q.active[0].text(), "stuck");
    }

    #[test]
    fn dismiss_fatal_drops_one_fatal_only() {
        let mut q = ToastQueue::new();
        q.push(Toast::info("a"));
        q.push(Toast::fatal("boom"));
        q.push(Toast::info("c"));
        assert!(q.dismiss_fatal());
        let texts: Vec<&str> = q.active.iter().map(|t| t.text()).collect();
        assert_eq!(texts, vec!["a", "c"]);
        assert!(!q.dismiss_fatal());
    }

    #[test]
    fn consecutive_duplicates_refresh_shown_at_instead_of_stacking() {
        let mut q = ToastQueue::new();
        q.push(Toast::error("nothing to repeat"));
        let first_at = q.active[0].shown_at;
        std::thread::sleep(Duration::from_millis(5));
        // 50 duplicate pushes should not grow the queue.
        for _ in 0..50 {
            q.push(Toast::error("nothing to repeat"));
        }
        assert_eq!(q.active.len(), 1);
        assert!(q.pending.is_empty());
        // The refreshed timestamp is later than the original.
        assert!(q.active[0].shown_at > first_at);
    }

    #[test]
    fn distinct_text_still_stacks() {
        let mut q = ToastQueue::new();
        q.push(Toast::error("a"));
        q.push(Toast::error("b"));
        q.push(Toast::error("a"));
        assert_eq!(q.active.len(), 3);
    }

    #[test]
    fn tick_removes_after_real_ttl_elapses() {
        // Wall-clock smoke test: push a real toast, wait past TTL,
        // and verify tick drops it. Catches anything that would
        // make the unit-test backdating trick give false positives.
        let mut q = ToastQueue::new();
        q.push(Toast::error("ephemeral"));
        assert_eq!(q.active.len(), 1);
        std::thread::sleep(TTL + Duration::from_millis(50));
        q.tick();
        assert!(q.active.is_empty(), "still active: {:?}", q.active.len());
    }

    #[test]
    fn remaining_picks_soonest_non_fatal() {
        let mut q = ToastQueue::new();
        let mut a = Toast::info("a");
        a.shown_at = Instant::now() - Duration::from_millis(2_500);
        let mut b = Toast::info("b");
        b.shown_at = Instant::now() - Duration::from_millis(500);
        q.active.push(a);
        q.active.push(b);
        let rem = q.remaining().unwrap();
        // a expires first (~0.5s left).
        assert!(rem < Duration::from_millis(700), "got {:?}", rem);
    }
}
