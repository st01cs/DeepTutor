//! System notifications, and the "come back to this session" hand-off.
//!
//! Tauri's desktop notification API posts and returns — there is no click
//! callback on macOS, and the notification is delivered by the OS even if the
//! app is hidden. So the shell keeps the *intent* (which session the user was
//! told about) and delivers it the moment the app is active again:
//!
//! * the window regains focus → the UI asks for it through
//!   `plugin:deeptutor|take_notification_target`;
//! * the app is activated with no window in front (`RunEvent::Reopen`) → the
//!   shell reveals the window and emits the same target as an event.
//!
//! "Take" is destructive on purpose: a target is delivered once, so a reload or
//! a focus change cannot yank the user into an old session twice.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use tauri_plugin_deeptutor::{NotificationRequest, NotificationTarget};

struct Pending {
    route: String,
    session_id: Option<String>,
    title: String,
    at: Instant,
}

/// Pending-target bookkeeping for system notifications.
///
/// Holds no `AppHandle`, so it is usable (and testable) from the supervisor
/// thread as well as from the plugin's command thread.
pub struct NotificationCenter {
    enabled: AtomicBool,
    posted: AtomicU64,
    pending: Mutex<Option<Pending>>,
}

impl NotificationCenter {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: AtomicBool::new(enabled),
            posted: AtomicU64::new(0),
            pending: Mutex::new(None),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }

    /// How many notifications actually went out this session.
    pub fn posted(&self) -> u64 {
        self.posted.load(Ordering::SeqCst)
    }

    /// Remember where a posted notification points.
    pub fn record(&self, request: &NotificationRequest) {
        self.posted.fetch_add(1, Ordering::SeqCst);
        let mut pending = self.pending.lock().expect("notification lock poisoned");
        *pending = Some(Pending {
            route: request.route.clone(),
            session_id: request.session_id.clone(),
            title: request.title.clone(),
            at: Instant::now(),
        });
    }

    /// Forget the pending target (used when the OS refused the notification).
    pub fn clear(&self) {
        let mut pending = self.pending.lock().expect("notification lock poisoned");
        *pending = None;
    }

    /// Claim the pending target, if any.
    pub fn take(&self) -> Option<NotificationTarget> {
        let mut pending = self.pending.lock().expect("notification lock poisoned");
        let taken = pending.take()?;
        Some(NotificationTarget {
            route: taken.route,
            session_id: taken.session_id,
            title: taken.title,
            age_ms: taken.at.elapsed().as_millis() as u64,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(route: &str) -> NotificationRequest {
        NotificationRequest {
            title: "回合完成".to_string(),
            body: "研究报告已生成".to_string(),
            route: route.to_string(),
            session_id: Some("s-1".to_string()),
            kind: Some("round_complete".to_string()),
        }
    }

    #[test]
    fn a_target_is_delivered_exactly_once() {
        let center = NotificationCenter::new(true);
        assert!(center.take().is_none());
        center.record(&request("/chat/s-1"));
        let target = center.take().expect("queued");
        assert_eq!(target.route, "/chat/s-1");
        assert_eq!(target.session_id.as_deref(), Some("s-1"));
        assert_eq!(target.title, "回合完成");
        // The second claim finds nothing: no yanking the user back twice.
        assert!(center.take().is_none());
        assert_eq!(center.posted(), 1);
    }

    #[test]
    fn the_latest_round_wins() {
        let center = NotificationCenter::new(true);
        center.record(&request("/chat/older"));
        center.record(&request("/chat/newer"));
        assert_eq!(
            center.take().map(|target| target.route),
            Some("/chat/newer".to_string())
        );
        assert_eq!(center.posted(), 2);
    }

    #[test]
    fn a_refused_notification_leaves_nothing_pending() {
        let center = NotificationCenter::new(true);
        center.record(&request("/chat/s-1"));
        center.clear();
        assert!(center.take().is_none());
    }

    #[test]
    fn the_preference_gate_defaults_on_and_can_be_flipped() {
        let center = NotificationCenter::new(true);
        assert!(center.is_enabled());
        center.set_enabled(false);
        assert!(!center.is_enabled());
        // Turning the preference off does not resurrect an old target.
        assert!(center.take().is_none());
    }
}
