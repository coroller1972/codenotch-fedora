//! X11/XWayland integration. Each worker owns its connection; a missing display
//! or a disappearing window is a recoverable error, never an Xlib fatal error.
use std::cell::RefCell;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConnectionExt, EventMask, KeyButMask, Window,
};
use x11rb::rust_connection::RustConnection;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tauri::Emitter;
use x11rb::protocol::Event;
use x11rb::protocol::shape::{ConnectionExt as _, SK, SO};
use x11rb::protocol::xproto::{ChangeWindowAttributesAux, ClipOrdering, NotifyDetail, Rectangle};

fn window_id(window: &tauri::WebviewWindow) -> Option<Window> {
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Xlib(h) => Some(h.window as u32),
        RawWindowHandle::Xcb(h) => Some(h.window.get()),
        _ => None,
    }
}

/// WebKit can lose DOM mouseout when an animated/composited child owns the
/// pointer. Observe native crossings of the persistent input shape instead.
/// These events also work when the pointer leaves for a native Wayland app.
fn start_pointer_events(window: &tauri::WebviewWindow, xid: Window) -> Result<(), String> {
    static WATCHED_WINDOW: std::sync::Mutex<Option<Window>> = std::sync::Mutex::new(None);
    let mut watched = WATCHED_WINDOW.lock().unwrap();
    if *watched == Some(xid) { return Ok(()); }
    let (connection, _) = x11rb::connect(None).map_err(|e| e.to_string())?;
    connection.change_window_attributes(xid, &ChangeWindowAttributesAux::new()
        .event_mask(EventMask::ENTER_WINDOW | EventMask::LEAVE_WINDOW | EventMask::STRUCTURE_NOTIFY))
        .map_err(|e| e.to_string())?.check().map_err(|e| e.to_string())?;
    let window = window.clone();
    std::thread::spawn(move || {
        while let Ok(event) = connection.wait_for_event() {
            let inside = match event {
                Event::EnterNotify(e) if e.event == xid && e.detail != NotifyDetail::INFERIOR => true,
                Event::LeaveNotify(e) if e.event == xid && e.detail != NotifyDetail::INFERIOR => false,
                Event::DestroyNotify(e) if e.window == xid => break,
                _ => continue,
            };
            let _ = window.emit("notch_pointer", inside);
        }
    });
    *watched = Some(xid);
    Ok(())
}

/// Keep the pill's wake zone receptive even while the rest of the transparent
/// window passes input through. XWayland cannot track a native Wayland cursor
/// to re-enable a window whose entire input region has been removed.
pub fn set_input_regions(window: &tauri::WebviewWindow, rects: &[[f64; 4]]) -> bool {
    let Some(xid) = window_id(window) else { return false };
    // GTK has no native handle yet during Tauri setup. The first DOM geometry
    // report reaches here once it exists, before we enable the wake region.
    if start_pointer_events(window, xid).is_err() { return false; }
    let rectangles = input_rectangles(rects);
    with_display(|connection, _| {
        connection.shape_rectangles(SO::SET, SK::INPUT, ClipOrdering::UNSORTED,
            xid, 0, 0, &rectangles).ok()?.check().ok()?;
        connection.flush().ok()?;
        Some(())
    }).is_some()
}

fn input_rectangles(rects: &[[f64; 4]]) -> Vec<Rectangle> {
    let valid: Vec<_> = rects.iter().filter(|r| r.iter().all(|v| v.is_finite()) && r[2] > 0.0 && r[3] > 0.0).collect();
    if valid.is_empty() { return Vec::new(); }
    // The gap between the pill, tooltip and handles also belongs to the widget.
    // Coordinates are physical pixels, just like the DOM's set_hot payload.
    let left = valid.iter().map(|r| r[0]).fold(f64::INFINITY, f64::min) - crate::HOT_PAD;
    let top = valid.iter().map(|r| r[1]).fold(f64::INFINITY, f64::min) - crate::HOT_PAD;
    let right = valid.iter().map(|r| r[0] + r[2]).fold(f64::NEG_INFINITY, f64::max) + crate::HOT_PAD;
    let bottom = valid.iter().map(|r| r[1] + r[3]).fold(f64::NEG_INFINITY, f64::max) + crate::HOT_PAD;
    let x = left.floor().clamp(0.0, i16::MAX as f64) as i16;
    let y = top.floor().clamp(0.0, i16::MAX as f64) as i16;
    let width = (right.ceil() - x as f64).clamp(0.0, u16::MAX as f64) as u16;
    let height = (bottom.ceil() - y as f64).clamp(0.0, u16::MAX as f64) as u16;
    if width == 0 || height == 0 { Vec::new() } else { vec![Rectangle { x, y, width, height }] }
}

thread_local! {
    static DISPLAY: RefCell<Option<(RustConnection, Window)>> = const { RefCell::new(None) };
}

fn with_display<T>(f: impl FnOnce(&RustConnection, Window) -> Option<T>) -> Option<T> {
    DISPLAY.with(|slot| {
        let mut display = slot.borrow_mut();
        if display.is_none() {
            let (connection, screen) = x11rb::connect(None).ok()?;
            let root = connection.setup().roots[screen].root;
            *display = Some((connection, root));
        }
        let (connection, root) = display.as_ref()?;
        f(connection, *root)
    })
}

fn property(connection: &RustConnection, window: Window, name: &[u8], kind: AtomEnum) -> Option<Vec<u32>> {
    let atom = connection.intern_atom(true, name).ok()?.reply().ok()?.atom;
    if atom == 0 { return None; }
    let reply = connection.get_property(false, window, atom, kind, 0, 4096).ok()?.reply().ok()?;
    let values = reply.value32()?.collect();
    Some(values)
}

pub fn left_button_down() -> bool {
    with_display(|connection, root| {
        Some(connection.query_pointer(root).ok()?.reply().ok()?.mask.contains(KeyButMask::BUTTON1))
    }).unwrap_or(false)
}

pub fn foreground_pid() -> u32 {
    with_display(|connection, root| {
        let window = *property(connection, root, b"_NET_ACTIVE_WINDOW", AtomEnum::WINDOW)?.first()?;
        property(connection, window, b"_NET_WM_PID", AtomEnum::CARDINAL)?.first().copied()
    }).unwrap_or(0)
}

/// EWMH activation only sees X11 clients. Native Wayland windows are deliberately
/// left to the compositor; no focus stealing or guessed application launch.
pub fn focus_terminal(pid: u32) -> bool {
    if pid == 0 { return false; }
    let maps = crate::focus::proc_maps();
    let chain = crate::focus::chain_of(pid, &maps.ppid);
    with_display(|connection, root| {
        let windows = property(connection, root, b"_NET_CLIENT_LIST_STACKING", AtomEnum::WINDOW)?;
        let window = windows.into_iter().rev().find(|window| {
            property(connection, *window, b"_NET_WM_PID", AtomEnum::CARDINAL)
                .and_then(|pids| pids.first().copied())
                .is_some_and(|p| chain.contains(&p))
        })?;
        let atom = connection.intern_atom(false, b"_NET_ACTIVE_WINDOW").ok()?.reply().ok()?.atom;
        let event = ClientMessageEvent::new(32, window, atom, [2, x11rb::CURRENT_TIME, 0, 0, 0]);
        connection.send_event(false, root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, event).ok()?.check().ok()?;
        connection.flush().ok()?;
        Some(true)
    }).unwrap_or(false)
}

/// /proc names may contain spaces and parentheses; the final ')' ends comm.
pub fn parse_stat(stat: &str) -> Option<(u32, String)> {
    let start = stat.find('(')?;
    let end = stat.rfind(')')?;
    if end <= start { return None; }
    let parent = stat[end + 1..].split_whitespace().nth(1)?.parse().ok()?;
    Some((parent, stat[start + 1..end].to_lowercase()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn input_region_keeps_the_pill_and_card_bridge_receptive() {
        use super::*;
        let tuples = |rects: &[[f64; 4]]| input_rectangles(rects).iter()
            .map(|r| (r.x, r.y, r.width, r.height)).collect::<Vec<_>>();
        assert_eq!(tuples(&[[316.0, 285.5, 44.0, 79.0]]), vec![(306, 275, 64, 100)]);
        assert_eq!(tuples(&[[310.0, 280.0, 50.0, 90.0], [25.0, 250.0, 270.0, 150.0]]),
            vec![(15, 240, 355, 170)]);
        assert!(input_rectangles(&[]).is_empty());
        assert!(input_rectangles(&[[f64::NAN, 0.0, 10.0, 10.0], [0.0, 0.0, 0.0, 10.0]]).is_empty());
        assert_eq!(tuples(&[[-5.0, -5.0, 30.0, 30.0]]), vec![(0, 0, 35, 35)]);
    }

    #[test]
    #[ignore = "Requires an isolated Xvfb display; temporarily owns its EWMH properties"]
    fn x11_window_activation_and_pointer_query() {
        use super::*;
        use x11rb::protocol::{Event, xproto::{ChangeWindowAttributesAux, CreateWindowAux, PropMode, WindowClass}};
        use x11rb::wrapper::ConnectionExt as _;
        let (connection, screen) = x11rb::connect(None).unwrap();
        let root = connection.setup().roots[screen].root;
        let window = connection.generate_id().unwrap();
        connection.create_window(x11rb::COPY_DEPTH_FROM_PARENT, window, root, 0, 0, 100, 100, 0,
            WindowClass::INPUT_OUTPUT, 0, &CreateWindowAux::new()).unwrap().check().unwrap();
        let atom = |name: &[u8]| connection.intern_atom(false, name).unwrap().reply().unwrap().atom;
        let clients = atom(b"_NET_CLIENT_LIST_STACKING");
        let active = atom(b"_NET_ACTIVE_WINDOW");
        let pid_atom = atom(b"_NET_WM_PID");
        connection.change_property32(PropMode::REPLACE, window, pid_atom, AtomEnum::CARDINAL, &[std::process::id()]).unwrap();
        connection.change_property32(PropMode::REPLACE, root, clients, AtomEnum::WINDOW, &[window]).unwrap();
        connection.change_property32(PropMode::REPLACE, root, active, AtomEnum::WINDOW, &[window]).unwrap();
        connection.change_window_attributes(root, &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY)).unwrap();
        connection.flush().unwrap();
        // Synchronize the setup before the thread-local connection reads it.
        connection.get_input_focus().unwrap().reply().unwrap();
        assert_eq!(foreground_pid(), std::process::id());
        assert!(!left_button_down());
        assert!(focus_terminal(std::process::id()));
        let Event::ClientMessage(event) = connection.wait_for_event().unwrap() else { panic!("missing activation event") };
        assert_eq!(event.window, window);
        assert_eq!(event.type_, active);
        connection.delete_property(root, clients).unwrap();
        connection.delete_property(root, active).unwrap();
        connection.destroy_window(window).unwrap().check().unwrap();
        assert_eq!(foreground_pid(), 0);
        assert!(!focus_terminal(std::process::id()));
    }

    #[test]
    fn proc_names_do_not_shift_parent_pid() {
        assert_eq!(super::parse_stat("123 (A terminal (child)) S 42 1 0"),
            Some((42, "a terminal (child)".into())));
        for stat in ["", "123", "123 (broken", "123 ()", "123 ) ("] {
            assert!(super::parse_stat(stat).is_none());
        }
    }
}
