//! Core data-device DnD with explicit, asynchronous offer completion.
//!
//! Dropped offers are owned independently of each seat's current hover offer.
//! A subsequent enter must not destroy an offer whose filesystem job is running.
use crate::{WindowState, events::DispatchMessageInner, id::Id};
use sctk::dispatch2::Dispatch2;
use std::{
    collections::HashMap,
    fs::File,
    io::{self, Write},
    os::fd::{AsFd, OwnedFd},
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Proxy, QueueHandle, WEnum,
    backend::ObjectId,
    globals::GlobalList,
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_data_device::{self, WlDataDevice},
        wl_data_device_manager::{self, DndAction, WlDataDeviceManager},
        wl_data_offer::{self, WlDataOffer},
        wl_data_source::{self, WlDataSource},
        wl_seat::WlSeat,
        wl_shm::{Format, WlShm},
        wl_surface::WlSurface,
    },
};

const MAX_PAYLOAD: usize = 4 * 1024 * 1024;
const MAX_MIME_TYPES: usize = 32;
const MAX_ITEMS: usize = 10_000;

#[derive(Debug, Clone)]
pub enum Event {
    Enter {
        offer: u64,
        x: f64,
        y: f64,
        mime_types: Vec<String>,
        copy: bool,
    },
    Actions {
        offer: u64,
        copy: bool,
    },
    Motion {
        offer: u64,
        x: f64,
        y: f64,
    },
    Leave {
        offer: u64,
    },
    Drop {
        offer: u64,
        x: f64,
        y: f64,
        copy: bool,
    },
    Payload {
        offer: u64,
        mime: String,
        reader: Arc<File>,
    },
    OfferCancelled {
        offer: u64,
    },
    SourceStarted {
        source: u64,
    },
    SourceDropped {
        source: u64,
    },
    SourceFinished {
        source: u64,
    },
    SourceCancelled {
        source: u64,
    },
    SourceError {
        source: u64,
        message: String,
    },
    OfferError {
        offer: u64,
        message: String,
    },
}

impl Event {
    pub fn scale_coordinates(&mut self, scale: f64) {
        if !scale.is_finite() || scale <= 0.0 {
            return;
        }
        match self {
            Self::Enter { x, y, .. } | Self::Motion { x, y, .. } | Self::Drop { x, y, .. } => {
                *x /= scale;
                *y /= scale;
            }
            _ => {}
        }
    }
}

#[derive(Clone)]
pub struct SourceData {
    pub identifier: u64,
    pub item_count: usize,
    /// MIME payloads describe local files, not the file contents themselves.
    pub payloads: Vec<(String, Arc<Vec<u8>>)>,
}

impl std::fmt::Debug for SourceData {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let formats: Vec<_> = self
            .payloads
            .iter()
            .map(|(mime, bytes)| (mime, bytes.len()))
            .collect();
        formatter
            .debug_struct("SourceData")
            .field("identifier", &self.identifier)
            .field("item_count", &self.item_count)
            .field("formats", &formats)
            .finish()
    }
}

impl SourceData {
    fn valid(&self) -> bool {
        self.identifier != 0
            && (1..=MAX_ITEMS).contains(&self.item_count)
            && !self.payloads.is_empty()
            && self.payloads.len() <= MAX_MIME_TYPES
            && self.payloads.iter().all(|(mime, bytes)| {
                !mime.is_empty() && mime.len() <= 256 && bytes.len() <= MAX_PAYLOAD
            })
    }
}

#[derive(Debug, Default)]
pub(crate) struct State {
    manager: Option<WlDataDeviceManager>,
    devices: HashMap<ObjectId, Device>,
    offers: HashMap<ObjectId, Offer>,
    sources: HashMap<ObjectId, Source>,
    next_offer: u64,
    writers: Arc<AtomicUsize>,
}

#[derive(Debug)]
struct Device {
    proxy: WlDataDevice,
    current: Option<ObjectId>,
}

#[derive(Debug)]
struct Offer {
    identifier: u64,
    proxy: WlDataOffer,
    mime_types: Vec<String>,
    window: Option<Id>,
    seat: Option<ObjectId>,
    serial: u32,
    x: f64,
    y: f64,
    dropped: bool,
    selected: DndAction,
    source_actions: DndAction,
    received: bool,
}

#[derive(Debug)]
struct Source {
    proxy: WlDataSource,
    window: Id,
    seat: ObjectId,
    data: SourceData,
    _icon: DragIcon,
    cancelled: Arc<AtomicBool>,
}

impl Drop for Source {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

/// Immutable storage lives until source completion, not merely wl_buffer.release.
#[derive(Debug)]
struct DragIcon {
    surface: WlSurface,
    buffer: WlBuffer,
    _file: File,
}

impl DragIcon {
    fn new<T: 'static>(
        compositor: &WlCompositor,
        shm: &WlShm,
        qh: &QueueHandle<WindowState<T>>,
        count: usize,
    ) -> io::Result<Self> {
        let pixels = drag_icon_pixels(count);
        let mut file = tempfile::tempfile()?;
        file.write_all(&pixels)?;

        let pool = shm.create_pool(file.as_fd(), pixels.len() as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            ICON_WIDTH as i32,
            ICON_HEIGHT as i32,
            (ICON_WIDTH * 4) as i32,
            Format::Argb8888,
            qh,
            (),
        );
        pool.destroy();
        let surface = compositor.create_surface(qh, ());

        Ok(Self {
            surface,
            buffer,
            _file: file,
        })
    }

    fn show(&self) {
        // A separate role surface uses one logical pixel per buffer pixel. This
        // deliberately makes no high-DPI claim or fractional-scale assumption.
        self.surface.attach(Some(&self.buffer), 0, 0);
        self.surface
            .damage(0, 0, ICON_WIDTH as i32, ICON_HEIGHT as i32);
        self.surface.commit();
    }
}

impl Drop for DragIcon {
    fn drop(&mut self) {
        if self.surface.is_alive() {
            self.surface.destroy();
        }
        if self.buffer.is_alive() {
            self.buffer.destroy();
        }
    }
}

const ICON_WIDTH: usize = 112;
const ICON_HEIGHT: usize = 48;

/// Native-endian ARGB8888, with only opaque and transparent pixels (premultiplied).
fn drag_icon_pixels(count: usize) -> Vec<u8> {
    const DIGITS: [[u8; 5]; 10] = [
        [7, 5, 5, 5, 7],
        [2, 6, 2, 2, 7],
        [7, 1, 7, 4, 7],
        [7, 1, 7, 1, 7],
        [5, 5, 7, 1, 1],
        [7, 4, 7, 1, 7],
        [7, 4, 7, 5, 7],
        [7, 1, 1, 1, 1],
        [7, 5, 7, 5, 7],
        [7, 5, 7, 1, 7],
    ];
    let mut pixels = vec![0; ICON_WIDTH * ICON_HEIGHT * 4];
    let mut rectangle = |x: usize, y: usize, width: usize, height: usize, color: u32| {
        for row in y..y.saturating_add(height).min(ICON_HEIGHT) {
            for column in x..x.saturating_add(width).min(ICON_WIDTH) {
                let offset = (row * ICON_WIDTH + column) * 4;
                pixels[offset..offset + 4].copy_from_slice(&color.to_ne_bytes());
            }
        }
    };

    // Neutral document stack and transfer arrow; no application branding.
    rectangle(4, 4, 27, 34, 0xff_30343b);
    rectangle(6, 6, 23, 30, 0xff_c9cdd3);
    rectangle(10, 8, 27, 34, 0xff_30343b);
    rectangle(12, 10, 23, 30, 0xff_f5f6f7);
    for y in [16, 21, 26] {
        rectangle(16, y, 15, 2, 0xff_747b85);
    }
    rectangle(29, 31, 13, 3, 0xff_30343b);
    for step in 0..5 {
        rectangle(38 + step, 27 + step, 2, 11 - step * 2, 0xff_30343b);
    }

    rectangle(46, 13, 64, 24, 0xff_30343b);
    // Five digits fit without changing allocation size. Preserve the actual
    // source count; clamp only this defensive raster input.
    let label = count.clamp(1, MAX_ITEMS).to_string();
    let left = 46 + (64 - (label.len() * 8 - 2)) / 2;
    for (index, digit) in label.bytes().enumerate() {
        for (row, bits) in DIGITS[(digit - b'0') as usize].iter().enumerate() {
            for column in 0..3 {
                if bits & (1 << (2 - column)) != 0 {
                    rectangle(
                        left + index * 8 + column * 2,
                        20 + row * 2,
                        2,
                        2,
                        0xff_ffffff,
                    );
                }
            }
        }
    }

    pixels
}

struct ManagerData;
struct DeviceData {
    seat: ObjectId,
}
struct OfferData;
struct SourceUserData;

impl State {
    pub(crate) fn bind<T: 'static>(globals: &GlobalList, qh: &QueueHandle<WindowState<T>>) -> Self {
        let mut state = Self::default();
        state.manager = globals
            .bind::<WlDataDeviceManager, _, _>(qh, 3..=3, ManagerData)
            .ok();
        state
    }
}

impl<T: 'static> WindowState<T> {
    pub(crate) fn add_dnd_seat(&mut self, seat: &WlSeat, qh: &QueueHandle<Self>) {
        if self.dnd.devices.contains_key(&seat.id()) {
            return;
        }
        if let Some(manager) = &self.dnd.manager {
            let proxy = manager.get_data_device(seat, qh, DeviceData { seat: seat.id() });
            self.dnd.devices.insert(
                seat.id(),
                Device {
                    proxy,
                    current: None,
                },
            );
        }
    }

    pub(crate) fn remove_dnd_seat(&mut self, seat: &WlSeat) {
        if self
            .move_grab
            .as_ref()
            .is_some_and(|grab| grab.seat == *seat)
        {
            self.move_grab = None;
        }
        if let Some(device) = self.dnd.devices.remove(&seat.id()) {
            device.proxy.release();
        }
        let offers: Vec<_> = self
            .dnd
            .offers
            .iter()
            .filter(|(_, offer)| offer.seat.as_ref() == Some(&seat.id()))
            .map(|(id, _)| id.clone())
            .collect();
        for id in offers {
            if let Some(offer) = self.dnd.offers.remove(&id) {
                self.dnd_event(
                    offer.window,
                    Event::OfferCancelled {
                        offer: offer.identifier,
                    },
                );
                offer.proxy.destroy();
            }
        }
        let sources: Vec<_> = self
            .dnd
            .sources
            .iter()
            .filter(|(_, source)| source.seat == seat.id())
            .map(|(id, _)| id.clone())
            .collect();
        for id in sources {
            self.end_dnd_source(&id, false);
        }
    }

    pub(crate) fn remove_dnd_window(&mut self, window: Id) {
        let offers: Vec<_> = self
            .dnd
            .offers
            .values()
            .filter(|offer| offer.window == Some(window))
            .map(|offer| offer.identifier)
            .collect();
        for offer in offers {
            self.finish_data_offer(offer, false);
        }

        let sources: Vec<_> = self
            .dnd
            .sources
            .iter()
            .filter(|(_, source)| source.window == window)
            .map(|(id, _)| id.clone())
            .collect();
        for source in sources {
            self.end_dnd_source(&source, false);
        }

        if self
            .move_grab
            .as_ref()
            .is_some_and(|grab| grab.window == window)
        {
            self.move_grab = None;
        }
        for seat in self.seats.values_mut() {
            if seat.pointer_surface == Some(window) {
                seat.pointer_surface = None;
            }
        }
        // The window is being removed. Do not deliver stale payload handles or
        // source notifications after its listener is gone.
        self.message.retain(|(id, event)| {
            *id != Some(window) || !matches!(event, DispatchMessageInner::Dnd(_))
        });
    }

    fn dnd_event(&mut self, window: Option<Id>, event: Event) {
        self.message
            .push((window, DispatchMessageInner::Dnd(event)));
    }

    pub fn start_data_drag(&mut self, window: Id, data: SourceData) {
        let identifier = data.identifier;
        let result = self.start_data_drag_inner(window, data);
        if let Err(error) = result {
            self.dnd_event(
                Some(window),
                Event::SourceError {
                    source: identifier,
                    message: error.to_string(),
                },
            );
        }
    }

    fn start_data_drag_inner(&mut self, window: Id, data: SourceData) -> io::Result<()> {
        if !data.valid() {
            return Err(io::Error::other(
                "Invalid drag item count or oversized drag MIME payload",
            ));
        }
        if self.dnd.sources.len() >= 16 {
            return Err(io::Error::other(
                "Too many outgoing transfers are waiting for other applications",
            ));
        }
        if self
            .dnd
            .sources
            .values()
            .any(|source| source.data.identifier == data.identifier)
        {
            return Err(io::Error::other("Drag source identifier already in use"));
        }
        let grab = self
            .move_grab
            .take()
            .ok_or_else(|| io::Error::other("No initiating pointer press for drag"))?;
        if grab.window != window || !grab.pointer.is_alive() || !grab.seat.is_alive() {
            return Err(io::Error::other(
                "Drag origin does not match the pointer grab",
            ));
        }
        let unit = self
            .get_unit_with_id(window)
            .ok_or_else(|| io::Error::other("Drag origin window disappeared"))?;
        let surface = unit.get_wlsurface().clone();
        let qh = unit.qh.clone();
        let manager = self
            .dnd
            .manager
            .as_ref()
            .ok_or_else(|| io::Error::other("Wayland data-device version 3 is unavailable"))?;
        let device = self
            .dnd
            .devices
            .get(&grab.seat.id())
            .ok_or_else(|| io::Error::other("Drag seat has no data device"))?;
        let compositor = self
            .wl_compositor
            .as_ref()
            .ok_or_else(|| io::Error::other("Wayland compositor is unavailable"))?;
        let shm = self
            .shm
            .as_ref()
            .ok_or_else(|| io::Error::other("Wayland shared memory is unavailable"))?;
        let icon = DragIcon::new(compositor, shm, &qh, data.item_count)?;
        let source = manager.create_data_source(&qh, SourceUserData);
        for (mime, _) in &data.payloads {
            source.offer(mime.clone());
        }
        // Set once before start_drag. External moves are deliberately unsupported.
        source.set_actions(DndAction::Copy);
        device
            .proxy
            .start_drag(Some(&source), &surface, Some(&icon.surface), grab.serial);
        icon.show();
        let identifier = data.identifier;
        self.dnd.sources.insert(
            source.id(),
            Source {
                proxy: source,
                window,
                seat: grab.seat.id(),
                data,
                _icon: icon,
                cancelled: Arc::new(AtomicBool::new(false)),
            },
        );
        self.dnd_event(Some(window), Event::SourceStarted { source: identifier });

        Ok(())
    }

    pub fn accept_data_offer(&mut self, identifier: u64, mime: Option<String>) {
        let Some(offer) = self
            .dnd
            .offers
            .values_mut()
            .find(|offer| offer.identifier == identifier && !offer.dropped)
        else {
            return;
        };
        let mime = mime.filter(|mime| offer.mime_types.contains(mime));
        offer.proxy.accept(offer.serial, mime.clone());
        offer.proxy.set_actions(
            if mime.is_some() {
                DndAction::Copy
            } else {
                DndAction::None
            },
            if mime.is_some() {
                DndAction::Copy
            } else {
                DndAction::None
            },
        );
    }

    pub fn receive_data_offer(&mut self, identifier: u64, mime: String) {
        let window = self
            .dnd
            .offers
            .values()
            .find(|offer| offer.identifier == identifier)
            .and_then(|offer| offer.window);
        let result = (|| {
            let offer = self
                .dnd
                .offers
                .values_mut()
                .find(|offer| offer.identifier == identifier)
                .ok_or_else(|| io::Error::other("Drag offer disappeared"))?;
            if !offer.dropped
                || offer.received
                || !copy_selected(offer.selected)
                || !offer.mime_types.contains(&mime)
            {
                return Err(io::Error::other("Offer is not an accepted copy drop"));
            }
            let (reader, writer) = UnixStream::pair()?;
            offer.proxy.receive(mime.clone(), writer.as_fd());
            offer.received = true;
            let fd: OwnedFd = reader.into();
            let window = offer.window;
            Ok((window, Arc::new(File::from(fd))))
        })();
        match result {
            Ok((window, reader)) => self.dnd_event(
                window,
                Event::Payload {
                    offer: identifier,
                    mime,
                    reader,
                },
            ),
            Err(error) => self.dnd_event(
                window,
                Event::OfferError {
                    offer: identifier,
                    message: error.to_string(),
                },
            ),
        }
    }

    pub fn finish_data_offer(&mut self, identifier: u64, success: bool) {
        let key = self
            .dnd
            .offers
            .iter()
            .find(|(_, offer)| offer.identifier == identifier)
            .map(|(key, _)| key.clone());
        if let Some(key) = key {
            for device in self.dnd.devices.values_mut() {
                if device.current.as_ref() == Some(&key) {
                    device.current = None;
                }
            }
            let Some(offer) = self.dnd.offers.remove(&key) else {
                return;
            };
            if success && offer.dropped && offer.received && copy_selected(offer.selected) {
                offer.proxy.finish();
            }
            offer.proxy.destroy();
        }
    }

    pub fn cancel_data_source(&mut self, identifier: u64) {
        let key = self
            .dnd
            .sources
            .iter()
            .find(|(_, source)| source.data.identifier == identifier)
            .map(|(key, _)| key.clone());
        if let Some(key) = key {
            self.end_dnd_source(&key, false);
        }
    }

    fn end_dnd_source(&mut self, key: &ObjectId, success: bool) {
        if let Some(source) = self.dnd.sources.remove(key) {
            self.dnd_event(
                Some(source.window),
                if success {
                    Event::SourceFinished {
                        source: source.data.identifier,
                    }
                } else {
                    Event::SourceCancelled {
                        source: source.data.identifier,
                    }
                },
            );
            source.proxy.destroy();
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        for source in self.sources.values() {
            if source.proxy.is_alive() {
                source.proxy.destroy();
            }
        }
        for offer in self.offers.values() {
            if offer.proxy.is_alive() {
                offer.proxy.destroy();
            }
        }
        for device in self.devices.values() {
            if device.proxy.is_alive() {
                device.proxy.release();
            }
        }
    }
}

fn copy_selected(action: DndAction) -> bool {
    action == DndAction::Copy
}

impl<T: 'static> Dispatch2<WlDataDeviceManager, WindowState<T>> for ManagerData {
    fn event(
        &self,
        _: &mut WindowState<T>,
        _: &WlDataDeviceManager,
        _: wl_data_device_manager::Event,
        _: &Connection,
        _: &QueueHandle<WindowState<T>>,
    ) {
    }
}

impl<T: 'static> Dispatch2<WlDataDevice, WindowState<T>> for DeviceData {
    wayland_client::event_created_child!(WindowState<T>, WlDataDevice, [0 => (WlDataOffer, OfferData)]);

    fn event(
        &self,
        state: &mut WindowState<T>,
        _: &WlDataDevice,
        event: wl_data_device::Event,
        _: &Connection,
        _: &QueueHandle<WindowState<T>>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { id } => {
                if state.dnd.offers.len() >= 256 {
                    id.destroy();
                    return;
                }
                let Some(next) = state.dnd.next_offer.checked_add(1) else {
                    id.destroy();
                    return;
                };
                state.dnd.next_offer = next;
                let identifier = state.dnd.next_offer;
                state.dnd.offers.insert(
                    id.id(),
                    Offer {
                        identifier,
                        proxy: id,
                        mime_types: Vec::new(),
                        window: None,
                        seat: None,
                        serial: 0,
                        x: 0.0,
                        y: 0.0,
                        dropped: false,
                        selected: DndAction::None,
                        source_actions: DndAction::None,
                        received: false,
                    },
                );
            }
            wl_data_device::Event::Enter {
                serial,
                surface,
                x,
                y,
                id,
            } => {
                let Some(id) = id else {
                    return;
                };
                let window = state.get_id_from_surface(&surface);
                let previous = state
                    .dnd
                    .devices
                    .get_mut(&self.seat)
                    .and_then(|device| device.current.replace(id.id()));
                if let Some(previous) = previous {
                    if state
                        .dnd
                        .offers
                        .get(&previous)
                        .is_some_and(|offer| !offer.dropped)
                    {
                        if let Some(previous) = state.dnd.offers.remove(&previous) {
                            previous.proxy.destroy();
                        }
                    }
                }
                if let Some(offer) = state.dnd.offers.get_mut(&id.id()) {
                    offer.window = window;
                    offer.seat = Some(self.seat.clone());
                    offer.serial = serial;
                    offer.x = x;
                    offer.y = y;
                    let event = Event::Enter {
                        offer: offer.identifier,
                        x,
                        y,
                        mime_types: offer.mime_types.clone(),
                        copy: offer.source_actions.contains(DndAction::Copy),
                    };
                    state.dnd_event(window, event);
                }
            }
            wl_data_device::Event::Motion { x, y, .. } => {
                let current = state
                    .dnd
                    .devices
                    .get(&self.seat)
                    .and_then(|device| device.current.as_ref());
                if let Some(offer) = current.and_then(|key| state.dnd.offers.get_mut(key)) {
                    offer.x = x;
                    offer.y = y;
                    let (window, event) = (
                        offer.window,
                        Event::Motion {
                            offer: offer.identifier,
                            x,
                            y,
                        },
                    );
                    state.dnd_event(window, event);
                }
            }
            wl_data_device::Event::Drop => {
                let current = state
                    .dnd
                    .devices
                    .get(&self.seat)
                    .and_then(|device| device.current.as_ref());
                if let Some(offer) = current.and_then(|key| state.dnd.offers.get_mut(key)) {
                    offer.dropped = true;
                    let (window, event) = (
                        offer.window,
                        Event::Drop {
                            offer: offer.identifier,
                            x: offer.x,
                            y: offer.y,
                            copy: copy_selected(offer.selected),
                        },
                    );
                    let rejected = !copy_selected(offer.selected);
                    let identifier = offer.identifier;
                    state.dnd_event(window, event);
                    if rejected {
                        state.finish_data_offer(identifier, false);
                    }
                }
            }
            wl_data_device::Event::Leave => {
                let previous = state
                    .dnd
                    .devices
                    .get_mut(&self.seat)
                    .and_then(|device| device.current.take());
                if let Some(key) = previous {
                    if let Some(offer) = state.dnd.offers.get(&key) {
                        let (window, event, dropped) = (
                            offer.window,
                            Event::Leave {
                                offer: offer.identifier,
                            },
                            offer.dropped,
                        );
                        state.dnd_event(window, event);
                        if !dropped {
                            if let Some(offer) = state.dnd.offers.remove(&key) {
                                offer.proxy.destroy();
                            }
                        }
                    }
                }
            }
            wl_data_device::Event::Selection { id: Some(id) } => {
                // Clipboard integration uses its existing independent connection.
                // These offers must not accumulate on this DnD-only data device.
                if let Some(offer) = state.dnd.offers.remove(&id.id()) {
                    offer.proxy.destroy();
                }
            }
            _ => {}
        }
    }
}

impl<T: 'static> Dispatch2<WlDataOffer, WindowState<T>> for OfferData {
    fn event(
        &self,
        state: &mut WindowState<T>,
        proxy: &WlDataOffer,
        event: wl_data_offer::Event,
        _: &Connection,
        _: &QueueHandle<WindowState<T>>,
    ) {
        let mut changed = None;
        if let Some(offer) = state.dnd.offers.get_mut(&proxy.id()) {
            match event {
                wl_data_offer::Event::Offer { mime_type }
                    if offer.mime_types.len() < MAX_MIME_TYPES && mime_type.len() <= 256 =>
                {
                    if !offer.mime_types.contains(&mime_type) {
                        offer.mime_types.push(mime_type);
                    }
                }
                wl_data_offer::Event::Action {
                    dnd_action: WEnum::Value(action),
                } => offer.selected = action,
                wl_data_offer::Event::SourceActions {
                    source_actions: WEnum::Value(actions),
                } => {
                    offer.source_actions = actions;
                    changed = Some((
                        offer.window,
                        Event::Actions {
                            offer: offer.identifier,
                            copy: actions.contains(DndAction::Copy),
                        },
                    ));
                }
                _ => {}
            }
        }
        if let Some((Some(window), event)) = changed {
            state.dnd_event(Some(window), event);
        }
    }
}

impl<T: 'static> Dispatch2<WlDataSource, WindowState<T>> for SourceUserData {
    fn event(
        &self,
        state: &mut WindowState<T>,
        proxy: &WlDataSource,
        event: wl_data_source::Event,
        _: &Connection,
        _: &QueueHandle<WindowState<T>>,
    ) {
        match event {
            wl_data_source::Event::Send { mime_type, fd } => {
                let payload = state.dnd.sources.get(&proxy.id()).and_then(|source| {
                    source
                        .data
                        .payloads
                        .iter()
                        .find(|(mime, _)| *mime == mime_type)
                        .map(|(_, bytes)| (bytes.clone(), source.cancelled.clone()))
                });
                if let Some((bytes, cancelled)) = payload {
                    if let Some(slot) = WriterSlot::claim(state.dnd.writers.clone()) {
                        let result = std::thread::Builder::new()
                            .name("drag-mime-writer".into())
                            .spawn(move || {
                                let _slot = slot;
                                let result =
                                    write_payload(fd, &bytes, &cancelled, Duration::from_secs(10));
                                if let Err(error) = result {
                                    log::debug!("Drag MIME payload transmission failed: {error}");
                                }
                            });
                        if let Err(error) = result {
                            log::warn!("Could not start drag MIME writer: {error}");
                        }
                    }
                }
            }
            wl_data_source::Event::Cancelled => state.end_dnd_source(&proxy.id(), false),
            wl_data_source::Event::DndFinished => state.end_dnd_source(&proxy.id(), true),
            wl_data_source::Event::DndDropPerformed => {
                if let Some(source) = state.dnd.sources.get(&proxy.id()) {
                    let (window, event) = (
                        source.window,
                        Event::SourceDropped {
                            source: source.data.identifier,
                        },
                    );
                    state.dnd_event(Some(window), event);
                }
            }
            _ => {}
        }
    }
}

struct WriterSlot(Arc<AtomicUsize>);

impl WriterSlot {
    fn claim(writers: Arc<AtomicUsize>) -> Option<Self> {
        loop {
            let current = writers.load(Ordering::Acquire);
            if current >= 4 {
                return None;
            }
            if writers
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(Self(writers));
            }
        }
    }
}

impl Drop for WriterSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

fn write_payload(
    fd: OwnedFd,
    bytes: &[u8],
    cancelled: &AtomicBool,
    timeout: Duration,
) -> io::Result<()> {
    use rustix::{
        event::{PollFd, PollFlags, Timespec, poll},
        fs::{OFlags, fcntl_getfl, fcntl_setfl},
    };

    // Data-source send FDs may be pipes or sockets; do not assume UnixStream.
    fcntl_setfl(&fd, fcntl_getfl(&fd)? | OFlags::NONBLOCK)?;
    let mut file = File::from(fd);
    let deadline = Instant::now() + timeout;
    let mut remaining = bytes;

    while !remaining.is_empty() {
        if cancelled.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Drag source cancelled",
            ));
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Drag MIME writer timed out",
            ));
        }

        match file.write(remaining) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(written) => remaining = &remaining[written..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                // Poll in short slices so source/window/seat cancellation also
                // interrupts a receiver that never drains its FD.
                let wait = (deadline - now).min(Duration::from_millis(50));
                let timeout = Timespec::try_from(wait).map_err(io::Error::other)?;
                let mut fds = [PollFd::new(&file, PollFlags::OUT)];
                match poll(&mut fds, Some(&timeout)) {
                    Ok(_) | Err(rustix::io::Errno::INTR) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => return Err(error),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn window_removal_cancels_dropped_offers_sources_and_stale_references() {
        let (socket, _peer) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let backend = connection.backend().downgrade();
        let window = Id::unique();
        let other_window = Id::unique();
        let seat = WlSeat::inert(backend.clone());
        let offer = WlDataOffer::inert(backend.clone());
        let source = WlDataSource::inert(backend.clone());
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut state = WindowState::<()>::default();

        state.dnd.devices.insert(
            seat.id(),
            Device {
                proxy: WlDataDevice::inert(backend.clone()),
                current: Some(offer.id()),
            },
        );
        state.dnd.offers.insert(
            offer.id(),
            Offer {
                identifier: 1,
                proxy: offer,
                mime_types: vec!["text/uri-list".into()],
                window: Some(window),
                seat: Some(seat.id()),
                serial: 1,
                x: 0.0,
                y: 0.0,
                dropped: true,
                selected: DndAction::Copy,
                source_actions: DndAction::Copy,
                received: true,
            },
        );
        state.dnd.sources.insert(
            source.id(),
            Source {
                proxy: source,
                window,
                seat: seat.id(),
                data: SourceData {
                    identifier: 2,
                    item_count: 1,
                    payloads: Vec::new(),
                },
                _icon: DragIcon {
                    surface: WlSurface::inert(backend.clone()),
                    buffer: WlBuffer::inert(backend.clone()),
                    _file: tempfile::tempfile().unwrap(),
                },
                cancelled: cancelled.clone(),
            },
        );
        let mut storage = crate::seat::SeatStorage::new(seat.clone());
        storage.pointer_surface = Some(window);
        state.seats.insert(seat.id(), storage);
        state.move_grab = Some(crate::MoveGrab {
            window,
            seat,
            pointer: wayland_client::protocol::wl_pointer::WlPointer::inert(backend),
            serial: 1,
        });
        state.dnd_event(
            Some(window),
            Event::Drop {
                offer: 1,
                x: 0.0,
                y: 0.0,
                copy: true,
            },
        );
        state.dnd_event(Some(other_window), Event::Leave { offer: 3 });

        state.remove_dnd_window(window);

        assert!(state.dnd.offers.is_empty());
        assert!(state.dnd.sources.is_empty());
        assert!(cancelled.load(Ordering::Acquire));
        assert!(
            state
                .dnd
                .devices
                .values()
                .all(|device| device.current.is_none())
        );
        assert!(state.move_grab.is_none());
        assert!(
            state
                .seats
                .values()
                .all(|seat| seat.pointer_surface.is_none())
        );
        assert_eq!(state.message.len(), 1);
        assert_eq!(state.message[0].0, Some(other_window));
    }

    #[test]
    fn writer_slots_are_bounded_and_release_on_drop_or_unwind() {
        let writers = Arc::new(AtomicUsize::new(0));
        let mut slots: Vec<_> = (0..4)
            .map(|_| WriterSlot::claim(writers.clone()).unwrap())
            .collect();

        assert!(WriterSlot::claim(writers.clone()).is_none());
        slots.pop();
        assert_eq!(writers.load(Ordering::Acquire), 3);
        let slot = WriterSlot::claim(writers.clone()).unwrap();
        // A failed spawn drops its captured closure without running it.
        let unspawned = move || drop(slot);
        drop(unspawned);
        assert_eq!(writers.load(Ordering::Acquire), 3);

        let _ = std::panic::catch_unwind(|| {
            let _slot = WriterSlot::claim(writers.clone()).unwrap();
            panic!("writer panic");
        });
        assert_eq!(writers.load(Ordering::Acquire), 3);
        drop(slots);
        assert_eq!(writers.load(Ordering::Acquire), 0);
    }

    #[test]
    fn stopped_pipe_reader_times_out_and_writer_closes_fd() {
        let (mut reader, writer) = io::pipe().unwrap();
        let writers = Arc::new(AtomicUsize::new(0));
        let slot = WriterSlot::claim(writers.clone()).unwrap();
        let bytes = vec![1; MAX_PAYLOAD];

        let worker = std::thread::spawn(move || {
            let _slot = slot;
            write_payload(
                writer.into(),
                &bytes,
                &AtomicBool::new(false),
                Duration::from_millis(25),
            )
        });
        let error = worker.join().unwrap().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(writers.load(Ordering::Acquire), 0);
        let mut received = Vec::new();
        // EOF proves the worker did not retain its write FD.
        reader.read_to_end(&mut received).unwrap();
        assert!(!received.is_empty());
        assert!(received.len() < MAX_PAYLOAD);
    }

    #[test]
    fn blocked_socket_writer_observes_source_cancellation_and_closes_fd() {
        let (mut reader, writer) = UnixStream::pair().unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = cancelled.clone();
        let writers = Arc::new(AtomicUsize::new(0));
        let slot = WriterSlot::claim(writers.clone()).unwrap();

        let worker = std::thread::spawn(move || {
            let _slot = slot;
            write_payload(
                writer.into(),
                &vec![1; MAX_PAYLOAD],
                &signal,
                Duration::from_secs(10),
            )
        });
        // Synchronize on actual writer progress, not thread scheduling or sleep.
        reader.read_exact(&mut [0; 1]).unwrap();
        let start = Instant::now();
        cancelled.store(true, Ordering::Release);
        let error = worker.join().unwrap().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(start.elapsed() < Duration::from_secs(3));
        assert_eq!(writers.load(Ordering::Acquire), 0);
        reader.read_to_end(&mut Vec::new()).unwrap();
    }

    #[test]
    fn draining_pipe_receives_complete_payload_and_eof() {
        let (mut reader, writer) = io::pipe().unwrap();
        let bytes = vec![7; MAX_PAYLOAD];
        let expected = bytes.clone();
        let worker = std::thread::spawn(move || {
            write_payload(
                writer.into(),
                &bytes,
                &AtomicBool::new(false),
                Duration::from_secs(10),
            )
        });

        let mut received = Vec::new();
        reader.read_to_end(&mut received).unwrap();

        worker.join().unwrap().unwrap();
        assert_eq!(received, expected);
    }

    #[test]
    fn slow_reader_does_not_extend_the_absolute_writer_deadline() {
        let (mut reader, writer) = UnixStream::pair().unwrap();
        let worker = std::thread::spawn(move || {
            write_payload(
                writer.into(),
                &vec![1; MAX_PAYLOAD],
                &AtomicBool::new(false),
                Duration::from_millis(25),
            )
        });

        let mut received = 0;
        let mut buffer = [0; 4096];
        loop {
            let count = reader.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            received += count;
            std::thread::sleep(Duration::from_millis(1));
        }

        assert_eq!(
            worker.join().unwrap().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(received > 0 && received < MAX_PAYLOAD);
    }

    #[test]
    fn external_sources_only_allow_copy_and_bounded_mime_data() {
        assert!(copy_selected(DndAction::Copy));
        assert!(!copy_selected(DndAction::Move));
        assert!(!copy_selected(DndAction::Ask));
        let mut source = SourceData {
            identifier: 1,
            item_count: 1,
            payloads: vec![(
                "text/uri-list".into(),
                Arc::new(b"file:///home/user/file\r\n".to_vec()),
            )],
        };
        assert!(source.valid());
        source.payloads[0].1 = Arc::new(vec![0; MAX_PAYLOAD + 1]);
        assert!(!source.valid());
    }

    #[test]
    fn source_item_count_is_bounded_without_truncation() {
        let mut source = SourceData {
            identifier: 1,
            item_count: 1,
            payloads: vec![("text/uri-list".into(), Arc::new(vec![1]))],
        };

        for count in [1, 9999, 10_000] {
            source.item_count = count;
            assert!(source.valid());
            assert_eq!(source.item_count, count);
        }
        for count in [0, 10_001, usize::MAX] {
            source.item_count = count;
            assert!(!source.valid());
        }
    }

    #[test]
    fn icon_raster_has_fixed_dimensions_and_bounded_extreme_counts() {
        for count in [0, 1, 9, 10, 99, 100, 999, 1000, 9999, 10_000, usize::MAX] {
            let pixels = drag_icon_pixels(count);

            assert_eq!(pixels.len(), ICON_WIDTH * ICON_HEIGHT * 4);
            assert!(pixels.chunks_exact(4).all(|pixel| matches!(
                u32::from_ne_bytes(pixel.try_into().unwrap()) >> 24,
                0 | 255
            )));
            assert_eq!(pixels, drag_icon_pixels(count));
        }
        assert_eq!(drag_icon_pixels(0), drag_icon_pixels(1));
        assert_eq!(drag_icon_pixels(usize::MAX), drag_icon_pixels(10_000));
    }

    #[test]
    fn icon_raster_draws_distinct_counts_including_five_digits() {
        let counts = [1, 2, 9, 10, 99, 100, 999, 1000, 9999, 10_000];

        for (index, count) in counts.iter().enumerate() {
            for other in &counts[index + 1..] {
                assert_ne!(drag_icon_pixels(*count), drag_icon_pixels(*other));
            }
        }
    }

    #[test]
    fn event_coordinates_follow_application_scaling() {
        let mut event = Event::Motion {
            offer: 1,
            x: 100.0,
            y: 60.0,
        };
        event.scale_coordinates(2.0);
        assert!(matches!(
            event,
            Event::Motion {
                x: 50.0,
                y: 30.0,
                ..
            }
        ));
    }
}
