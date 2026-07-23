use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(any(target_os = "linux", test))]
use std::time::{Duration, Instant};

#[cfg(any(target_os = "linux", test))]
use anyhow::Context;
use anyhow::Result;
use sigil_protocol::{PointerPosition, PointerSurfaceDimensions};

#[cfg(target_os = "linux")]
const POINTER_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_micros(16_667);
#[cfg(any(target_os = "linux", test))]
const BOOTSTRAP_RECONNECT_INITIAL: Duration = Duration::from_millis(100);
#[cfg(any(target_os = "linux", test))]
const BOOTSTRAP_RECONNECT_MAXIMUM: Duration = Duration::from_secs(2);
#[cfg(any(target_os = "linux", test))]
const POINTER_ACTIVITY_VISIBLE_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(target_os = "linux")]
const FOCUS_DISPLAY_PROPERTY: &[u8] = b"GAMESCOPE_MOUSE_FOCUS_DISPLAY";
#[cfg(target_os = "linux")]
const CURSOR_VISIBLE_PROPERTY: &[u8] = b"GAMESCOPE_CURSOR_VISIBLE_FEEDBACK";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointerState {
    pub position: Option<PointerPosition>,
    pub visible: bool,
}

#[cfg(target_os = "linux")]
impl PointerState {
    const UNAVAILABLE: Self = Self {
        position: None,
        visible: false,
    };
}

#[cfg(any(target_os = "linux", test))]
fn available_pointer_state(position: PointerPosition, visible: Option<bool>) -> PointerState {
    PointerState {
        position: Some(position),
        // Unknown visibility fails closed without discarding the independently
        // valid compositor position.
        visible: visible.unwrap_or(false),
    }
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug)]
struct PointerActivityVisibility {
    last_position: Option<PointerPosition>,
    visible_until: Option<Instant>,
}

#[cfg(any(target_os = "linux", test))]
impl PointerActivityVisibility {
    fn new(initial_position: PointerPosition) -> Self {
        Self {
            last_position: Some(initial_position),
            visible_until: None,
        }
    }

    fn reset(&mut self) {
        self.last_position = None;
        self.visible_until = None;
    }

    fn resolve(
        &mut self,
        position: PointerPosition,
        compositor_visible: Option<bool>,
        sampled_at: Instant,
    ) -> bool {
        if self
            .last_position
            .is_some_and(|previous| previous != position)
        {
            self.visible_until = sampled_at.checked_add(POINTER_ACTIVITY_VISIBLE_TIMEOUT);
        }
        self.last_position = Some(position);

        compositor_visible.unwrap_or(false)
            || self
                .visible_until
                .is_some_and(|deadline| sampled_at < deadline)
    }
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug)]
struct ReconnectBackoff {
    delay: Duration,
    retry_not_before: Instant,
}

#[cfg(any(target_os = "linux", test))]
impl ReconnectBackoff {
    fn new(now: Instant) -> Self {
        Self {
            delay: BOOTSTRAP_RECONNECT_INITIAL,
            retry_not_before: now,
        }
    }

    fn ready(&self, now: Instant) -> bool {
        now >= self.retry_not_before
    }

    fn failed(&mut self, now: Instant) {
        self.retry_not_before = now.checked_add(self.delay).unwrap_or(now);
        self.delay = self
            .delay
            .checked_mul(2)
            .unwrap_or(BOOTSTRAP_RECONNECT_MAXIMUM)
            .min(BOOTSTRAP_RECONNECT_MAXIMUM);
    }

    fn recovered(&mut self, now: Instant) {
        self.delay = BOOTSTRAP_RECONNECT_INITIAL;
        self.retry_not_before = now;
    }
}

#[derive(Clone)]
pub struct PointerPositionTracker {
    inner: Arc<TrackerInner>,
}

impl std::fmt::Debug for PointerPositionTracker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PointerPositionTracker")
            .finish_non_exhaustive()
    }
}

struct TrackerInner {
    latest: tokio::sync::watch::Sender<PointerState>,
    stop: Arc<AtomicBool>,
}

impl Drop for TrackerInner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl PointerPositionTracker {
    /// Connect to the Gamescope Xwayland display inherited by the service.
    /// Failure is non-fatal: callers omit pointer-feedback capability rather
    /// than synthesizing a position that could diverge from the compositor.
    #[cfg(target_os = "linux")]
    pub fn try_initialize(
        configured_display: Option<&str>,
        pointer_surface_dimensions: PointerSurfaceDimensions,
    ) -> Result<Self> {
        let bootstrap_display = match configured_display {
            Some(display) => display.to_owned(),
            None => std::env::var("DISPLAY")
                .context("neither gamescope_pipewire.xwayland_display nor DISPLAY is configured")?,
        };
        validate_display_name(&bootstrap_display)?;
        let mut bootstrap = connect_bootstrap_display(&bootstrap_display)?;
        anyhow::ensure!(
            bootstrap.pointer_surface_dimensions == pointer_surface_dimensions,
            "bootstrap Gamescope pointer surface is {}x{}, but capture preflight resolved {}x{}",
            bootstrap.pointer_surface_dimensions.width,
            bootstrap.pointer_surface_dimensions.height,
            pointer_surface_dimensions.width,
            pointer_surface_dimensions.height
        );
        let focus_display = bootstrap
            .query_focus_display()
            .context("querying initial Gamescope mouse-focus display")?;
        let mut active = connect_active_display(&focus_display)
            .context("connecting to active Gamescope Xwayland")?;
        let initial_position = query_pointer_position(
            &active.connection,
            active.root,
            bootstrap.pointer_surface_dimensions,
        )
        .context("querying initial Gamescope Xwayland pointer position")?;
        let initial =
            available_pointer_state(initial_position, active.query_cursor_visible_bounded());
        let (latest, _) = tokio::sync::watch::channel(initial);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_latest = latest.clone();
        let thread_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("sigil-xwayland-pointer".into())
            .spawn(move || {
                let mut bootstrap = Some(bootstrap);
                let mut active_display = focus_display;
                let mut active_connection = Some(active);
                let mut activity_visibility = PointerActivityVisibility::new(initial_position);
                let mut reconnect_backoff = ReconnectBackoff::new(Instant::now());
                while !thread_stop.load(Ordering::Acquire) {
                    std::thread::sleep(POINTER_POLL_INTERVAL);
                    if bootstrap.is_none() {
                        let now = Instant::now();
                        if !reconnect_backoff.ready(now) {
                            continue;
                        }
                        match connect_bootstrap_display(&bootstrap_display) {
                            Ok(mut reconnected) => {
                                let focus_display = reconnected.query_focus_display();
                                let recovered = focus_display.and_then(|focus_display| {
                                    let mut active = connect_active_display(&focus_display)?;
                                    let position = query_pointer_position(
                                        &active.connection,
                                        active.root,
                                        reconnected.pointer_surface_dimensions,
                                    )?;
                                    let visible = active.query_cursor_visible_bounded();
                                    Ok((focus_display, active, position, visible))
                                });
                                match recovered {
                                    Ok((focus_display, active, position, visible)) => {
                                        let pointer_surface_dimensions =
                                            reconnected.pointer_surface_dimensions;
                                        bootstrap = Some(reconnected);
                                        active_display = focus_display;
                                        active_connection = Some(active);
                                        activity_visibility =
                                            PointerActivityVisibility::new(position);
                                        reconnect_backoff.recovered(now);
                                        let state = available_pointer_state(position, visible);
                                        thread_latest.send_if_modified(|latest| {
                                            if *latest == state {
                                                false
                                            } else {
                                                *latest = state;
                                                true
                                            }
                                        });
                                        tracing::info!(
                                            display = %bootstrap_display,
                                            pointer_surface_width = pointer_surface_dimensions.width,
                                            pointer_surface_height = pointer_surface_dimensions.height,
                                            "Gamescope Xwayland pointer feedback reconnected"
                                        );
                                    }
                                    Err(error) => {
                                        reconnect_backoff.failed(now);
                                        tracing::warn!(
                                            display = %bootstrap_display,
                                            error = %error,
                                            "Gamescope Xwayland pointer feedback reconnect incomplete"
                                        );
                                    }
                                }
                            }
                            Err(error) => {
                                reconnect_backoff.failed(now);
                                tracing::warn!(
                                    display = %bootstrap_display,
                                    error = %error,
                                    "Gamescope Xwayland pointer feedback reconnect failed"
                                );
                            }
                        }
                        continue;
                    }
                    let bootstrap_connection = bootstrap
                        .as_mut()
                        .expect("bootstrap connection was checked above");
                    let focus_display = match bootstrap_connection.query_focus_display() {
                        Ok(display) => display,
                        Err(error) => {
                            tracing::warn!(
                                display = %bootstrap_display,
                                error = %error,
                                "Gamescope bootstrap Xwayland connection was lost"
                            );
                            bootstrap = None;
                            active_connection = None;
                            activity_visibility.reset();
                            reconnect_backoff.failed(Instant::now());
                            publish_unavailable(&thread_latest);
                            continue;
                        }
                    };
                    if focus_display != active_display || active_connection.is_none() {
                        active_display = focus_display;
                        active_connection = connect_active_display(&active_display).ok();
                        activity_visibility.reset();
                    }
                    let Some(active) = active_connection.as_mut() else {
                        publish_unavailable(&thread_latest);
                        continue;
                    };
                    match query_pointer_position(
                        &active.connection,
                        active.root,
                        bootstrap_connection.pointer_surface_dimensions,
                    ) {
                        Ok(position) => {
                            let visible = activity_visibility.resolve(
                                position,
                                active.query_cursor_visible_bounded(),
                                Instant::now(),
                            );
                            let state = available_pointer_state(position, Some(visible));
                            thread_latest.send_if_modified(|latest| {
                                if *latest == state {
                                    false
                                } else {
                                    *latest = state;
                                    true
                                }
                            });
                        }
                        Err(_) => {
                            active_connection = None;
                            activity_visibility.reset();
                            publish_unavailable(&thread_latest);
                        }
                    }
                }
            })
            .context("starting Xwayland pointer sampler")?;
        Ok(Self {
            inner: Arc::new(TrackerInner { latest, stop }),
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn try_initialize(
        configured_display: Option<&str>,
        _pointer_surface_dimensions: PointerSurfaceDimensions,
    ) -> Result<Self> {
        if let Some(display) = configured_display {
            validate_display_name(display)?;
        }
        anyhow::bail!("Xwayland pointer feedback is supported only on Linux")
    }

    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<PointerState> {
        self.inner.latest.subscribe()
    }
}

#[cfg(target_os = "linux")]
fn connect_display(display: &str) -> Result<(x11rb::rust_connection::RustConnection, u32)> {
    use x11rb::connection::Connection;

    validate_display_name(display)?;
    let (connection, screen_index) = x11rb::connect(Some(display))
        .with_context(|| format!("connecting to Xwayland {display}"))?;
    let root = connection
        .setup()
        .roots
        .get(screen_index)
        .with_context(|| format!("Xwayland {display} selected an invalid screen"))?
        .root;
    Ok((connection, root))
}

#[cfg(target_os = "linux")]
struct BootstrapXwayland {
    display: String,
    connection: x11rb::rust_connection::RustConnection,
    root: u32,
    focus_atom: Option<u32>,
    focus_fallback_reported: bool,
    pointer_surface_dimensions: PointerSurfaceDimensions,
}

#[cfg(target_os = "linux")]
fn connect_bootstrap_display(display: &str) -> Result<BootstrapXwayland> {
    use x11rb::protocol::xproto::ConnectionExt;

    let (connection, root) =
        connect_display(display).context("connecting to bootstrap Gamescope Xwayland")?;
    let focus_atom = connection
        // SteamOS and older Gamescope builds may not publish this Bazzite-era
        // extension. Do not create the atom and mistake its existence for
        // compositor support.
        .intern_atom(true, FOCUS_DISPLAY_PROPERTY)
        .context("interning Gamescope mouse-focus property")?
        .reply()
        .context("receiving Gamescope mouse-focus atom")?
        .atom;
    let geometry = connection
        .get_geometry(root)
        .context("sending bootstrap X11 GetGeometry")?
        .reply()
        .context("receiving bootstrap X11 GetGeometry reply")?;
    let pointer_surface_dimensions = PointerSurfaceDimensions::new(geometry.width, geometry.height)
        .context("validating bootstrap Gamescope pointer surface dimensions")?;
    Ok(BootstrapXwayland {
        display: display.to_owned(),
        connection,
        root,
        focus_atom: (focus_atom != x11rb::NONE).then_some(focus_atom),
        focus_fallback_reported: false,
        pointer_surface_dimensions,
    })
}

#[cfg(target_os = "linux")]
impl BootstrapXwayland {
    fn query_focus_display(&mut self) -> Result<String> {
        let discovered = match self.focus_atom {
            Some(focus_atom) => query_focus_display(&self.connection, self.root, focus_atom)?,
            None => None,
        };
        let (focused_display, used_fallback) = select_focus_display(&self.display, discovered);
        if !used_fallback {
            if self.focus_fallback_reported {
                tracing::info!(
                    display = %self.display,
                    focus_display = %focused_display,
                    "Gamescope mouse-focus discovery recovered"
                );
            }
            self.focus_fallback_reported = false;
        } else if !self.focus_fallback_reported {
            tracing::warn!(
                display = %self.display,
                "Gamescope mouse-focus property is unavailable; pointer feedback is bounded to the configured Xwayland display"
            );
            self.focus_fallback_reported = true;
        }
        Ok(focused_display)
    }
}

#[cfg(any(target_os = "linux", test))]
fn select_focus_display(
    configured_display: &str,
    discovered_display: Option<String>,
) -> (String, bool) {
    match discovered_display {
        Some(discovered_display) => (discovered_display, false),
        None => (configured_display.to_owned(), true),
    }
}

#[cfg(target_os = "linux")]
fn publish_unavailable(latest: &tokio::sync::watch::Sender<PointerState>) {
    latest.send_if_modified(|latest| {
        if *latest == PointerState::UNAVAILABLE {
            false
        } else {
            *latest = PointerState::UNAVAILABLE;
            true
        }
    });
}

#[cfg(target_os = "linux")]
struct ActiveXwayland {
    display: String,
    connection: x11rb::rust_connection::RustConnection,
    root: u32,
    cursor_visible_atom: Option<u32>,
    visibility_fallback_reported: bool,
}

#[cfg(target_os = "linux")]
fn connect_active_display(display: &str) -> Result<ActiveXwayland> {
    use x11rb::protocol::xproto::ConnectionExt;

    let (connection, root) = connect_display(display)?;
    let cursor_visible_atom = connection
        .intern_atom(true, CURSOR_VISIBLE_PROPERTY)
        .context("interning Gamescope cursor-visibility property")?
        .reply()
        .context("receiving Gamescope cursor-visibility atom")?
        .atom;
    Ok(ActiveXwayland {
        display: display.to_owned(),
        connection,
        root,
        cursor_visible_atom: (cursor_visible_atom != x11rb::NONE).then_some(cursor_visible_atom),
        visibility_fallback_reported: false,
    })
}

#[cfg(target_os = "linux")]
impl ActiveXwayland {
    fn query_cursor_visible_bounded(&mut self) -> Option<bool> {
        match self
            .cursor_visible_atom
            .map(|atom| query_cursor_visible(&self.connection, self.root, atom))
            .transpose()
        {
            Ok(Some(visible)) => {
                if self.visibility_fallback_reported {
                    tracing::info!(
                        display = %self.display,
                        "Gamescope cursor-visibility feedback recovered"
                    );
                }
                self.visibility_fallback_reported = false;
                Some(visible)
            }
            Ok(None) => {
                if !self.visibility_fallback_reported {
                    tracing::warn!(
                        display = %self.display,
                        "Gamescope cursor-visibility property is unavailable; visibility will follow bounded pointer activity"
                    );
                    self.visibility_fallback_reported = true;
                }
                None
            }
            Err(error) => {
                if !self.visibility_fallback_reported {
                    tracing::warn!(
                        display = %self.display,
                        %error,
                        "Gamescope cursor-visibility property is invalid; visibility will follow bounded pointer activity"
                    );
                    self.visibility_fallback_reported = true;
                }
                None
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn query_cursor_visible(
    connection: &x11rb::rust_connection::RustConnection,
    root: u32,
    cursor_visible_atom: u32,
) -> Result<bool> {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};

    let reply = connection
        .get_property(false, root, cursor_visible_atom, AtomEnum::CARDINAL, 0, 1)
        .context("sending Gamescope cursor-visibility property query")?
        .reply()
        .context("receiving Gamescope cursor-visibility property")?;
    anyhow::ensure!(
        reply.format == 32,
        "Gamescope cursor-visibility property is not CARDINAL/32"
    );
    let mut values = reply
        .value32()
        .context("Gamescope cursor-visibility property cannot be read as CARDINAL/32")?;
    let visible = values
        .next()
        .context("Gamescope cursor-visibility property is empty")?;
    anyhow::ensure!(
        values.next().is_none(),
        "Gamescope cursor-visibility property has more than one value"
    );
    Ok(visible != 0)
}

fn validate_display_name(display: &str) -> Result<()> {
    let number = display.strip_prefix(':').unwrap_or_default();
    anyhow::ensure!(
        !number.is_empty() && number.len() <= 3 && number.bytes().all(|byte| byte.is_ascii_digit()),
        "Xwayland display must be : followed by 1 to 3 digits"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn query_focus_display(
    connection: &x11rb::rust_connection::RustConnection,
    root: u32,
    focus_atom: u32,
) -> Result<Option<String>> {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};

    let reply = connection
        .get_property(false, root, focus_atom, AtomEnum::CARDINAL, 0, 4)
        .context("sending Gamescope mouse-focus property query")?
        .reply()
        .context("receiving Gamescope mouse-focus property")?;
    parse_optional_focus_display(reply.format, &reply.value)
}

#[cfg(any(target_os = "linux", test))]
fn parse_optional_focus_display(format: u8, value: &[u8]) -> Result<Option<String>> {
    if format == 0 && value.is_empty() {
        return Ok(None);
    }
    anyhow::ensure!(
        format == 32,
        "Gamescope mouse-focus property is not CARDINAL/32"
    );
    parse_focus_display(value).map(Some)
}

#[cfg(any(target_os = "linux", test))]
fn parse_focus_display(value: &[u8]) -> Result<String> {
    let first_word = value
        .get(..4)
        .context("Gamescope mouse-focus property is shorter than one CARDINAL")?;
    let end = first_word
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(first_word.len());
    let display = std::str::from_utf8(&first_word[..end])
        .context("Gamescope mouse-focus display is not UTF-8")?
        .to_owned();
    validate_display_name(&display)?;
    Ok(display)
}

#[cfg(target_os = "linux")]
fn query_pointer_position(
    connection: &x11rb::rust_connection::RustConnection,
    root: u32,
    pointer_surface_dimensions: PointerSurfaceDimensions,
) -> Result<PointerPosition> {
    use x11rb::protocol::xproto::ConnectionExt;

    let geometry = connection
        .get_geometry(root)
        .context("sending X11 GetGeometry")?
        .reply()
        .context("receiving X11 GetGeometry reply")?;

    let reply = connection
        .query_pointer(root)
        .context("sending X11 QueryPointer")?
        .reply()
        .context("receiving X11 QueryPointer reply")?;
    anyhow::ensure!(
        reply.same_screen && reply.root == root,
        "Xwayland pointer is not on the queried active root"
    );
    map_pointer_position(
        geometry.width,
        geometry.height,
        i32::from(reply.root_x),
        i32::from(reply.root_y),
        pointer_surface_dimensions,
    )
    .context("mapping Xwayland pointer position to the captured surface")
}

#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PointerSurfaceRegion {
    offset_x: u16,
    offset_y: u16,
    width: u16,
    height: u16,
}

#[cfg(any(target_os = "linux", test))]
fn pointer_surface_region(
    width: u16,
    height: u16,
    pointer_surface_dimensions: PointerSurfaceDimensions,
) -> Result<PointerSurfaceRegion> {
    pointer_surface_dimensions
        .validate()
        .context("validating captured pointer surface dimensions")?;
    anyhow::ensure!(
        width > 0 && height > 0,
        "active Xwayland root dimensions must be non-zero"
    );

    // Gamescope centers a focused game surface while preserving its aspect
    // ratio. Reproduce that transform in the compositor's native pointer
    // coordinate space: independent axis stretching would make the overlay
    // disagree with the game whenever the active Xwayland root is letterboxed.
    let root_width = u64::from(width);
    let root_height = u64::from(height);
    let surface_width = u64::from(pointer_surface_dimensions.width);
    let surface_height = u64::from(pointer_surface_dimensions.height);
    let root_is_wider = root_width * surface_height >= root_height * surface_width;
    let (fitted_width, fitted_height) = if root_is_wider {
        (
            surface_width,
            root_height
                .checked_mul(surface_width)
                .context("pointer surface height scaling overflowed")?
                / root_width,
        )
    } else {
        (
            root_width
                .checked_mul(surface_height)
                .context("pointer surface width scaling overflowed")?
                / root_height,
            surface_height,
        )
    };
    anyhow::ensure!(
        fitted_width > 0 && fitted_height > 0,
        "active Xwayland root aspect cannot be represented on the captured pointer surface"
    );
    let fitted_width =
        u16::try_from(fitted_width).context("fitted pointer surface width exceeds u16")?;
    let fitted_height =
        u16::try_from(fitted_height).context("fitted pointer surface height exceeds u16")?;
    Ok(PointerSurfaceRegion {
        offset_x: (pointer_surface_dimensions.width - fitted_width) / 2,
        offset_y: (pointer_surface_dimensions.height - fitted_height) / 2,
        width: fitted_width,
        height: fitted_height,
    })
}

#[cfg(any(target_os = "linux", test))]
fn map_pointer_position(
    root_width: u16,
    root_height: u16,
    root_x: i32,
    root_y: i32,
    pointer_surface_dimensions: PointerSurfaceDimensions,
) -> Result<PointerPosition> {
    let region = pointer_surface_region(root_width, root_height, pointer_surface_dimensions)?;
    let x = map_pointer_axis(root_x, root_width, region.offset_x, region.width, "x")?;
    let y = map_pointer_axis(root_y, root_height, region.offset_y, region.height, "y")?;
    PointerPosition::new(x, y).context("scaled pointer position is outside the protocol range")
}

#[cfg(any(target_os = "linux", test))]
fn map_pointer_axis(
    coordinate: i32,
    root_extent: u16,
    surface_offset: u16,
    surface_extent: u16,
    axis: &str,
) -> Result<i32> {
    let coordinate = u64::try_from(coordinate)
        .with_context(|| format!("Xwayland returned a negative {axis} pointer coordinate"))?;
    let root_extent = u64::from(root_extent);
    let surface_extent = u64::from(surface_extent);
    anyhow::ensure!(
        coordinate < root_extent,
        "Xwayland {axis} pointer coordinate is outside the active root"
    );
    let scaled_within_region = coordinate
        .checked_mul(surface_extent)
        .context("pointer coordinate scaling overflowed")?
        / root_extent;
    let scaled = u64::from(surface_offset)
        .checked_add(scaled_within_region)
        .context("pointer surface offset overflowed")?;
    anyhow::ensure!(
        scaled_within_region < surface_extent,
        "scaled {axis} pointer coordinate is outside the captured surface"
    );
    i32::try_from(scaled).context("scaled pointer coordinate exceeds the protocol integer range")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "linux")]
    fn pointer_sampler_is_bounded_to_sixty_hertz() {
        assert!(POINTER_POLL_INTERVAL >= std::time::Duration::from_nanos(1_000_000_000 / 60));
    }

    #[test]
    fn parses_gamescope_cardinal_string_abi_fail_closed() {
        assert_eq!(parse_optional_focus_display(0, &[]).unwrap(), None);
        assert_eq!(
            parse_optional_focus_display(32, &[b':', b'0', 0, 0]).unwrap(),
            Some(":0".to_owned())
        );
        assert!(parse_optional_focus_display(8, b":0").is_err());
        assert!(parse_optional_focus_display(0, b":0").is_err());
        assert_eq!(parse_focus_display(&[b':', b'0', 0, 0]).unwrap(), ":0");
        assert_eq!(
            parse_focus_display(&[b':', b'1', 0, 0, 78, 0, 0, 0]).unwrap(),
            ":1"
        );
        assert!(parse_focus_display(&[b'0', 0, 0, 0]).is_err());
        assert!(parse_focus_display(&[b':', b'x', 0, 0]).is_err());
        assert!(parse_focus_display(b":0").is_err());
    }

    #[test]
    fn missing_gamescope_focus_extension_is_bounded_to_configured_display() {
        assert_eq!(select_focus_display(":0", None), (":0".to_owned(), true));
        assert_eq!(
            select_focus_display(":0", Some(":1".to_owned())),
            (":1".to_owned(), false)
        );
    }

    #[test]
    fn xwayland_pointer_mapping_is_identity_for_equal_surfaces() {
        let expected = PointerSurfaceDimensions::new(2_560, 1_600).unwrap();

        assert_eq!(
            pointer_surface_region(2_560, 1_600, expected).unwrap(),
            PointerSurfaceRegion {
                offset_x: 0,
                offset_y: 0,
                width: 2_560,
                height: 1_600,
            }
        );
        assert_eq!(
            map_pointer_position(2_560, 1_600, 212, 140, expected).unwrap(),
            PointerPosition::new(212, 140).unwrap()
        );
    }

    #[test]
    fn logical_xwayland_pointer_scales_to_native_capture_surface() {
        let native = PointerSurfaceDimensions::new(2_560, 1_600).unwrap();

        assert_eq!(
            map_pointer_position(1_280, 800, 212, 140, native).unwrap(),
            PointerPosition::new(424, 280).unwrap()
        );
        assert_eq!(
            map_pointer_position(1_280, 800, 1_279, 799, native).unwrap(),
            PointerPosition::new(2_558, 1_598).unwrap()
        );
    }

    #[test]
    fn pointer_mapping_uses_each_focused_xwayland_root_geometry() {
        let native = PointerSurfaceDimensions::new(2_560, 1_600).unwrap();

        let logical = map_pointer_position(1_280, 800, 212, 140, native).unwrap();
        let native_root = map_pointer_position(2_560, 1_600, 424, 280, native).unwrap();

        assert_eq!(logical, native_root);
    }

    #[test]
    fn wide_xwayland_root_maps_into_centered_letterbox_region() {
        let native = PointerSurfaceDimensions::new(2_560, 1_600).unwrap();

        assert_eq!(
            pointer_surface_region(1_280, 720, native).unwrap(),
            PointerSurfaceRegion {
                offset_x: 0,
                offset_y: 80,
                width: 2_560,
                height: 1_440,
            }
        );
        assert_eq!(
            map_pointer_position(1_280, 720, 0, 0, native).unwrap(),
            PointerPosition::new(0, 80).unwrap()
        );
        assert_eq!(
            map_pointer_position(1_280, 720, 640, 360, native).unwrap(),
            PointerPosition::new(1_280, 800).unwrap()
        );
        assert_eq!(
            map_pointer_position(1_280, 720, 1_279, 719, native).unwrap(),
            PointerPosition::new(2_558, 1_518).unwrap()
        );
    }

    #[test]
    fn narrow_xwayland_root_maps_into_centered_pillarbox_region() {
        let native = PointerSurfaceDimensions::new(2_560, 1_600).unwrap();

        assert_eq!(
            pointer_surface_region(1_024, 768, native).unwrap(),
            PointerSurfaceRegion {
                offset_x: 213,
                offset_y: 0,
                width: 2_133,
                height: 1_600,
            }
        );
        assert_eq!(
            map_pointer_position(1_024, 768, 0, 0, native).unwrap(),
            PointerPosition::new(213, 0).unwrap()
        );
        assert_eq!(
            map_pointer_position(1_024, 768, 512, 384, native).unwrap(),
            PointerPosition::new(1_279, 800).unwrap()
        );
    }

    #[test]
    fn pointer_mapping_rejects_invalid_geometry_and_coordinates() {
        let native = PointerSurfaceDimensions::new(2_560, 1_600).unwrap();

        assert!(map_pointer_position(0, 800, 0, 0, native).is_err());
        assert!(
            map_pointer_position(
                u16::MAX,
                1,
                0,
                0,
                PointerSurfaceDimensions::new(64, 64).unwrap(),
            )
            .is_err()
        );
        assert!(map_pointer_position(1_280, 800, -1, 0, native).is_err());
        assert!(map_pointer_position(1_280, 800, 1_280, 0, native).is_err());
        assert!(map_pointer_position(1_280, 800, 0, 800, native).is_err());
        assert!(
            map_pointer_position(
                1_280,
                800,
                0,
                0,
                PointerSurfaceDimensions {
                    width: 0,
                    height: 1_600,
                },
            )
            .is_err()
        );
    }

    #[test]
    fn pointer_mapping_is_bounded_at_maximum_supported_dimensions() {
        let maximum = PointerSurfaceDimensions::new(7_680, 4_320).unwrap();
        let mapped = map_pointer_position(65_520, 36_855, 65_519, 36_854, maximum).unwrap();

        assert!(mapped.x >= 0 && mapped.x < i32::from(maximum.width));
        assert!(mapped.y >= 0 && mapped.y < i32::from(maximum.height));
    }

    #[test]
    fn cursor_visibility_is_independent_and_fails_closed() {
        let position = PointerPosition::new(1_280, 800).unwrap();

        assert_eq!(
            available_pointer_state(position, Some(false)),
            PointerState {
                position: Some(position),
                visible: false,
            }
        );
        assert_eq!(
            available_pointer_state(position, None),
            PointerState {
                position: Some(position),
                visible: false,
            }
        );
        assert!(available_pointer_state(position, Some(true)).visible);
    }

    #[test]
    fn recent_authoritative_motion_recovers_gamescope_virtual_cursor_visibility() {
        let origin = PointerPosition::new(1_280, 800).unwrap();
        let moved = PointerPosition::new(1_312, 816).unwrap();
        let started_at = Instant::now();
        let mut visibility = PointerActivityVisibility::new(origin);

        assert!(!visibility.resolve(origin, Some(false), started_at));
        assert!(visibility.resolve(moved, Some(false), started_at));
        assert!(visibility.resolve(
            moved,
            Some(false),
            started_at + POINTER_ACTIVITY_VISIBLE_TIMEOUT - Duration::from_millis(1),
        ));
        assert!(!visibility.resolve(
            moved,
            Some(false),
            started_at + POINTER_ACTIVITY_VISIBLE_TIMEOUT,
        ));
    }

    #[test]
    fn compositor_visibility_wins_and_reconnect_does_not_inherit_motion() {
        let origin = PointerPosition::new(1_280, 800).unwrap();
        let moved = PointerPosition::new(1_312, 816).unwrap();
        let sampled_at = Instant::now();
        let mut visibility = PointerActivityVisibility::new(origin);

        assert!(visibility.resolve(origin, Some(true), sampled_at));
        assert!(visibility.resolve(moved, None, sampled_at));
        visibility.reset();
        assert!(!visibility.resolve(moved, None, sampled_at));
    }

    #[test]
    fn bootstrap_reconnect_backoff_is_bounded_and_resets_after_recovery() {
        let started_at = Instant::now();
        let mut backoff = ReconnectBackoff::new(started_at);

        assert!(backoff.ready(started_at));
        backoff.failed(started_at);
        assert!(!backoff.ready(started_at));
        assert!(backoff.ready(started_at + BOOTSTRAP_RECONNECT_INITIAL));

        let mut now = started_at + BOOTSTRAP_RECONNECT_INITIAL;
        for _ in 0..16 {
            backoff.failed(now);
            assert!(backoff.delay <= BOOTSTRAP_RECONNECT_MAXIMUM);
            now += BOOTSTRAP_RECONNECT_MAXIMUM;
        }
        assert_eq!(backoff.delay, BOOTSTRAP_RECONNECT_MAXIMUM);

        backoff.recovered(now);
        assert!(backoff.ready(now));
        assert_eq!(backoff.delay, BOOTSTRAP_RECONNECT_INITIAL);
    }
}
