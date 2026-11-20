//! State shared between the window thread (hotkeys, settings window) and the publisher.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use crate::config::CheckIn;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// After a login or unlock without a check-in, the settings window stays closed this long:
/// someone forcing you to log in must not see it, nor be able to switch check-ins off.
const LOCKOUT_AFTER_UNCONFIRMED: Duration = Duration::from_secs(15 * 60);

static PAUSED: AtomicBool = AtomicBool::new(false);
/// Check-in time and reminders; the settings window can change them while the agent runs.
static CHECK_IN: Mutex<Option<CheckIn>> = Mutex::new(None);
/// Shows the on-screen part of a reminder (set by the window code).
static REMIND_ON_SCREEN: OnceLock<fn()> = OnceLock::new();
static UNCONFIRMED_SINCE: Mutex<Option<Instant>> = Mutex::new(None);

/// Check-ins paused: no `session_start` on logon/unlock. Duress keeps working. Not saved,
/// so a restart or new sign-in always starts enabled.
pub fn paused() -> bool {
    PAUSED.load(Ordering::SeqCst)
}

pub fn set_paused(p: bool) {
    PAUSED.store(p, Ordering::SeqCst);
}

pub fn set_check_in(c: CheckIn) {
    *CHECK_IN.lock().unwrap_or_else(|e| e.into_inner()) = Some(c);
}

pub fn check_in() -> CheckIn {
    CHECK_IN.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default()
}

pub fn timeout_secs() -> u64 {
    match check_in().timeout_seconds {
        0 => 60,
        t => t,
    }
}

pub fn set_remind_on_screen(f: fn()) {
    let _ = REMIND_ON_SCREEN.set(f);
}

pub fn remind_on_screen() {
    if let Some(f) = REMIND_ON_SCREEN.get() {
        f();
    }
}

/// Whether the newest session has waited about `after` without a check-in (a newer session
/// gets its own reminder).
pub fn still_waiting(after: Duration) -> bool {
    unconfirmed_for().is_some_and(|d| d + Duration::from_millis(500) >= after && d < after + Duration::from_secs(5))
}

/// A session started and waits for its check-in.
pub fn session_started() {
    *UNCONFIRMED_SINCE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
}

/// The check-in was given.
pub fn checked_in() {
    *UNCONFIRMED_SINCE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// How long the newest session has waited for its check-in (`None`: nothing waiting).
pub fn unconfirmed_for() -> Option<Duration> {
    UNCONFIRMED_SINCE.lock().unwrap_or_else(|e| e.into_inner()).map(|t| t.elapsed())
}

/// Text of the small confirmation shown after the check-in key: the time since the session
/// started, and whether it came too late to stop the alarm.
pub fn ok_feedback_text(waited: Option<Duration>, timeout_secs: u64) -> String {
    match waited {
        None => "\u{2713}".to_string(),
        Some(d) if d.as_secs() > timeout_secs => format!("\u{2713} {} s \u{b7} late, contacts alerted", d.as_secs()),
        Some(d) => format!("\u{2713} {} s", d.as_secs()),
    }
}

/// Whether the settings window may open now.
pub fn settings_allowed() -> bool {
    match *UNCONFIRMED_SINCE.lock().unwrap_or_else(|e| e.into_inner()) {
        Some(t) => t.elapsed() >= LOCKOUT_AFTER_UNCONFIRMED,
        None => true,
    }
}

static WATCHER_RELOAD: std::sync::LazyLock<tokio::sync::Notify> = std::sync::LazyLock::new(tokio::sync::Notify::new);

/// Ask the built-in watcher to restart with the config on disk (after a settings change).
pub fn reload_watcher() {
    WATCHER_RELOAD.notify_one();
}

pub async fn watcher_reload_requested() {
    WATCHER_RELOAD.notified().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_feedback_shows_time_and_lateness() {
        assert_eq!(ok_feedback_text(None, 60), "\u{2713}");
        assert_eq!(ok_feedback_text(Some(Duration::from_millis(14_700)), 60), "\u{2713} 14 s");
        assert_eq!(ok_feedback_text(Some(Duration::from_secs(60)), 60), "\u{2713} 60 s");
        assert_eq!(ok_feedback_text(Some(Duration::from_secs(75)), 60), "\u{2713} 75 s \u{b7} late, contacts alerted");
    }
}
