//! Real client proxies on an unconnected private socket: no desktop or compositor.
use super::*;
use std::os::unix::net::UnixStream;
use wayland_client::WEnum;
use wayland_client::protocol::{wl_pointer, wl_registry::WlRegistry};

struct Wire;
wayland_client::delegate_noop!(Wire: ignore WlRegistry);

struct Fixture {
    state: WindowState<()>,
    connection: Connection,
    qh: QueueHandle<WindowState<()>>,
    compositor: WlCompositor,
    wm: XdgWmBase,
    registry: WlRegistry,
    // Keep the peer alive; never connect to a real Wayland socket.
    _peer: UnixStream,
}

impl Fixture {
    fn new() -> Self {
        let (socket, peer) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let queue = connection.new_event_queue::<WindowState<()>>();
        let qh = queue.handle();
        let wire_queue = connection.new_event_queue::<Wire>();
        let registry = connection.display().get_registry(&wire_queue.handle(), ());
        let compositor = registry.bind::<WlCompositor, _, _>(1, 4, &qh, ());
        let wm = registry.bind::<XdgWmBase, _, _>(2, 1, &qh, ());
        Self {
            state: WindowState::new("toplevel-regression"),
            connection,
            qh,
            compositor,
            wm,
            registry,
            _peer: peer,
        }
    }

    fn window(&mut self, popup: bool) -> id::Id {
        let id = id::Id::unique();
        let surface = self.compositor.create_surface(&self.qh, ());
        let xdg = self.wm.get_xdg_surface(&surface, &self.qh, ());
        let shell = if popup {
            let positioner = self.wm.create_positioner(&self.qh, ());
            let popup = xdg.get_popup(None, &positioner, &self.qh, ());
            Shell::PopUp((popup, xdg))
        } else {
            Shell::XdgTopLevel((xdg.get_toplevel(&self.qh, ()), xdg, None))
        };
        self.state.units.push(
            WindowStateUnitBuilder::new(
                id,
                self.qh.clone(),
                self.connection.display(),
                surface,
                self.compositor.clone(),
                shell,
            )
            .build(),
        );
        id
    }

    fn pointer(&mut self, name: u32) -> (WlSeat, wl_pointer::WlPointer) {
        let seat = self.registry.bind::<WlSeat, _, _>(name, 5, &self.qh, ());
        let pointer = seat.get_pointer(&self.qh, ());
        let mut storage = SeatStorage::new(seat.clone());
        storage.pointer = Some(pointer.clone());
        self.state.seats.insert(seat.id(), storage);
        (seat, pointer)
    }

    fn layer_window(&mut self) -> id::Id {
        let id = id::Id::unique();
        let surface = self.compositor.create_surface(&self.qh, ());
        let layers = self
            .registry
            .bind::<ZwlrLayerShellV1, _, _>(5, 1, &self.qh, ());
        let layer =
            layers.get_layer_surface(&surface, None, Layer::Top, "test".into(), &self.qh, ());
        self.state.units.push(
            WindowStateUnitBuilder::new(
                id,
                self.qh.clone(),
                self.connection.display(),
                surface,
                self.compositor.clone(),
                Shell::LayerShell(layer),
            )
            .build(),
        );
        id
    }

    fn event(&mut self, pointer: &wl_pointer::WlPointer, event: wl_pointer::Event) {
        <WindowState<()> as Dispatch<wl_pointer::WlPointer, ()>>::event(
            &mut self.state,
            pointer,
            event,
            &(),
            &self.connection,
            &self.qh,
        );
    }

    fn enter(&mut self, pointer: &wl_pointer::WlPointer, window: id::Id) {
        let surface = self
            .state
            .get_unit_with_id(window)
            .unwrap()
            .window
            .wl_surface
            .clone();
        self.event(
            pointer,
            wl_pointer::Event::Enter {
                serial: 10,
                surface,
                surface_x: 20.0,
                surface_y: 20.0,
            },
        );
    }

    fn button(&mut self, pointer: &wl_pointer::WlPointer, serial: u32, button: u32, pressed: bool) {
        self.event(
            pointer,
            wl_pointer::Event::Button {
                serial,
                time: 0,
                button,
                state: WEnum::Value(if pressed {
                    wl_pointer::ButtonState::Pressed
                } else {
                    wl_pointer::ButtonState::Released
                }),
            },
        );
    }
}

#[test]
fn parents_reject_missing_self_popup_and_removed_windows() {
    let mut f = Fixture::new();
    let parent = f.window(false);
    let popup = f.window(true);
    let layer = f.layer_window();
    let child = id::Id::unique();
    assert!(
        f.state
            .resolve_toplevel_parent(child, None)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        f.state
            .resolve_toplevel_parent(child, Some(parent))
            .unwrap(),
        f.state.toplevel(parent)
    );
    assert!(
        f.state
            .resolve_toplevel_parent(child, Some(id::Id::unique()))
            .is_err()
    );
    assert!(
        f.state
            .resolve_toplevel_parent(parent, Some(parent))
            .is_err()
    );
    assert!(f.state.resolve_toplevel_parent(child, Some(popup)).is_err());
    assert!(f.state.resolve_toplevel_parent(child, Some(layer)).is_err());
    f.state.units.retain(|unit| unit.id != parent);
    assert!(
        f.state
            .resolve_toplevel_parent(child, Some(parent))
            .is_err()
    );
}

#[test]
fn moves_require_matching_surface_and_consume_authorization_once() {
    let mut f = Fixture::new();
    let window = f.window(false);
    let other = f.window(false);
    let (seat, pointer) = f.pointer(3);
    assert!(f.state.take_move_request(window).is_none());
    f.enter(&pointer, window);
    f.button(&pointer, 41, 272, true);
    assert!(f.state.take_move_request(other).is_none());
    assert!(
        f.state.take_move_request(window).is_none(),
        "wrong-surface request consumes stale authorization"
    );
    f.button(&pointer, 42, 272, true);
    let (toplevel, grab) = f.state.take_move_request(window).unwrap();
    assert_eq!(Some(toplevel), f.state.toplevel(window));
    assert_eq!(grab.serial, 42);
    assert_eq!(grab.seat, seat);
    assert_eq!(grab.pointer, pointer);
    assert!(f.state.take_move_request(window).is_none());
}

#[test]
fn release_leave_and_non_left_buttons_cannot_authorize_moves() {
    let mut f = Fixture::new();
    let window = f.window(false);
    let (_, pointer) = f.pointer(3);
    f.enter(&pointer, window);
    f.button(&pointer, 41, 273, true);
    assert!(f.state.take_move_request(window).is_none());
    f.button(&pointer, 42, 272, true);
    f.button(&pointer, 43, 272, false);
    assert!(f.state.take_move_request(window).is_none());
    f.button(&pointer, 44, 272, true);
    let surface = f
        .state
        .get_unit_with_id(window)
        .unwrap()
        .window
        .wl_surface
        .clone();
    f.event(
        &pointer,
        wl_pointer::Event::Leave {
            serial: 45,
            surface,
        },
    );
    assert!(f.state.take_move_request(window).is_none());
    f.button(&pointer, 46, 272, true);
    assert!(
        f.state.take_move_request(window).is_none(),
        "press without an entered surface"
    );
}

#[test]
fn another_seats_surface_and_release_do_not_retarget_the_initiating_press() {
    let mut f = Fixture::new();
    let window = f.window(false);
    let other = f.window(false);
    let (seat, pointer) = f.pointer(3);
    let (_, other_pointer) = f.pointer(4);
    f.enter(&pointer, window);
    f.enter(&other_pointer, other);
    f.button(&pointer, 51, 272, true);
    f.button(&other_pointer, 52, 272, false);
    let (_, grab) = f.state.take_move_request(window).unwrap();
    assert_eq!(grab.window, window);
    assert_eq!(grab.seat, seat);
    assert_eq!(grab.serial, 51);
}

#[test]
fn removed_windows_and_popup_targets_cannot_be_moved() {
    let mut f = Fixture::new();
    let window = f.window(false);
    let popup = f.window(true);
    let (_, pointer) = f.pointer(3);
    f.enter(&pointer, popup);
    f.button(&pointer, 60, 272, true);
    assert!(f.state.take_move_request(popup).is_none());
    f.enter(&pointer, window);
    f.button(&pointer, 61, 272, true);
    f.state.units.retain(|unit| unit.id != window);
    assert!(f.state.take_move_request(window).is_none());
}

#[test]
fn dead_pointer_or_seat_invalidates_a_held_press() {
    for release_pointer in [true, false] {
        let mut f = Fixture::new();
        let window = f.window(false);
        let (seat, pointer) = f.pointer(3);
        f.enter(&pointer, window);
        f.button(&pointer, 70, 272, true);
        if release_pointer {
            pointer.release();
        } else {
            seat.release();
        }
        assert!(f.state.take_move_request(window).is_none());
        assert!(f.state.move_grab.is_none());
    }
}
