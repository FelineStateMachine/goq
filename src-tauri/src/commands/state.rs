use serde::Serialize;
use sigil_protocol::{KeyframeRequestReasonV3, MediaFeedbackReportV1};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex as TokioMutex;

#[cfg(target_os = "macos")]
static MACOS_CURSOR_HIDE_DEPTH: StdMutex<usize> = StdMutex::new(0);
#[cfg(target_os = "macos")]
static MACOS_CURSOR_GRABBED: AtomicBool = AtomicBool::new(false);

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGAssociateMouseAndMouseCursorPosition(connected: u32) -> i32;
    fn CGDisplayHideCursor(display: u32) -> i32;
    fn CGDisplayShowCursor(display: u32) -> i32;
}

#[cfg(target_os = "macos")]
fn set_native_cursor_grab(_window: &tauri::WebviewWindow, grab: bool) -> Result<(), String> {
    if !should_apply_native_cursor_association(MACOS_CURSOR_GRABBED.load(Ordering::Acquire), grab) {
        return Ok(());
    }
    let status = unsafe { CGAssociateMouseAndMouseCursorPosition(u32::from(!grab)) };
    if status != 0 {
        return Err(format!(
            "Could not {} native cursor grab: CGError {status}",
            if grab { "enable" } else { "release" }
        ));
    }
    MACOS_CURSOR_GRABBED.store(grab, Ordering::Release);
    Ok(())
}

/// CoreGraphics cursor disassociation is scoped to the foreground application.
/// Re-applying an active grab is therefore required after the window regains
/// focus even though Sigil's desired grab state has not changed.
#[cfg(any(target_os = "macos", test))]
const fn should_apply_native_cursor_association(current: bool, requested: bool) -> bool {
    requested || current != requested
}

#[cfg(all(
    not(target_os = "macos"),
    feature = "experimental-non-macos-pointer-capture"
))]
fn set_native_cursor_grab(window: &tauri::WebviewWindow, grab: bool) -> Result<(), String> {
    window
        .set_cursor_grab(grab)
        .map_err(|error| format!("Could not set native cursor grab to {grab}: {error}"))
}

#[cfg(all(
    not(target_os = "macos"),
    not(feature = "experimental-non-macos-pointer-capture")
))]
fn set_native_cursor_grab(_window: &tauri::WebviewWindow, grab: bool) -> Result<(), String> {
    if grab {
        return Err(
            "Relative pointer capture is unavailable on this Portal build; rebuild with the \
             experimental-non-macos-pointer-capture feature for platform UAT"
                .to_string(),
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn set_macos_cursor_hidden(hidden: bool) -> Result<(), String> {
    let mut depth = MACOS_CURSOR_HIDE_DEPTH
        .lock()
        .map_err(|_| "macOS cursor hide state is poisoned".to_string())?;
    update_macos_cursor_hide_depth(
        &mut depth,
        hidden,
        false,
        hide_macos_cursor,
        show_macos_cursor,
    )
}

#[cfg(target_os = "macos")]
fn reassert_macos_cursor_hidden() -> Result<(), String> {
    let mut depth = MACOS_CURSOR_HIDE_DEPTH
        .lock()
        .map_err(|_| "macOS cursor hide state is poisoned".to_string())?;
    update_macos_cursor_hide_depth(&mut depth, true, true, hide_macos_cursor, show_macos_cursor)
}

#[cfg(target_os = "macos")]
fn hide_macos_cursor() -> Result<(), String> {
    // kCGDirectMainDisplay is a macro that calls CGMainDisplayID(), not a
    // zero-valued sentinel. Passing zero can report success without hiding the
    // cursor on the active display.
    let status = unsafe { CGDisplayHideCursor(CGMainDisplayID()) };
    (status == 0)
        .then_some(())
        .ok_or_else(|| format!("Could not globally hide the macOS cursor: CGError {status}"))
}

#[cfg(target_os = "macos")]
fn show_macos_cursor() -> Result<(), String> {
    let status = unsafe { CGDisplayShowCursor(CGMainDisplayID()) };
    (status == 0)
        .then_some(())
        .ok_or_else(|| format!("Could not restore the macOS cursor: CGError {status}"))
}

#[cfg(any(target_os = "macos", test))]
fn update_macos_cursor_hide_depth(
    depth: &mut usize,
    hidden: bool,
    force: bool,
    mut hide: impl FnMut() -> Result<(), String>,
    mut show: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    if hidden {
        if *depth > 0 && !force {
            return Ok(());
        }
        hide()?;
        *depth = depth
            .checked_add(1)
            .ok_or_else(|| "macOS cursor hide depth overflowed".to_string())?;
        return Ok(());
    }

    while *depth > 0 {
        show()?;
        *depth -= 1;
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn set_macos_cursor_hidden(_hidden: bool) -> Result<(), String> {
    Ok(())
}

pub(crate) fn restore_client_cursor() {
    let _ = set_macos_cursor_hidden(false);
    #[cfg(target_os = "macos")]
    {
        let status = unsafe { CGAssociateMouseAndMouseCursorPosition(1) };
        if status == 0 {
            MACOS_CURSOR_GRABBED.store(false, Ordering::Release);
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn reassert_client_cursor_grab(window: &tauri::WebviewWindow) -> Result<(), String> {
    if !MACOS_CURSOR_GRABBED.load(Ordering::Acquire) {
        return Ok(());
    }
    set_native_cursor_grab(window, true)?;
    reassert_macos_cursor_hidden()?;
    let cursor_rect_result = reapply_hidden_cursor_rect(|visible| {
        window.set_cursor_visible(visible).map_err(|error| {
            format!(
                "Could not {} after focus changed: {error}",
                if visible {
                    "reset native cursor visibility state"
                } else {
                    "re-hide native cursor"
                }
            )
        })
    });
    // WebKit also rebuilds cursor rectangles after focus. Refresh both native
    // layers: the CoreGraphics hide is acquired before Tao briefly resets its
    // cursor state, so the local pointer cannot flash at its frozen position.
    cursor_rect_result
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn reassert_client_cursor_grab(_window: &tauri::WebviewWindow) -> Result<(), String> {
    Ok(())
}

fn reapply_hidden_cursor_rect(
    mut set_visible: impl FnMut(bool) -> Result<(), String>,
) -> Result<(), String> {
    // Tao only invalidates the platform cursor state when this boolean
    // changes. Force that transition while the native grab is active; on
    // macOS the balanced CoreGraphics hide prevents the intermediate state
    // from flashing at the frozen cursor position.
    set_visible(true)?;
    set_visible(false)
}

pub const AUDIO_DELIVERY_CAPACITY: usize = 3;

#[derive(Debug, Default)]
pub struct AudioDeliveryState {
    generation: Option<u64>,
    next_delivery_id: u64,
    outstanding: BTreeSet<u64>,
}

impl AudioDeliveryState {
    pub fn begin_generation(&mut self, generation: u64) -> Result<(), String> {
        if generation == 0 {
            return Err("Audio generation must be non-zero".to_string());
        }
        self.generation = Some(generation);
        self.next_delivery_id = 1;
        self.outstanding.clear();
        Ok(())
    }

    pub fn reserve(&mut self, generation: u64) -> Result<Option<u64>, String> {
        if self.generation != Some(generation) {
            return Err(format!(
                "Stale audio generation {generation}; current generation is {:?}",
                self.generation
            ));
        }
        if self.outstanding.len() >= AUDIO_DELIVERY_CAPACITY {
            return Ok(None);
        }
        let delivery_id = self.next_delivery_id;
        self.next_delivery_id = delivery_id
            .checked_add(1)
            .ok_or_else(|| "Audio delivery ID overflowed".to_string())?;
        if !self.outstanding.insert(delivery_id) {
            return Err(format!("Audio delivery ID {delivery_id} was reused"));
        }
        Ok(Some(delivery_id))
    }

    pub fn acknowledge(&mut self, generation: u64, delivery_id: u64) -> Result<(), String> {
        if self.generation != Some(generation) {
            return Err(format!(
                "Stale audio generation {generation}; current generation is {:?}",
                self.generation
            ));
        }
        if !self.outstanding.remove(&delivery_id) {
            return Err(format!(
                "Unknown or already acknowledged audio delivery ID {delivery_id}"
            ));
        }
        Ok(())
    }

    pub fn release_failed_delivery(&mut self, generation: u64, delivery_id: u64) -> bool {
        self.generation == Some(generation) && self.outstanding.remove(&delivery_id)
    }

    pub fn cancel_generation(&mut self, expected_generation: u64) -> bool {
        if self.generation != Some(expected_generation) {
            return false;
        }
        self.generation = None;
        self.outstanding.clear();
        true
    }

    pub fn depth(&self, generation: u64) -> Option<usize> {
        (self.generation == Some(generation)).then_some(self.outstanding.len())
    }

    pub fn generation(&self) -> Option<u64> {
        self.generation
    }

    pub fn clear(&mut self) {
        self.generation = None;
        self.outstanding.clear();
    }
}

pub const fn development_direct_node_available() -> bool {
    cfg!(any(debug_assertions, feature = "demo-direct-node"))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaunchOptions {
    pub dev_connect_node_id: Option<iroh::PublicKey>,
}

pub fn parse_launch_options<I, S>(args: I, allow_dev_connect: bool) -> Result<LaunchOptions, String>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut options = LaunchOptions::default();
    let mut args = args.into_iter().map(Into::into);

    // The first value is the executable path. Unknown values are left for the
    // platform/Tauri runtime, but Portal-owned flags are parsed strictly.
    let _ = args.next();
    while let Some(arg) = args.next() {
        if arg == "--daemon" {
            return Err(
                "--daemon is not supported by Portal; run the separate sigil executable"
                    .to_string(),
            );
        }

        let arg = arg
            .into_string()
            .map_err(|_| "Portal command-line arguments must be valid UTF-8".to_string())?;
        let node_id = if arg == "--dev-connect" {
            Some(
                args.next()
                    .ok_or_else(|| "--dev-connect requires an iroh node ID".to_string())?
                    .into_string()
                    .map_err(|_| "The --dev-connect node ID must be valid UTF-8".to_string())?,
            )
        } else {
            arg.strip_prefix("--dev-connect=").map(str::to_owned)
        };

        let Some(node_id) = node_id else {
            continue;
        };
        if !allow_dev_connect {
            return Err(
                "--dev-connect requires a debug build or the explicit demo-direct-node feature; ordinary release builds require the normal passkey flow"
                    .to_string(),
            );
        }
        if options.dev_connect_node_id.is_some() {
            return Err("--dev-connect may be specified only once".to_string());
        }
        if node_id.is_empty() {
            return Err("--dev-connect requires an iroh node ID".to_string());
        }
        options.dev_connect_node_id = Some(
            node_id
                .parse::<iroh::PublicKey>()
                .map_err(|e| format!("Invalid iroh node ID for --dev-connect: {e}"))?,
        );
    }

    Ok(options)
}

pub struct AppState {
    pub enrollment: super::enrollment::EnrollmentState,
    pub input_send: TokioMutex<Option<tokio::sync::mpsc::Sender<sigil_protocol::InputEvent>>>,
    pub client_endpoint: TokioMutex<Option<iroh::Endpoint>>,
    pub media_connection: TokioMutex<Option<(u64, iroh::endpoint::Connection)>>,
    pub media_control: TokioMutex<Option<(u64, MediaControlRequestSender)>>,
    pub media_feedback: TokioMutex<Option<(u64, iroh::endpoint::Connection, MediaFeedbackSender)>>,
    pub media_feedback_report_id: Arc<AtomicU64>,
    pub frame_delivery: TokioMutex<Option<(u64, Arc<AtomicUsize>)>>,
    pub client_media_generation: Arc<AtomicU64>,
    pub audio_deliveries: Arc<StdMutex<AudioDeliveryState>>,
    pub audio_connection: TokioMutex<Option<(u64, iroh::endpoint::Connection)>>,
    pub audio_connection_generation: Arc<AtomicU64>,
    pub client_connection_serial: TokioMutex<()>,
    pub client_connection_active: Arc<AtomicBool>,
    pub webcodecs: AtomicBool,
    pub dev_connect_node_id: Option<iroh::PublicKey>,
}

pub type MediaControlRequestSender =
    tokio::sync::mpsc::Sender<(KeyframeRequestReasonV3, Option<u64>)>;
pub type MediaFeedbackSender = tokio::sync::watch::Sender<Option<AccumulatedMediaFeedback>>;

const MAX_MEDIA_FEEDBACK_INTERVAL_MS: u64 = 5_000;

/// Constant-size latest receiver state plus cumulative interval pressure.
///
/// A watch channel deliberately coalesces updates while its consumer is
/// blocked. Keeping cumulative counters here lets the writer recover the full
/// delta since its last successful write instead of losing the reports that
/// were replaced in the meantime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccumulatedMediaFeedback {
    latest: MediaFeedbackReportV1,
    interval_ms_total: u64,
    transport_dropped_total: u64,
    frontend_dropped_total: u64,
    decoder_dropped_total: u64,
    presenter_dropped_total: u64,
}

impl AccumulatedMediaFeedback {
    pub fn new(report: MediaFeedbackReportV1) -> Self {
        Self {
            latest: report,
            interval_ms_total: u64::from(report.interval_ms),
            transport_dropped_total: u64::from(report.transport_dropped_delta),
            frontend_dropped_total: u64::from(report.frontend_dropped_delta),
            decoder_dropped_total: u64::from(report.decoder_dropped_delta),
            presenter_dropped_total: u64::from(report.presenter_dropped_delta),
        }
    }

    pub fn merge(&mut self, report: MediaFeedbackReportV1) {
        self.latest = report;
        self.interval_ms_total = self
            .interval_ms_total
            .saturating_add(u64::from(report.interval_ms));
        self.transport_dropped_total = self
            .transport_dropped_total
            .saturating_add(u64::from(report.transport_dropped_delta));
        self.frontend_dropped_total = self
            .frontend_dropped_total
            .saturating_add(u64::from(report.frontend_dropped_delta));
        self.decoder_dropped_total = self
            .decoder_dropped_total
            .saturating_add(u64::from(report.decoder_dropped_delta));
        self.presenter_dropped_total = self
            .presenter_dropped_total
            .saturating_add(u64::from(report.presenter_dropped_delta));
    }

    pub fn report_since(self, previous: Option<Self>) -> MediaFeedbackReportV1 {
        let previous = previous.unwrap_or(Self {
            latest: self.latest,
            interval_ms_total: 0,
            transport_dropped_total: 0,
            frontend_dropped_total: 0,
            decoder_dropped_total: 0,
            presenter_dropped_total: 0,
        });
        let mut report = self.latest;
        report.interval_ms = self
            .interval_ms_total
            .saturating_sub(previous.interval_ms_total)
            .min(MAX_MEDIA_FEEDBACK_INTERVAL_MS) as u16;
        report.transport_dropped_delta = cumulative_delta(
            self.transport_dropped_total,
            previous.transport_dropped_total,
        );
        report.frontend_dropped_delta =
            cumulative_delta(self.frontend_dropped_total, previous.frontend_dropped_total);
        report.decoder_dropped_delta =
            cumulative_delta(self.decoder_dropped_total, previous.decoder_dropped_total);
        report.presenter_dropped_delta = cumulative_delta(
            self.presenter_dropped_total,
            previous.presenter_dropped_total,
        );
        report
    }
}

fn cumulative_delta(current: u64, previous: u64) -> u32 {
    current.saturating_sub(previous).min(u64::from(u32::MAX)) as u32
}

impl Default for AppState {
    fn default() -> Self {
        Self::new(LaunchOptions::default())
    }
}

impl AppState {
    pub fn from_args<I, S>(args: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        parse_launch_options(args, development_direct_node_available()).map(Self::new)
    }

    fn new(options: LaunchOptions) -> Self {
        Self {
            enrollment: super::enrollment::EnrollmentState::default(),
            input_send: TokioMutex::new(None),
            client_endpoint: TokioMutex::new(None),
            media_connection: TokioMutex::new(None),
            media_control: TokioMutex::new(None),
            media_feedback: TokioMutex::new(None),
            media_feedback_report_id: Arc::new(AtomicU64::new(0)),
            frame_delivery: TokioMutex::new(None),
            client_media_generation: Arc::new(AtomicU64::new(0)),
            audio_deliveries: Arc::new(StdMutex::new(AudioDeliveryState::default())),
            audio_connection: TokioMutex::new(None),
            audio_connection_generation: Arc::new(AtomicU64::new(0)),
            client_connection_serial: TokioMutex::new(()),
            client_connection_active: Arc::new(AtomicBool::new(false)),
            webcodecs: AtomicBool::new(false),
            dev_connect_node_id: options.dev_connect_node_id,
        }
    }
}

// ─── Simple state Tauri commands ─────────────────────────────────────────────

#[derive(Clone, Debug, Serialize)]
pub struct DevelopmentConnectionMode {
    pub enabled: bool,
    pub host_node_id: Option<String>,
    pub warning: &'static str,
}

#[tauri::command]
pub fn development_connection_mode(state: tauri::State<'_, AppState>) -> DevelopmentConnectionMode {
    DevelopmentConnectionMode {
        enabled: state.dev_connect_node_id.is_some(),
        host_node_id: state.dev_connect_node_id.map(|node_id| node_id.to_string()),
        warning: "Development direct-node routing skips passkey identity lookup; it is not client authorization.",
    }
}

#[tauri::command]
pub fn set_client_cursor_grab(window: tauri::WebviewWindow, grab: bool) -> Result<bool, String> {
    if grab {
        set_native_cursor_grab(&window, true)?;
        if let Err(error) = reapply_hidden_cursor_rect(|visible| {
            window
                .set_cursor_visible(visible)
                .map_err(|error| format!("Could not set native cursor visibility: {error}"))
        }) {
            let _ = set_native_cursor_grab(&window, false);
            return Err(error);
        }
        if let Err(error) = set_macos_cursor_hidden(true) {
            let _ = window.set_cursor_visible(true);
            let _ = set_native_cursor_grab(&window, false);
            return Err(error);
        }
        // CoreGraphics disassociation already supplies relative deltas on
        // macOS. WebKit Pointer Lock would be a second asynchronous cursor
        // owner and can reveal the native pointer after this command returns.
        return Ok(!cfg!(target_os = "macos"));
    }

    let global_visibility_result = set_macos_cursor_hidden(false);
    let visibility_result = window
        .set_cursor_visible(true)
        .map_err(|error| format!("Could not restore native cursor visibility: {error}"));
    let release_result = set_native_cursor_grab(&window, false);
    global_visibility_result
        .and(release_result)
        .and(visibility_result)
        .map(|()| !cfg!(target_os = "macos"))
}

#[tauri::command]
pub fn set_webcodecs_available(state: tauri::State<'_, AppState>, available: bool) -> bool {
    state.webcodecs.store(available, Ordering::SeqCst);
    available
}

#[tauri::command]
pub fn is_webcodecs_available(state: tauri::State<'_, AppState>) -> bool {
    state.webcodecs.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_node_id() -> String {
        iroh::SecretKey::generate().public().to_string()
    }

    #[test]
    fn active_native_cursor_grab_is_reapplied_after_focus_changes() {
        assert!(should_apply_native_cursor_association(true, true));
        assert!(should_apply_native_cursor_association(false, true));
        assert!(should_apply_native_cursor_association(true, false));
        assert!(!should_apply_native_cursor_association(false, false));
    }

    #[test]
    fn cursor_focus_reassertion_forces_tao_visibility_transition() {
        let mut visibility = Vec::new();

        reapply_hidden_cursor_rect(|visible| {
            visibility.push(visible);
            Ok(())
        })
        .unwrap();

        assert_eq!(visibility, [true, false]);
    }

    #[test]
    fn cursor_focus_reassertion_owns_each_core_graphics_hide() {
        use std::cell::Cell;

        let mut depth = 0;
        let hides = Cell::new(0);
        let shows = Cell::new(0);
        for force in [false, false, true] {
            update_macos_cursor_hide_depth(
                &mut depth,
                true,
                force,
                || {
                    hides.set(hides.get() + 1);
                    Ok(())
                },
                || {
                    shows.set(shows.get() + 1);
                    Ok(())
                },
            )
            .unwrap();
        }
        assert_eq!((depth, hides.get(), shows.get()), (2, 2, 0));

        update_macos_cursor_hide_depth(
            &mut depth,
            false,
            false,
            || {
                hides.set(hides.get() + 1);
                Ok(())
            },
            || {
                shows.set(shows.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!((depth, hides.get(), shows.get()), (0, 2, 2));
    }

    #[test]
    fn cursor_hide_depth_keeps_failed_operations_recoverable() {
        let mut depth = 0;
        let error = update_macos_cursor_hide_depth(
            &mut depth,
            true,
            false,
            || Err("hide failed".to_string()),
            || Ok(()),
        )
        .unwrap_err();
        assert_eq!(error, "hide failed");
        assert_eq!(depth, 0);

        depth = 2;
        let error = update_macos_cursor_hide_depth(
            &mut depth,
            false,
            false,
            || Ok(()),
            || Err("show failed".to_string()),
        )
        .unwrap_err();
        assert_eq!(error, "show failed");
        assert_eq!(depth, 2);
    }

    #[test]
    fn parses_debug_direct_node_flag() {
        let node_id = test_node_id();
        let options = parse_launch_options(["portal", "--dev-connect", &node_id], true).unwrap();

        assert_eq!(options.dev_connect_node_id.unwrap().to_string(), node_id);
    }

    #[test]
    fn rejects_legacy_daemon_mode() {
        let error = parse_launch_options(["portal", "--daemon"], true).unwrap_err();
        assert!(error.contains("separate sigil executable"));
    }

    #[test]
    fn parses_equals_form() {
        let node_id = test_node_id();
        let flag = format!("--dev-connect={node_id}");
        let options = parse_launch_options(["portal", &flag], true).unwrap();

        assert_eq!(options.dev_connect_node_id.unwrap().to_string(), node_id);
    }

    #[test]
    fn rejects_direct_node_when_debug_mode_is_disabled() {
        let node_id = test_node_id();
        let error = parse_launch_options(["portal", "--dev-connect", &node_id], false).unwrap_err();

        assert!(error.contains("debug build or the explicit demo-direct-node feature"));
    }

    #[test]
    fn ordinary_release_excludes_direct_node_bypass() {
        if cfg!(not(debug_assertions)) && cfg!(not(feature = "demo-direct-node")) {
            assert!(!development_direct_node_available());
        }
    }

    #[test]
    fn app_state_accepts_direct_node_only_in_debug_builds() {
        let node_id = test_node_id();
        let result = AppState::from_args(["portal", "--dev-connect", &node_id]);

        assert_eq!(result.is_ok(), development_direct_node_available());
    }

    #[test]
    fn app_state_starts_without_owned_transport_connections() {
        let state = AppState::default();

        assert!(state.client_endpoint.try_lock().unwrap().is_none());
        assert!(state.media_connection.try_lock().unwrap().is_none());
        assert!(state.media_control.try_lock().unwrap().is_none());
        assert!(state.media_feedback.try_lock().unwrap().is_none());
        assert!(state.audio_connection.try_lock().unwrap().is_none());
    }

    #[test]
    fn rejects_missing_invalid_and_duplicate_node_ids() {
        assert!(
            parse_launch_options(["portal", "--dev-connect"], true)
                .unwrap_err()
                .contains("requires an iroh node ID")
        );
        assert!(
            parse_launch_options(["portal", "--dev-connect", "not-a-node-id"], true)
                .unwrap_err()
                .contains("Invalid iroh node ID")
        );

        let node_id = test_node_id();
        assert!(
            parse_launch_options(
                [
                    "portal",
                    "--dev-connect",
                    &node_id,
                    "--dev-connect",
                    &node_id,
                ],
                true,
            )
            .unwrap_err()
            .contains("only once")
        );
    }
}
