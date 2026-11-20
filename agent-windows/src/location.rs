//! Where the laptop is, from the Windows location service (Wi-Fi positioning on laptops
//! without GPS). Never delays a message by more than `WAIT`: if no fresh fix arrives in time,
//! the last known position is used and marked as such, while the lookup finishes in the
//! background for the next message.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Longest a message waits for a fresh position.
pub const WAIT: Duration = Duration::from_secs(3);
/// A fix younger than this counts as current.
const FRESH: Duration = Duration::from_secs(120);
/// Give the background lookup this long before giving up.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Fix {
    pub lat: f64,
    pub lon: f64,
    /// Metres.
    pub accuracy: f64,
    /// Unix seconds when measured.
    pub at: u64,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Remembers the last fix in memory and in one small file (overwritten, no history).
pub struct Locator {
    file: PathBuf,
    last: Mutex<Option<Fix>>,
}

impl Locator {
    pub fn new(home: &Path) -> Self {
        let file = home.join("last_location.json");
        let last = std::fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str(&t).ok());
        Self { file, last: Mutex::new(last) }
    }

    fn remember(&self, fix: Fix) {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Some(fix);
        if let Ok(json) = serde_json::to_string(&fix) {
            let _ = std::fs::write(&self.file, json);
        }
    }

    fn last(&self) -> Option<Fix> {
        *self.last.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A fresh position if one arrives within `WAIT`, else the last known one (if any).
    pub async fn current(self: &std::sync::Arc<Self>) -> Option<Fix> {
        let this = self.clone();
        let lookup = tokio::spawn(async move {
            let fix = measure_in_child().await;
            match fix {
                Some(f) => this.remember(f),
                None => tracing::info!("location: no fix"),
            }
            fix
        });
        match tokio::time::timeout(WAIT, lookup).await {
            Ok(Ok(Some(fix))) => Some(fix),
            // Too slow or failed: the lookup keeps running and updates the cache.
            _ => self.last(),
        }
    }
}

/// One lookup, for the `locate` command that `measure_in_child` runs.
pub fn measure_now() -> Option<Fix> {
    measure(LOOKUP_TIMEOUT)
}

/// Run the lookup in a separate short-lived process (this program's `locate` command). The
/// Windows location code once crashed the whole agent; in a child process a crash or hang
/// only loses the position, never a message.
async fn measure_in_child() -> Option<Fix> {
    let exe = std::env::current_exe().ok()?;
    let child = tokio::process::Command::new(exe)
        .arg("locate")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("location: lookup process not started: {e}");
            return None;
        }
    };
    match tokio::time::timeout(LOOKUP_TIMEOUT + Duration::from_secs(10), child.wait_with_output()).await {
        Ok(Ok(out)) if out.status.success() => serde_json::from_slice(&out.stdout).ok().flatten(),
        Ok(Ok(out)) => {
            tracing::warn!("location: lookup process failed (exit code {:#x})", out.status.code().unwrap_or(-1) as u32);
            None
        }
        Ok(Err(e)) => {
            tracing::warn!("location: lookup process: {e}");
            None
        }
        // Dropping the future kills the hung process.
        Err(_) => {
            tracing::warn!("location: lookup process hung and was stopped");
            None
        }
    }
}

/// Human-readable location for contacts: a map link with accuracy, marked when not fresh.
pub fn describe(fix: &Fix, sent_at: u64) -> String {
    let age = sent_at.saturating_sub(fix.at);
    let when = if age <= FRESH.as_secs() {
        String::new()
    } else if age < 3600 {
        format!(", last known {} min earlier", age / 60)
    } else if age < 48 * 3600 {
        format!(", last known {} h earlier", age / 3600)
    } else {
        format!(", last known {} days earlier", age / 86400)
    };
    format!(
        "https://www.openstreetmap.org/?mlat={lat:.5}&mlon={lon:.5}#map=17/{lat:.5}/{lon:.5} (±{acc:.0} m{when})",
        lat = fix.lat,
        lon = fix.lon,
        acc = fix.accuracy,
    )
}

/// Combine the configured place (e.g. a home address) with a measured fix.
pub fn combine(configured: Option<&str>, fix: Option<&Fix>) -> Option<String> {
    let measured = fix.map(|f| describe(f, now()));
    match (configured, measured) {
        (Some(c), Some(m)) => Some(format!("{c}\n{m}")),
        (Some(c), None) => Some(c.to_string()),
        (None, m) => m,
    }
}

#[cfg(windows)]
type Request = (Duration, std::sync::mpsc::Sender<Option<Fix>>);

/// Measure on the one location thread and wait for its answer.
#[cfg(windows)]
fn measure(timeout: Duration) -> Option<Fix> {
    use std::sync::mpsc;
    static WORKER: std::sync::OnceLock<Option<mpsc::Sender<Request>>> = std::sync::OnceLock::new();
    let worker = WORKER.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Request>();
        match std::thread::Builder::new().name("location".into()).spawn(move || location_thread(rx)) {
            Ok(_) => Some(tx),
            Err(e) => {
                tracing::error!("location thread not started: {e}");
                None
            }
        }
    });
    let (reply_tx, reply_rx) = mpsc::channel();
    worker.as_ref()?.send((timeout, reply_tx)).ok()?;
    reply_rx.recv().ok().flatten()
}

/// All location work runs on this thread, which lives as long as the agent and keeps one
/// Geolocator. With a short-lived thread per lookup, Windows unloaded Geolocation.dll once
/// the thread was gone while the location service still called into it, and the agent
/// crashed at a later unlock.
#[cfg(windows)]
fn location_thread(rx: std::sync::mpsc::Receiver<Request>) {
    use windows::Devices::Geolocation::{Geolocator, PositionAccuracy};
    use windows::Foundation::TimeSpan;
    use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

    if let Err(e) = unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
        tracing::error!("location: RoInitialize failed: {}", e.message());
    }
    let mut geo: Option<Geolocator> = None;
    for (timeout, reply) in rx {
        let mut run = || -> windows::core::Result<Fix> {
            if geo.is_none() {
                let g = Geolocator::new()?;
                g.SetDesiredAccuracy(PositionAccuracy::High)?;
                geo = Some(g);
            }
            let g = geo.as_ref().expect("just set");
            // TimeSpan is in 100 ns units. Accept a fix Windows already has if it is recent.
            let ticks = |d: Duration| TimeSpan { Duration: (d.as_nanos() / 100) as i64 };
            let pos = g.GetGeopositionAsyncWithAgeAndTimeout(ticks(FRESH), ticks(timeout))?.join()?;
            let c = pos.Coordinate()?;
            let p = c.Point()?.Position()?;
            Ok(Fix { lat: p.Latitude, lon: p.Longitude, accuracy: c.Accuracy()?, at: now() })
        };
        let fix = match run() {
            Ok(f) => Some(f),
            Err(e) => {
                tracing::info!("location unavailable: {}", e.message());
                None
            }
        };
        let _ = reply.send(fix);
    }
}

#[cfg(not(windows))]
fn measure(_timeout: Duration) -> Option<Fix> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIX: Fix = Fix { lat: 50.123456, lon: 4.654321, accuracy: 117.4, at: 1_800_000_000 };

    #[test]
    fn fresh_fix_has_no_age_note() {
        let s = describe(&FIX, FIX.at + 30);
        assert!(s.contains("mlat=50.12346&mlon=4.65432"), "{s}");
        assert!(s.ends_with("(±117 m)"), "{s}");
    }

    #[test]
    fn old_fix_is_marked_last_known() {
        assert!(describe(&FIX, FIX.at + 14 * 60).ends_with("(±117 m, last known 14 min earlier)"));
        assert!(describe(&FIX, FIX.at + 5 * 3600).contains("last known 5 h earlier"));
    }

    #[test]
    fn combines_configured_place_and_fix() {
        assert_eq!(combine(Some("Home"), None).as_deref(), Some("Home"));
        assert_eq!(combine(None, None), None);
        let both = combine(Some("Home"), Some(&Fix { at: now(), ..FIX })).unwrap();
        assert!(both.starts_with("Home\nhttps://www.openstreetmap.org/"), "{both}");
    }
}
