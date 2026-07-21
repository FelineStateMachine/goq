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
        use x11rb::protocol::xproto::ConnectionExt;

        let bootstrap_display = match configured_display {
            Some(display) => display.to_owned(),
            None => std::env::var("DISPLAY")
                .context("neither gamescope_pipewire.xwayland_display nor DISPLAY is configured")?,
        };
        validate_display_name(&bootstrap_display)?;
        let (bootstrap_connection, bootstrap_root) = connect_display(&bootstrap_display)
            .context("connecting to bootstrap Gamescope Xwayland")?;
        let focus_atom = bootstrap_connection
            .intern_atom(false, FOCUS_DISPLAY_PROPERTY)
            .context("interning Gamescope mouse-focus property")?
            .reply()
            .context("receiving Gamescope mouse-focus atom")?
            .atom;
        let focus_display = query_focus_display(&bootstrap_connection, bootstrap_root, focus_atom)
            .context("querying initial Gamescope mouse-focus display")?;
        let active = connect_active_display(&focus_display)
            .context("connecting to active Gamescope Xwayland")?;
        let initial_position =
            query_pointer_position(&active.connection, active.root, pointer_surface_dimensions)
                .context("querying initial Gamescope Xwayland pointer position")?;
        let initial = available_pointer_state(initial_position, query_cursor_visible(&active).ok());
        let (latest, _) = tokio::sync::watch::channel(initial);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_latest = latest.clone();
        let thread_stop = Arc::clone(&stop);
        std::thread::Builder::new()
            .name("sigil-xwayland-pointer".into())
            .spawn(move || {
                let mut active_display = focus_display;
                let mut active_connection = Some(active);
                let mut activity_visibility = PointerActivityVisibility::new(initial_position);
                while !thread_stop.load(Ordering::Acquire) {
                    std::thread::sleep(POINTER_POLL_INTERVAL);
                    let focus_display = match query_focus_display(
                        &bootstrap_connection,
                        bootstrap_root,
                        focus_atom,
                    ) {
                        Ok(display) => display,
                        Err(_) => {
                            activity_visibility.reset();
                            thread_latest.send_if_modified(|latest| {
                                if *latest == PointerState::UNAVAILABLE {
                                    false
                                } else {
                                    *latest = PointerState::UNAVAILABLE;
                                    true
                                }
                            });
                            continue;
                        }
                    };
                    if focus_display != active_display || active_connection.is_none() {
                        active_display = focus_display;
                        active_connection = connect_active_display(&active_display).ok();
                        activity_visibility.reset();
                    }
                    let Some(active) = active_connection.as_ref() else {
                        thread_latest.send_if_modified(|latest| {
                            if *latest == PointerState::UNAVAILABLE {
                                false
                            } else {
                                *latest = PointerState::UNAVAILABLE;
                                true
                            }
                        });
                        continue;
                    };
                    match query_pointer_position(
                        &active.connection,
                        active.root,
                        pointer_surface_dimensions,
                    ) {
                        Ok(position) => {
                            let visible = activity_visibility.resolve(
                                position,
                                query_cursor_visible(active).ok(),
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
                            thread_latest.send_if_modified(|latest| {
                                if *latest == PointerState::UNAVAILABLE {
                                    false
                                } else {
                                    *latest = PointerState::UNAVAILABLE;
                                    true
                                }
                            });
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
struct ActiveXwayland {
    connection: x11rb::rust_connection::RustConnection,
    root: u32,
    cursor_visible_atom: u32,
}

#[cfg(target_os = "linux")]
fn connect_active_display(display: &str) -> Result<ActiveXwayland> {
    use x11rb::protocol::xproto::ConnectionExt;

    let (connection, root) = connect_display(display)?;
    let cursor_visible_atom = connection
        .intern_atom(false, CURSOR_VISIBLE_PROPERTY)
        .context("interning Gamescope cursor-visibility property")?
        .reply()
        .context("receiving Gamescope cursor-visibility atom")?
        .atom;
    Ok(ActiveXwayland {
        connection,
        root,
        cursor_visible_atom,
    })
}

#[cfg(target_os = "linux")]
fn query_cursor_visible(active: &ActiveXwayland) -> Result<bool> {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};

    let reply = active
        .connection
        .get_property(
            false,
            active.root,
            active.cursor_visible_atom,
            AtomEnum::CARDINAL,
            0,
            1,
        )
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
) -> Result<String> {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};

    let reply = connection
        .get_property(false, root, focus_atom, AtomEnum::CARDINAL, 0, 4)
        .context("sending Gamescope mouse-focus property query")?
        .reply()
        .context("receiving Gamescope mouse-focus property")?;
    anyhow::ensure!(
        reply.format == 32,
        "Gamescope mouse-focus property is not CARDINAL/32"
    );
    parse_focus_display(&reply.value)
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
fn validate_root_geometry(
    width: u16,
    height: u16,
    pointer_surface_dimensions: PointerSurfaceDimensions,
) -> Result<()> {
    pointer_surface_dimensions
        .validate()
        .context("validating captured pointer surface dimensions")?;
    anyhow::ensure!(
        width > 0 && height > 0,
        "active Xwayland root dimensions must be non-zero"
    );

    // Widen before cross multiplication so every pair of u16 dimensions is
    // overflow-safe. Exact aspect equality prevents a focus switch from
    // silently stretching pointer coordinates into a differently shaped
    // captured surface.
    let root_aspect = u64::from(width) * u64::from(pointer_surface_dimensions.height);
    let surface_aspect = u64::from(height) * u64::from(pointer_surface_dimensions.width);
    anyhow::ensure!(
        root_aspect == surface_aspect,
        "active Xwayland root is {width}x{height}, but the captured pointer surface {}x{} has a different aspect ratio",
        pointer_surface_dimensions.width,
        pointer_surface_dimensions.height
    );
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn map_pointer_position(
    root_width: u16,
    root_height: u16,
    root_x: i32,
    root_y: i32,
    pointer_surface_dimensions: PointerSurfaceDimensions,
) -> Result<PointerPosition> {
    validate_root_geometry(root_width, root_height, pointer_surface_dimensions)?;
    let x = map_pointer_axis(root_x, root_width, pointer_surface_dimensions.width, "x")?;
    let y = map_pointer_axis(root_y, root_height, pointer_surface_dimensions.height, "y")?;
    PointerPosition::new(x, y).context("scaled pointer position is outside the protocol range")
}

#[cfg(any(target_os = "linux", test))]
fn map_pointer_axis(
    coordinate: i32,
    root_extent: u16,
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
    let scaled = coordinate
        .checked_mul(surface_extent)
        .context("pointer coordinate scaling overflowed")?
        / root_extent;
    anyhow::ensure!(
        scaled < surface_extent,
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
    fn xwayland_pointer_mapping_is_identity_for_equal_surfaces() {
        let expected = PointerSurfaceDimensions::new(2_560, 1_600).unwrap();

        validate_root_geometry(2_560, 1_600, expected).unwrap();
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
    fn pointer_mapping_rejects_invalid_geometry_and_coordinates() {
        let native = PointerSurfaceDimensions::new(2_560, 1_600).unwrap();

        assert!(map_pointer_position(0, 800, 0, 0, native).is_err());
        assert!(map_pointer_position(1_280, 720, 0, 0, native).is_err());
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
}
