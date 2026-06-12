//! bui-update — GitHub-release auto-updater for the copper binary.
//!
//! Flow: a background thread polls `/releases/latest`, compares the tag
//! against the running version, downloads + SHA256-verifies the matching
//! tarball, and stages the new binary next to the installed one. The UI
//! reads [`UpdateStatus`] to paint a status-line pill; on confirmation
//! [`apply_and_relaunch`] swaps the binary and restarts.
//!
//! Environment knobs (testing/opt-out):
//! - `COPPER_NO_UPDATE=1`  — never check.
//! - `COPPER_UPDATE_API`   — override the releases-API URL (fixtures).
//! - `COPPER_UPDATE_FORCE=1` — skip the version comparison.

mod github;
mod install;
mod json;
mod version;

pub use github::{DEFAULT_API_URL, ReleaseInfo, TARGET};
pub use install::{StagedUpdate, apply_and_relaunch, cleanup_stale};
pub use version::Version;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bui_net::Client;

#[derive(Debug, Clone)]
pub enum UpdateError {
    Net(String),
    Api { status: u16 },
    /// 404 from the API — repo has no (stable) releases yet.
    NoRelease,
    Parse(String),
    AssetMissing(String),
    ChecksumMismatch,
    /// Install directory isn't writable; user must update manually.
    NotWritable,
    Io(String),
    Tar(String),
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateError::Net(e) => write!(f, "network: {e}"),
            UpdateError::Api { status } => write!(f, "api status {status}"),
            UpdateError::NoRelease => write!(f, "no release found"),
            UpdateError::Parse(e) => write!(f, "parse: {e}"),
            UpdateError::AssetMissing(name) => write!(f, "release has no asset {name}"),
            UpdateError::ChecksumMismatch => write!(f, "checksum mismatch"),
            UpdateError::NotWritable => write!(f, "install dir not writable"),
            UpdateError::Io(e) => write!(f, "{e}"),
            UpdateError::Tar(e) => write!(f, "extract: {e}"),
        }
    }
}

/// How long transient states (UpToDate / Failed) stay visible.
pub const TRANSIENT_SECS: u64 = 4;

#[derive(Debug, Clone)]
pub enum UpdateStatus {
    Idle,
    Checking,
    UpToDate { at: Instant },
    Downloading { version: String },
    /// New binary verified and staged — one ⌘U away from running it.
    Ready(StagedUpdate),
    /// Update exists but we can't write the install dir; `html_url`
    /// is the release page to open instead.
    AvailableNotWritable { version: String, html_url: String },
    Failed { msg: String, at: Instant },
}

impl UpdateStatus {
    /// Transient states render for a few seconds, then disappear.
    pub fn is_expired(&self) -> bool {
        match self {
            UpdateStatus::UpToDate { at } | UpdateStatus::Failed { at, .. } => {
                at.elapsed() >= Duration::from_secs(TRANSIENT_SECS)
            }
            _ => false,
        }
    }
}

pub type SharedUpdateStatus = Arc<Mutex<UpdateStatus>>;

pub fn new_shared_status() -> SharedUpdateStatus {
    Arc::new(Mutex::new(UpdateStatus::Idle))
}

fn set_status(status: &SharedUpdateStatus, value: UpdateStatus, notify: &impl Fn()) {
    if let Ok(mut st) = status.lock() {
        *st = value;
    }
    notify();
}

/// Kick off one check→download→stage pass on a background thread.
///
/// No-op when the platform has no release asset, when updates are
/// disabled, when a pass is already in flight, or when an update is
/// already staged. `notify` is called after every visible state change
/// (wire it to `Redrawer::request_redraw`).
pub fn spawn_check(
    status: SharedUpdateStatus,
    current: Version,
    notify: impl Fn() + Send + 'static,
    delay: Duration,
) {
    static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

    if TARGET.is_empty() {
        return;
    }
    if std::env::var("COPPER_NO_UPDATE").is_ok_and(|v| v == "1") {
        return;
    }
    if let Ok(st) = status.lock() {
        if matches!(*st, UpdateStatus::Ready(_)) {
            return;
        }
    }
    if IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }

    std::thread::spawn(move || {
        std::thread::sleep(delay);
        let final_status = run_pass(&status, &current, &notify);
        let is_transient = final_status.is_some();
        if let Some(value) = final_status {
            set_status(&status, value, &notify);
        }
        IN_FLIGHT.store(false, Ordering::SeqCst);
        if is_transient {
            // One more wakeup so the event-driven repaint clears the
            // transient text instead of leaving it until the next input.
            std::thread::sleep(Duration::from_secs(TRANSIENT_SECS) + Duration::from_millis(200));
            notify();
        }
    });
}

/// One full pass. Returns `Some(transient final state)` for states that
/// should auto-clear, `None` when the state was already set to a sticky
/// value (Ready / AvailableNotWritable).
fn run_pass(
    status: &SharedUpdateStatus,
    current: &Version,
    notify: &(impl Fn() + Send),
) -> Option<UpdateStatus> {
    let fail = |e: UpdateError| {
        Some(UpdateStatus::Failed {
            msg: e.to_string(),
            at: Instant::now(),
        })
    };

    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return fail(UpdateError::Io("tokio runtime".into()));
    };

    set_status(status, UpdateStatus::Checking, notify);

    let api_url =
        std::env::var("COPPER_UPDATE_API").unwrap_or_else(|_| DEFAULT_API_URL.to_string());
    // Own client: the shared one lives on the UI thread's runtime, and a
    // multi-MB download must not serialize against page loads.
    let client = Client::new();

    let doc = match rt.block_on(github::fetch_latest(&client, &api_url)) {
        Ok(doc) => doc,
        Err(UpdateError::NoRelease) => return Some(UpdateStatus::UpToDate { at: Instant::now() }),
        Err(e) => return fail(e),
    };
    let info = match github::parse_release(&doc, TARGET) {
        Ok(info) => info,
        Err(e) => return fail(e),
    };

    let force = std::env::var("COPPER_UPDATE_FORCE").is_ok_and(|v| v == "1");
    if !force && !info.version.is_newer_than(current) {
        return Some(UpdateStatus::UpToDate { at: Instant::now() });
    }

    set_status(
        status,
        UpdateStatus::Downloading {
            version: info.version.to_string(),
        },
        notify,
    );

    match rt.block_on(install::download_and_stage(&client, &info)) {
        Ok(staged) => {
            set_status(status, UpdateStatus::Ready(staged), notify);
            None
        }
        Err(UpdateError::NotWritable) => {
            set_status(
                status,
                UpdateStatus::AvailableNotWritable {
                    version: info.version.to_string(),
                    html_url: info.html_url,
                },
                notify,
            );
            None
        }
        Err(e) => fail(e),
    }
}
