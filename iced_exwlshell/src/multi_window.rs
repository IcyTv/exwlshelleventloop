use crate::redraw::{Policy, Targets};
use crate::reexport::{PopupAnchor, PopupConstraintAdjustment};
use crate::{
    DefaultStyle,
    actions::{ExwlShellCustomActionWithId, IcedNewPopupSettings},
    ime_preedit::ImeState,
    multi_window::window_manager::WindowManager,
    settings::VirtualKeyboardSettings,
    user_interface::UserInterfaces,
};
use crate::{
    actions::ExwlShellCustomAction, clipboard::ExwlShellClipboard, conversion, error::Error,
};
use crate::{
    event::{IcedWlShellEvent, WindowEvent as ExwlShellWindowEvent},
    proxy::IcedProxy,
    settings::Settings,
};
use exwlshellev::{
    DisplayWrapper, ExWlShellEvent, NewPopUpSettings, PixelSize, PopUpRepositionSettings,
    PopupPlacement, RefreshRequest, ReturnData, WindowState, WindowWrapper,
    id::Id as LayerShellId,
    reexport::{
        wayland_client::{WlCompositor, WlRegion},
        zwp_virtual_keyboard_v1,
    },
};
#[cfg(all(feature = "linux-theme-detection", target_os = "linux"))]
use futures::StreamExt;
#[cfg(not(all(feature = "linux-theme-detection", target_os = "linux")))]
use iced_core::theme::Mode;
use iced_core::{
    Event as IcedEvent, theme,
    window::{Event as IcedWindowEvent, Id as IcedId, RedrawRequest},
};
use iced_core::{Size, mouse::Cursor};
use iced_futures::{Executor, Runtime};
use iced_graphics::{Compositor, Shell, compositor};
use iced_program::Instance;
use iced_program::Program as IcedProgram;
use iced_runtime::Action;
use iced_runtime::user_interface;
use iced_wayland_subscriber::shell;
use std::time::Instant;
use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
    mem,
    os::fd::AsFd,
    sync::Arc,
    time::Duration,
};
use window_manager::Window;

mod popup_visuals;
mod state;
mod window_manager;

struct EnableSurfaceTransition;
struct FocusVisibility {
    pose: crate::popup_animation::Pose,
    hidden: bool,
}
impl iced_core::widget::Operation for FocusVisibility {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn iced_core::widget::Operation)) {
        operate(self);
    }
    fn focusable(
        &mut self,
        _: Option<&iced_core::widget::Id>,
        bounds: iced_core::Rectangle,
        state: &mut dyn iced_core::widget::operation::Focusable,
    ) {
        let visible = self.pose.visible_bounds(bounds);
        if state.is_focused() && (visible.width <= 0.0 || visible.height <= 0.0) {
            self.hidden = true;
        }
    }
}
impl iced_core::widget::Operation for EnableSurfaceTransition {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn iced_core::widget::Operation)) {
        operate(self);
    }
    fn custom(
        &mut self,
        id: Option<&iced_core::widget::Id>,
        _: iced_core::Rectangle,
        state: &mut dyn std::any::Any,
    ) {
        if id == Some(&iced_core::widget::Id::new("exwlshell.popup.grow")) {
            if let Some(enabled) = state.downcast_mut::<bool>() {
                *enabled = true;
            }
        }
    }
}

type MultiRuntime<E, Message> = Runtime<E, IcedProxy<Action<Message>>, Action<Message>>;

/// Visible popup bounds reported by widgets, independently of root UI capacity.
/// Coordinates are application-logical; the backend applies application scaling.
#[derive(Default)]
struct PopupWindowGeometry(Option<iced_core::Rectangle>);

fn scaled_popup_geometry(bounds: iced_core::Rectangle, scale: f64) -> (i32, i32, i32, i32) {
    (
        (bounds.x as f64 * scale).round() as i32,
        (bounds.y as f64 * scale).round() as i32,
        (bounds.width as f64 * scale).ceil().max(1.0) as i32,
        (bounds.height as f64 * scale).ceil().max(1.0) as i32,
    )
}
impl iced_core::widget::Operation for PopupWindowGeometry {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn iced_core::widget::Operation)) {
        operate(self);
    }
    fn container(&mut self, id: Option<&iced_core::widget::Id>, bounds: iced_core::Rectangle) {
        if id
            == Some(&iced_core::widget::Id::new(
                crate::actions::POPUP_WINDOW_GEOMETRY_ID,
            ))
        {
            self.0 = Some(bounds);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PresentRecovery {
    Reconfigure,
    Recreate,
    Report,
    Fatal,
}

fn scaled_popup_size(size: Size, scale: f64) -> Option<Size<u32>> {
    let width = (size.width as f64 * scale).ceil() as u32;
    let height = (size.height as f64 * scale).ceil() as u32;

    (width > 0 && height > 0).then_some(Size::new(width, height))
}

fn present_recovery(error: &compositor::SurfaceError) -> PresentRecovery {
    match error {
        compositor::SurfaceError::Outdated => PresentRecovery::Reconfigure,
        compositor::SurfaceError::Lost => PresentRecovery::Recreate,
        compositor::SurfaceError::Timeout | compositor::SurfaceError::Other => {
            PresentRecovery::Report
        }
        compositor::SurfaceError::Occluded => PresentRecovery::Report,
        compositor::SurfaceError::OutOfMemory => PresentRecovery::Fatal,
    }
}

// a dispatch loop, another is listen loop
pub fn run<P>(
    program: P,
    namespace: &str,
    settings: Settings,
    lock: bool,
    on_new_shell: Option<crate::NewShellHook<P::Message>>,
    redraw_policy: Policy<P::Message>,
) -> Result<(), Error>
where
    P: IcedProgram + 'static,
    P::Theme: DefaultStyle,
    P::Message: 'static + TryInto<ExwlShellCustomActionWithId, Error = P::Message>,
{
    let compositor_settings = iced_core::backend::Settings {
        antialiasing: settings.antialiasing,
        ..Default::default()
    };
    let renderer_settings = iced_core::renderer::Settings {
        font: settings.default_font,
        text_size: settings.default_text_size,
        ..Default::default()
    };

    use exwlshellev::calloop::channel::channel;
    let (message_sender, message_receiver) = channel::<Action<P::Message>>();

    let boot_span = iced_debug::boot();
    let proxy = IcedProxy::new(message_sender);

    #[cfg(feature = "debug")]
    {
        let proxy = proxy.clone();

        iced_debug::on_hotpatch(move || {
            proxy.send_action(Action::Reload);
        });
    }

    let proxy_back = proxy.clone();
    let mut runtime: MultiRuntime<P::Executor, P::Message> = {
        let executor = P::Executor::new().map_err(Error::ExecutorCreationFailed)?;

        Runtime::new(executor, proxy)
    };

    let (application, task) = runtime.enter(|| Instance::new(program));

    if let Some(stream) = iced_runtime::task::into_stream(task) {
        runtime.run(stream);
    }

    runtime.track(iced_futures::subscription::into_recipes(
        runtime.enter(|| application.subscription().map(Action::Output)),
    ));

    let ev: WindowState<iced_core::window::Id> = exwlshellev::WindowState::new(namespace)
        .with_start_mode(settings.layer_settings.start_mode)
        .with_use_display_handle(true)
        .with_events_transparent(settings.layer_settings.events_transparent)
        .with_size(settings.layer_settings.size)
        .with_layer(settings.layer_settings.layer)
        .with_anchor(settings.layer_settings.anchor)
        .with_exclusive_zone(settings.layer_settings.exclusive_zone)
        .with_margin(settings.layer_settings.margin)
        .with_keyboard_interacivity(settings.layer_settings.keyboard_interactivity)
        .with_blur_option(settings.layer_settings.blur_option)
        .with_connection(settings.with_connection)
        .build()
        .expect("Cannot create layershell");

    #[cfg(all(feature = "linux-theme-detection", target_os = "linux"))]
    let system_theme = {
        let to_mode = |color_scheme| match color_scheme {
            mundy::ColorScheme::NoPreference => theme::Mode::None,
            mundy::ColorScheme::Light => theme::Mode::Light,
            mundy::ColorScheme::Dark => theme::Mode::Dark,
        };

        runtime.run(
            mundy::Preferences::stream(mundy::Interest::ColorScheme)
                .map(move |preferences| {
                    Action::System(iced_runtime::system::Action::NotifyTheme(to_mode(
                        preferences.color_scheme,
                    )))
                })
                .boxed(),
        );

        runtime
            .enter(|| {
                mundy::Preferences::once_blocking(
                    mundy::Interest::ColorScheme,
                    core::time::Duration::from_millis(200),
                )
            })
            .map(|preferences| to_mode(preferences.color_scheme))
            .unwrap_or_default()
    };

    #[cfg(not(all(feature = "linux-theme-detection", target_os = "linux")))]
    let system_theme = Mode::default();

    let context = Context::<
        P,
        <P as iced_program::Program>::Executor,
        <P::Renderer as iced_graphics::compositor::Default>::Compositor,
    >::new(
        application,
        compositor_settings,
        renderer_settings,
        runtime,
        on_new_shell,
        settings.shell_broadcast,
        settings.fonts,
        system_theme,
        proxy_back,
        settings.keep_compositor_alive,
        settings.popup_transitions,
        redraw_policy,
    )
    .lock(lock);
    let mut context_state = ContextState::Context(context);
    boot_span.finish();

    let mut waiting_layer_shell_events = VecDeque::new();

    ev.running_with_proxy(message_receiver, move |event, ev, layer_shell_id| {
        let mut def_returndata = ReturnData::None;
        match event {
            ExWlShellEvent::InitRequest => {
                def_returndata = ReturnData::RequestBind;
            }
            ExWlShellEvent::BindProvide(globals, qh) => {
                let wl_compositor = globals
                    .bind::<WlCompositor, _, _>(qh, 1..=1, ())
                    .expect("could not bind wl_compositor");
                waiting_layer_shell_events.push_back((
                    None,
                    IcedWlShellEvent::UpdateInputRegion(wl_compositor.create_region(qh, ())),
                ));

                if let Some(virtual_keyboard_setting) = settings.virtual_keyboard_support.as_ref() {
                    let virtual_keyboard_manager = globals
                        .bind::<zwp_virtual_keyboard_v1::ZwpVirtualKeyboardManagerV1, _, _>(
                            qh,
                            1..=1,
                            (),
                        )
                        .expect("no support virtual_keyboard");
                    let VirtualKeyboardSettings {
                        file,
                        keymap_size,
                        keymap_format,
                    } = virtual_keyboard_setting;
                    let seat = ev.get_seat();
                    let virtual_keyboard_in =
                        virtual_keyboard_manager.create_virtual_keyboard(seat, qh, ());
                    virtual_keyboard_in.keymap((*keymap_format).into(), file.as_fd(), *keymap_size);
                    ev.set_virtual_keyboard(virtual_keyboard_in);
                }
            }
            ExWlShellEvent::RequestMessages(message) => {
                let window_event = ExwlShellWindowEvent::from_dispatch(message, ev);
                waiting_layer_shell_events
                    .push_back((layer_shell_id, IcedWlShellEvent::Window(window_event)));
            }
            ExWlShellEvent::UserEvent(event) => {
                waiting_layer_shell_events
                    .push_back((layer_shell_id, IcedWlShellEvent::UserAction(event)));
            }
            ExWlShellEvent::NormalDispatch => {
                waiting_layer_shell_events
                    .push_back((layer_shell_id, IcedWlShellEvent::NormalDispatch));
            }
            _ => {}
        }
        loop {
            let mut need_continue = false;
            context_state = match std::mem::replace(&mut context_state, ContextState::None) {
                ContextState::None => unreachable!("context state is taken but not returned"),
                ContextState::Context(context) => {
                    if let Some((layer_shell_id, layer_shell_event)) =
                        waiting_layer_shell_events.pop_front()
                    {
                        need_continue = true;
                        let (context_state, waiting_layer_shell_event) =
                            context.handle_event(ev, layer_shell_id, layer_shell_event);
                        if let Some(waiting_layer_shell_event) = waiting_layer_shell_event {
                            waiting_layer_shell_events
                                .push_front((layer_shell_id, waiting_layer_shell_event));
                        }
                        context_state
                    } else {
                        ContextState::Context(context)
                    }
                }
            };
            if !need_continue {
                break;
            }
        }
        def_returndata
    })?;
    Ok(())
}

enum ContextState<Context> {
    None,
    Context(Context),
}

struct Context<P, E, C>
where
    P: IcedProgram + 'static,
    C: Compositor<Renderer = P::Renderer> + 'static,
    E: Executor + 'static,
    P::Theme: DefaultStyle,
    P::Message: 'static,
{
    compositor_settings: iced_core::backend::Settings,
    renderer_settings: iced_core::renderer::Settings,
    runtime: MultiRuntime<E, P::Message>,
    on_new_shell: Option<crate::NewShellHook<P::Message>>,
    shell_broadcast: shell::ShellSender,
    system_theme: iced_core::theme::Mode,
    fonts: Vec<Cow<'static, [u8]>>,
    compositor: Option<C>,
    window_manager: WindowManager<P, C>,
    cached_layer_dimensions: HashMap<IcedId, (Size<u32>, f32)>,
    clipboard: ExwlShellClipboard,
    wl_input_region: Option<WlRegion>,
    user_interfaces: UserInterfaces<P>,
    pending_popup_settings: HashMap<IcedId, IcedNewPopupSettings>,
    waiting_layer_shell_actions: Vec<(Option<IcedId>, ExwlShellCustomAction)>,
    iced_events: Vec<(IcedId, IcedEvent)>,
    messages: Vec<P::Message>,
    proxy: IcedProxy<Action<P::Message>>,
    time: Instant,
    keep_compositor_alive: bool,
    redraw_policy: Policy<P::Message>,
    popup_transitions: Option<crate::popup_animation::Profile>,
    visuals: HashMap<LayerShellId, popup_visuals::Visual<C>>,
}

impl<P, E, C> Context<P, E, C>
where
    P: IcedProgram + 'static,
    C: Compositor<Renderer = P::Renderer> + 'static,
    E: Executor + 'static,
    P::Theme: DefaultStyle,
    P::Message: 'static + TryInto<ExwlShellCustomActionWithId, Error = P::Message>,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        application: Instance<P>,
        compositor_settings: iced_core::backend::Settings,
        renderer_settings: iced_core::renderer::Settings,
        runtime: MultiRuntime<E, P::Message>,
        on_new_shell: Option<crate::NewShellHook<P::Message>>,
        shell_broadcast: shell::ShellSender,
        fonts: Vec<Cow<'static, [u8]>>,
        system_theme: iced_core::theme::Mode,
        proxy: IcedProxy<Action<P::Message>>,
        keep_compositor_alive: bool,
        popup_transitions: Option<crate::popup_animation::Profile>,
        redraw_policy: Policy<P::Message>,
    ) -> Self {
        Self {
            on_new_shell,
            shell_broadcast,
            compositor_settings,
            renderer_settings,
            runtime,
            system_theme,
            fonts,
            keep_compositor_alive,
            compositor: Default::default(),
            window_manager: WindowManager::new(),
            cached_layer_dimensions: HashMap::new(),
            clipboard: ExwlShellClipboard::unconnected(),
            wl_input_region: Default::default(),
            user_interfaces: UserInterfaces::new(application),
            pending_popup_settings: HashMap::new(),
            waiting_layer_shell_actions: Default::default(),
            iced_events: Default::default(),
            messages: Default::default(),
            proxy,
            time: Instant::now(),
            redraw_policy,
            popup_transitions,
            visuals: HashMap::new(),
        }
    }

    pub fn lock(mut self, lock: bool) -> Self {
        if lock {
            self.waiting_layer_shell_actions
                .push((None, ExwlShellCustomAction::Lock));
        }
        self
    }

    fn build_user_interface(
        ev: &mut WindowState<IcedId>,
        user_interfaces: &mut UserInterfaces<P>,
        iced_id: IcedId,
        cache: user_interface::Cache,
        window: &mut Window<P, C>,
        measure_popup: bool,
    ) {
        window.interaction_epoch = window.interaction_epoch.wrapping_add(1);
        let build_size = if measure_popup {
            Size::new(4096.0, 4096.0)
        } else {
            window.state.viewport().logical_size()
        };

        user_interfaces.build(
            iced_id,
            cache,
            &mut window.renderer.borrow_mut(),
            build_size,
        );

        if !measure_popup {
            return;
        }

        let Some(mut ui) = user_interfaces.ui_mut(&iced_id) else {
            return;
        };
        let scale = window.state.application_scale_factor();
        let capacity = scaled_popup_size(ui.min_size(), scale);
        let mut geometry = PopupWindowGeometry::default();
        ui.operate(&window.renderer.borrow(), &mut geometry);
        window.popup_geometry = geometry.0;
        let rect = geometry
            .0
            .map(|bounds| scaled_popup_geometry(bounds, scale));
        if let Some(capacity) = capacity
            && let Some(unit) = ev.get_mut_unit_with_id(window.id)
        {
            unit.set_popup_surface_geometry(rect, (capacity.width, capacity.height));
        }
        let target_size = rect
            .map(|(_, _, width, height)| Size::new(width as u32, height as u32))
            .or(capacity);

        if let Some(target_size) = target_size
            && let Some(popup_settings) = window.popup_settings
            && popup_settings.size.to_set() != (target_size.width, target_size.height)
        {
            let settings = IcedNewPopupSettings {
                size: PixelSize::px(target_size.width, target_size.height),
                ..popup_settings
            };
            window.popup_settings = Some(settings);
            ev.append_return_data(ReturnData::PopUpReposition((
                PopUpRepositionSettings {
                    size: settings.size,
                    placement: settings.placement,
                    anchor: settings.anchor,
                    gravity: settings.gravity,
                    constraint_adjustment: settings.constraint_adjustment,
                },
                window.id,
            )));
        }

        ui.relayout(
            window.state.viewport().logical_size(),
            &mut window.renderer.borrow_mut(),
        );
    }

    /// Create compositor synchronously. This is a one-time init that must finish
    /// before the first frame can render. Copies iced_winit logic.
    fn create_compositor(&mut self, window: Arc<WindowWrapper>, display: DisplayWrapper) {
        let shell = Shell::new(self.proxy.clone());
        let compositor_future = C::new(
            self.compositor_settings.clone(),
            display,
            window.clone(),
            shell,
        );
        let mut new_compositor =
            futures::executor::block_on(compositor_future).expect("Cannot create compositor");
        for font in self.fonts.clone() {
            new_compositor.load_font(font);
        }
        self.compositor = Some(new_compositor);
        // Opt-out for clients that don't need clipboard access. Skipping
        // connect() avoids spawning the smithay-clipboard worker thread,
        // which otherwise runs an always-on wayland event loop (~0.4% CPU).
        self.clipboard = if crate::clipboard::is_disabled() {
            ExwlShellClipboard::unconnected()
        } else {
            ExwlShellClipboard::connect(&window)
        };
    }

    fn remove_compositor(&mut self) {
        self.compositor = None;
        self.clipboard = ExwlShellClipboard::unconnected();
    }

    fn handle_event(
        mut self,
        ev: &mut WindowState<IcedId>,
        layer_shell_id: Option<LayerShellId>,
        layer_shell_event: IcedWlShellEvent<P::Message>,
    ) -> (ContextState<Self>, Option<IcedWlShellEvent<P::Message>>) {
        tracing::debug!(
            "Handle layer shell event, layer_shell_id: {:?},  waiting actions: {}, messages: {}",
            layer_shell_id,
            self.waiting_layer_shell_actions.len(),
            self.messages.len(),
        );
        if let IcedWlShellEvent::Window(ExwlShellWindowEvent::Refresh) = layer_shell_event
            && self.compositor.is_none()
        {
            let Some(layer_shell_window) = layer_shell_id.and_then(|lid| ev.get_unit_with_id(lid))
            else {
                tracing::error!("layer shell window not found: {:?}", layer_shell_id);
                return (ContextState::Context(self), None);
            };
            tracing::debug!("creating compositor");
            let window = layer_shell_window.gen_wrapper();
            let display = ev.display_wrapper();
            self.create_compositor(window, display);
        }

        match layer_shell_event {
            IcedWlShellEvent::UpdateInputRegion(region) => self.wl_input_region = Some(region),
            IcedWlShellEvent::Window(ExwlShellWindowEvent::Refresh) => {
                self.handle_refresh_event(ev, layer_shell_id)
            }
            IcedWlShellEvent::Window(ExwlShellWindowEvent::Closed) => {
                self.handle_closed_event(ev, layer_shell_id)
            }
            IcedWlShellEvent::Window(window_event) => {
                self.handle_window_event(ev, layer_shell_id, window_event)
            }
            IcedWlShellEvent::UserAction(user_action) => self.handle_user_action(ev, user_action),
            IcedWlShellEvent::NormalDispatch => self.handle_normal_dispatch(ev),
        }

        // at each interaction try to resolve those waiting actions.
        let mut waiting_layer_shell_actions = Vec::new();
        mem::swap(
            &mut self.waiting_layer_shell_actions,
            &mut waiting_layer_shell_actions,
        );
        for (iced_id, action) in waiting_layer_shell_actions {
            self.handle_layer_shell_action(ev, iced_id, action);
        }

        (ContextState::Context(self), None)
    }

    fn handle_refresh_event(
        &mut self,
        ev: &mut WindowState<IcedId>,
        layer_shell_id: Option<LayerShellId>,
    ) {
        if let Some(id) = layer_shell_id {
            if let Some(visual) = self.visuals.get(&id) {
                if let Some(native) = visual.native {
                    if visual.pending.is_some() && !visual.finished {
                        self.draw_retained_visual(ev, id);
                    } else {
                        self.handle_refresh_event(ev, Some(native));
                    }
                } else {
                    self.draw_retained_visual(ev, id);
                }
                return;
            }
            if let Some(visual) = self.visuals.values_mut().find(|v| v.native == Some(id)) {
                if visual.pending.is_some() {
                    return;
                }
                // Drawing can mutate retained layout/gesture state without an
                // application view rebuild. Old pixels must not authorize it.
                visual.confirmed = None;
            }
        }
        let Some(ex_wlshell_window) = layer_shell_id.and_then(|lid| ev.get_unit_with_id(lid))
        else {
            return;
        };
        let unit_id = ex_wlshell_window.id();
        let (width, height) = ex_wlshell_window.get_size();
        let scale_float = ex_wlshell_window.scale_float();
        if width == 0 || height == 0 {
            ev.request_refresh(unit_id, RefreshRequest::NextFrame);
            return;
        }
        // events may not be handled after RequestRefreshWithWrapper in the same
        // interaction, we dispatched them immediately.
        let mut events = Vec::new();
        let (iced_id, window, is_new_window) =
            if let Some((iced_id, window)) = self.window_manager.get_mut_alias(unit_id) {
                let window_size = window.state.window_size();

                if window_size.width != width
                    || window_size.height != height
                    || window.state.wayland_scale_factor() != scale_float
                {
                    let layout_span = iced_debug::layout(iced_id);
                    window.state.update_view_port(width, height, scale_float);
                    if let Some(ui) = self.user_interfaces.ui_mut(&iced_id) {
                        ui.relayout(
                            window.state.viewport().logical_size(),
                            &mut window.renderer.borrow_mut(),
                        );
                    }
                    layout_span.finish();
                    events.push(IcedEvent::Window(IcedWindowEvent::Resized(
                        window.state.window_size_f32(),
                    )));
                }
                (iced_id, window, false)
            } else {
                let wrapper = ex_wlshell_window.gen_wrapper();
                let iced_id = ex_wlshell_window
                    .get_binding()
                    .copied()
                    .unwrap_or_else(IcedId::unique);
                let shell_type = match ex_wlshell_window.wl_shell_type() {
                    exwlshellev::WlShellType::LayerShell => shell::ShellType::LayerShell,
                    exwlshellev::WlShellType::PopUp => shell::ShellType::PopUp,
                    exwlshellev::WlShellType::XdgTopLevel => shell::ShellType::XdgTopLevel,
                    exwlshellev::WlShellType::InputPanel => shell::ShellType::InputPanel,
                    exwlshellev::WlShellType::SessionLock => shell::ShellType::SessionLock,
                    exwlshellev::WlShellType::Visual => return,
                };
                let info = shell::ShellInfo {
                    window: iced_id,
                    shell: shell_type,
                };
                self.shell_broadcast.send(shell::ShellEvent::NewShell(info));
                if let Some(output) = ev
                    .get_unit_with_id(unit_id)
                    .and_then(|unit| unit.get_wloutput().cloned())
                    && let Some(inner) = ev.get_output_info_of(&output)
                {
                    self.shell_broadcast
                        .send(shell::ShellEvent::WindowOutputChanged {
                            window: iced_id,
                            output: Some(inner),
                        });
                }
                if let Some(message) = self.on_new_shell.as_ref().and_then(|f| f(info)) {
                    ev.request_refresh_all(RefreshRequest::NextFrame);
                    let (caches, application) = self.user_interfaces.extract_all();
                    update(
                        application,
                        &mut self.runtime,
                        &mut vec![message],
                        &mut self.waiting_layer_shell_actions,
                    );
                    for (_, window) in self.window_manager.iter_mut() {
                        window.state.synchronize(application);
                    }
                    iced_debug::theme_changed(|| {
                        self.window_manager.first().and_then(|window| {
                            <P::Theme as theme::Base>::seed(window.state.theme())
                        })
                    });
                    for (iced_id, cache) in caches {
                        let Some(window) = self.window_manager.get_mut(iced_id) else {
                            continue;
                        };
                        self.user_interfaces.build(
                            iced_id,
                            cache,
                            &mut window.renderer.borrow_mut(),
                            window.state.viewport().logical_size(),
                        );
                    }
                }

                let is_first = self.window_manager.is_empty();
                let proxy = self.proxy.clone();
                let waker = iced_core::shell::Waker::new(move || {
                    proxy.send_action(Action::Event {
                        window: iced_id,
                        event: IcedEvent::Waken,
                    });
                });

                let window = self.window_manager.insert(
                    iced_id,
                    (width, height),
                    scale_float,
                    wrapper,
                    self.user_interfaces.application(),
                    self.compositor
                        .as_mut()
                        .expect("It should have been created"),
                    self.renderer_settings,
                    waker,
                    self.system_theme,
                );

                iced_debug::theme_changed(|| {
                    if is_first {
                        <P::Theme as theme::Base>::seed(window.state.theme())
                    } else {
                        None
                    }
                });

                let theme = window.state.theme().mode();
                if self.system_theme != theme {
                    self.runtime
                        .broadcast(iced_futures::subscription::Event::SystemThemeChanged(theme));
                }

                window.popup_settings = self.pending_popup_settings.remove(&iced_id);
                window.visual_popup =
                    self.popup_transitions.is_some() && window.popup_settings.is_some();

                Self::build_user_interface(
                    ev,
                    &mut self.user_interfaces,
                    iced_id,
                    user_interface::Cache::default(),
                    window,
                    shell_type == shell::ShellType::PopUp,
                );

                events.push(IcedEvent::Window(IcedWindowEvent::Opened {
                    position: None,
                    size: window.state.window_size_f32(),
                    scale_factor: window.state.application_scale_factor() as f32,
                }));
                (iced_id, window, true)
            };

        let compositor = self
            .compositor
            .as_mut()
            .expect("The compositor should have been created");

        let content_updated_at = self.user_interfaces.content_updated_at(&iced_id);
        window.interaction_epoch = window.interaction_epoch.wrapping_add(1);
        let mut ui = self
            .user_interfaces
            .ui_mut(&iced_id)
            .expect("Get User interface");

        if self.popup_transitions.is_some() && window.popup_settings.is_some() {
            ui.operate(&window.renderer.borrow(), &mut EnableSurfaceTransition);
        }

        let mut cursor = if ev.is_mouse_surface(unit_id) {
            window.state.cursor()
        } else {
            Cursor::Unavailable
        };
        let sampled_pose = self
            .visuals
            .values()
            .find(|v| v.native == Some(unit_id))
            .map(|visual| {
                let bounds =
                    iced_core::Rectangle::with_size(window.state.viewport().logical_size());
                let sample = visual.timeline.sample(Instant::now(), bounds);
                let placement = ev
                    .popup_visual_placement(unit_id)
                    .unwrap_or(visual.placement);
                let source = visual.pointer_placement.unwrap_or(placement);
                let scale = window.state.application_scale_factor() as f32;
                let translation = crate::popup_animation::rebase_input(
                    source.origin,
                    placement.origin,
                    visual.pointer_scale.unwrap_or(scale as f64),
                    scale as f64,
                );
                cursor = if sample.0.accepts_input() {
                    cursor * translation * sample.0.transformation.inverse()
                } else {
                    Cursor::Unavailable
                };
                sample
            });

        events.push(IcedEvent::Window(IcedWindowEvent::RedrawRequested(
            Instant::now(),
        )));

        let draw_span = iced_debug::draw(iced_id);
        let (ui_state, statuses) = ui.update(
            window.raw.as_ref(),
            &window.waker,
            &events,
            cursor,
            &mut window.renderer.borrow_mut(),
            &mut self.messages,
        );
        run_clipboard(
            &mut self.clipboard,
            &ui_state,
            iced_id,
            &mut self.iced_events,
        );

        let physical_size = window.state.viewport().physical_size();

        if !is_new_window
            && self
                .cached_layer_dimensions
                .get(&iced_id)
                .is_none_or(|(size, scale)| {
                    *size != physical_size || *scale != window.state.viewport().scale_factor()
                })
        {
            self.cached_layer_dimensions.insert(
                iced_id,
                (physical_size, window.state.viewport().scale_factor()),
            );

            if physical_size.width > 0 && physical_size.height > 0 {
                compositor.configure_surface(
                    &mut window.surface,
                    physical_size.width,
                    physical_size.height,
                );
            }
        }

        for (idx, event) in events.into_iter().enumerate() {
            let status = statuses
                .get(idx)
                .cloned()
                .unwrap_or(iced_core::event::Status::Ignored);
            self.runtime
                .broadcast(iced_futures::subscription::Event::Interaction {
                    window: iced_id,
                    event,
                    status,
                });
        }

        ui.draw(
            &mut window.renderer.borrow_mut(),
            window.state.theme(),
            &iced_core::renderer::Style {
                text_color: window.state.text_color(),
            },
            cursor,
        );
        draw_span.finish();

        // get layer_shell_id so that layer_shell_window can be drop, and ev can be borrow mut
        let layer_shell_id = unit_id;

        Self::handle_ui_state(ev, window, ui_state, false, true);

        window.draw_preedit();

        if physical_size.width == 0 || physical_size.height == 0 {
            return;
        }

        if is_new_window {
            if self.popup_transitions.is_some() && window.popup_settings.is_some() {
                if let Some(unit) = ev.get_unit_with_id(unit_id) {
                    unit.set_visual_input_region(&[]);
                }
            }
            tracing::debug!(?iced_id, "skipping first present for new window");
            ev.request_refresh(layer_shell_id, RefreshRequest::NextFrame);
            return;
        }

        if let Some(profile) = self
            .popup_transitions
            .filter(|_| window.popup_settings.is_some())
        {
            assert!(
                compositor.supports_frames(),
                "popup transitions require immutable compositor frames"
            );
            assert!(
                ev.supports_presentation(),
                "popup transitions require presentation feedback"
            );
            let Some(placement) = ev.popup_visual_placement(unit_id) else {
                ev.request_refresh(unit_id, RefreshRequest::NextFrame);
                return;
            };
            let existing = self
                .visuals
                .iter()
                .find(|(_, v)| v.native == Some(unit_id))
                .map(|(id, _)| *id);
            let visual_id = if let Some(id) = existing {
                id
            } else {
                let settings = window.popup_settings.expect("popup settings");
                let reusable = self
                    .visuals
                    .iter()
                    .find(|(_, v)| {
                        v.native.is_none()
                            && v.placement == placement
                            && v.settings.parent == settings.parent
                            && v.settings.placement == settings.placement
                            && v.viewport.scale_factor() == window.state.viewport().scale_factor()
                            && v.viewport.logical_size() == window.state.viewport().logical_size()
                    })
                    .map(|(id, _)| *id);
                if let Some(id) = reusable {
                    let visual = self.visuals.get_mut(&id).expect("visual");
                    visual.native = Some(unit_id);
                    visual.owner = iced_id;
                    visual.settings = settings;
                    visual.timeline.retarget(visual.pose, false);
                    visual.finished = false;
                    visual.retire_deadline = None;
                    visual.confirmed = None;
                    visual.native_capacity = None;
                    id
                } else {
                    let above = settings.parent.and_then(|parent| {
                        self.visuals
                            .iter()
                            .find(|(_, v)| v.owner == parent)
                            .map(|(id, _)| *id)
                    });
                    let id = ev
                        .create_visual_host(placement, above)
                        .expect("visual subsurface unavailable");
                    let wrapper = ev.get_unit_with_id(id).expect("visual unit").gen_wrapper();
                    let surface = compositor.create_surface(
                        wrapper,
                        physical_size.width,
                        physical_size.height,
                    );
                    self.visuals.insert(
                        id,
                        popup_visuals::Visual::new(
                            unit_id,
                            iced_id,
                            settings,
                            surface,
                            window.state.viewport().clone(),
                            placement,
                            profile,
                        ),
                    );
                    id
                }
            };
            let visual = self.visuals.get_mut(&visual_id).expect("visual");
            let viewport = window.state.viewport();
            let capacity = (physical_size.width, physical_size.height, placement.scale);
            if visual.native_capacity != Some(capacity) {
                let presentation = compositor::FramePresentation {
                    transformation: iced_core::Transformation::IDENTITY,
                    opacity: 1.0,
                    clip: iced_core::Rectangle::with_size(viewport.logical_size()),
                };
                if let Err(error) = compositor.present_frame(
                    None,
                    presentation,
                    &mut window.surface,
                    viewport,
                    || ev.request_next_present(unit_id),
                ) {
                    Self::recover_frame_surface(
                        compositor,
                        &mut window.surface,
                        window.raw.clone(),
                        viewport,
                        &error,
                    );
                    ev.request_refresh(unit_id, RefreshRequest::NextFrame);
                    return;
                }
                visual.native_capacity = Some(capacity);
            }
            if visual.placement != placement || visual.viewport.physical_size() != physical_size {
                compositor.configure_surface(
                    &mut visual.surface,
                    physical_size.width,
                    physical_size.height,
                );
            }
            ev.configure_visual_host(visual_id, placement);
            visual.placement = placement;
            visual.viewport = viewport.clone();
            visual.bounds = iced_core::Rectangle::with_size(viewport.logical_size());
            visual.visible = window.popup_geometry.unwrap_or(visual.bounds);
            visual.frame = compositor.capture_frame(
                &mut window.renderer.borrow_mut(),
                viewport,
                window.state.background_color(),
            );
            visual.frame_content = content_updated_at;
            visual.frame_epoch = window.interaction_epoch;
            visual.frame_ime = window.ime_descriptor();
            visual.application_scale = window.state.application_scale_factor() as f32;
            let (pose, finished) = sampled_pose
                .unwrap_or_else(|| visual.timeline.sample(Instant::now(), visual.bounds));
            assert!(pose.is_valid(), "invalid popup transition pose");
            let region = pose.visible_bounds(visual.bounds);
            if let Some(unit) = ev.get_unit_with_id(unit_id) {
                let scale = window.state.application_scale_factor() as f32;
                let left = (region.x * scale).floor() as i32;
                let top = (region.y * scale).floor() as i32;
                let right = ((region.x + region.width) * scale).ceil() as i32;
                let bottom = ((region.y + region.height) * scale).ceil() as i32;
                if pose.accepts_input() {
                    unit.set_visual_input_region(&[(left, top, right - left, bottom - top)]);
                } else {
                    unit.set_visual_input_region(&[]);
                }
                unit.get_wlsurface().commit();
            }
            let stamp = Instant::now();
            tracing::debug!(target: "iced_exwlshell::pacing", ?visual_id, ?iced_id, root = ?placement.root, scale = pose.transformation.scale_factor(), "PACE_BEGIN capture");
            let presentation = compositor::FramePresentation {
                transformation: pose.transformation,
                opacity: pose.opacity,
                clip: pose.clip,
            };
            let presented = visual.timeline.submit(|| { compositor.present_frame(
                visual.frame.as_ref(),
                presentation,
                &mut visual.surface,
                viewport,
                || {
                    tracing::debug!(target: "iced_exwlshell::pacing", ?visual_id, elapsed_us = stamp.elapsed().as_micros(), "PACE_PRE capture");
                    ev.request_next_present(visual_id);
                    ev.request_presentation(visual_id, stamp);
                },
            ) });
            match presented {
                Ok(()) => {
                    tracing::debug!(target: "iced_exwlshell::pacing", ?visual_id, elapsed_us = stamp.elapsed().as_micros(), "PACE_END capture");
                    visual.pose = pose;
                    visual.finished = finished;
                    visual.pending = Some(popup_visuals::Submission {
                        stamp,
                        content: content_updated_at.unwrap_or(stamp),
                        pose,
                        placement,
                        epoch: window.interaction_epoch,
                        ime: visual.frame_ime.map(|(cursor, purpose)| {
                            (
                                (cursor * pose.transformation) * visual.application_scale,
                                purpose,
                            )
                        }),
                    });
                    ev.request_owner_wakeup(stamp + Duration::from_millis(100));
                    tracing::debug!(
                        ?iced_id,
                        ?visual_id,
                        ?stamp,
                        ?pose,
                        "popup visual submitted"
                    );
                    if !finished {
                        ev.request_refresh(visual_id, RefreshRequest::NextFrame);
                    }
                }
                Err(error) => {
                    if let Some(unit) = ev.get_unit_with_id(visual_id) {
                        Self::recover_frame_surface(
                            compositor,
                            &mut visual.surface,
                            unit.gen_wrapper(),
                            viewport,
                            &error,
                        );
                    }
                    ev.request_refresh(
                        visual_id,
                        RefreshRequest::At(Instant::now() + Duration::from_millis(16)),
                    );
                }
            }
            return;
        }

        let present_span = iced_debug::present(iced_id);
        let present_started = Instant::now();
        tracing::debug!(target: "iced_exwlshell::pacing", surface = ?layer_shell_id, ?iced_id, width = physical_size.width, height = physical_size.height, "PACE_BEGIN normal");
        tracing::debug!(
            ?iced_id,
            width = physical_size.width,
            height = physical_size.height,
            "presenting surface"
        );
        match compositor.present(
            &mut window.renderer.borrow_mut(),
            &mut window.surface,
            window.state.viewport(),
            window.state.background_color(),
            || {
                tracing::debug!(target: "iced_exwlshell::pacing", surface = ?layer_shell_id, ?iced_id, elapsed_us = present_started.elapsed().as_micros(), "PACE_PRE normal");
                ev.request_next_present(layer_shell_id);
                if let Some(updated_at) = content_updated_at {
                    ev.request_presentation(layer_shell_id, updated_at);
                }
            },
        ) {
            Ok(()) => {
                tracing::debug!(target: "iced_exwlshell::pacing", surface = ?layer_shell_id, ?iced_id, elapsed_us = present_started.elapsed().as_micros(), "PACE_END normal");
                present_span.finish();
            }
            Err(error) => {
                present_span.finish();
                match present_recovery(&error) {
                    PresentRecovery::Fatal => {
                        panic!("{error:?}");
                    }
                    PresentRecovery::Reconfigure => {
                        compositor.configure_surface(
                            &mut window.surface,
                            physical_size.width,
                            physical_size.height,
                        );
                        ev.request_refresh(layer_shell_id, RefreshRequest::NextFrame);
                    }
                    PresentRecovery::Recreate => {
                        let surface_window = Arc::new(
                            ev.get_unit_with_id(layer_shell_id)
                                .expect("the presented shell surface should still exist")
                                .gen_wrapper(),
                        );
                        window.surface = compositor.create_surface(
                            surface_window,
                            physical_size.width,
                            physical_size.height,
                        );
                        ev.request_refresh(layer_shell_id, RefreshRequest::NextFrame);
                    }
                    PresentRecovery::Report => {
                        tracing::error!("Error {error:?} when presenting surface.");
                    }
                }
            }
        }
    }

    fn recover_frame_surface(
        compositor: &mut C,
        surface: &mut C::Surface,
        wrapper: Arc<WindowWrapper>,
        viewport: &iced_graphics::Viewport,
        error: &compositor::SurfaceError,
    ) {
        let size = viewport.physical_size();
        tracing::debug!(target: "iced_exwlshell::pacing", surface = ?wrapper.id(), ?error, "PACE_RECOVERY");
        match present_recovery(error) {
            PresentRecovery::Recreate => {
                *surface = compositor.create_surface(wrapper, size.width, size.height);
            }
            PresentRecovery::Reconfigure => {
                compositor.configure_surface(surface, size.width, size.height)
            }
            PresentRecovery::Fatal => panic!("{error:?}"),
            PresentRecovery::Report => {}
        }
    }

    fn draw_retained_visual(&mut self, ev: &mut WindowState<IcedId>, id: LayerShellId) {
        let Some(visual) = self.visuals.get_mut(&id) else {
            return;
        };
        // No widget state changes while replaying an immutable frame. Frame
        // callbacks can pace its pose even while presentation proof is delayed.
        visual.pending = None;
        let Some(compositor) = &mut self.compositor else {
            return;
        };
        if visual.frame.is_none() {
            ev.request_close(id);
            return;
        }
        let (pose, finished) = visual.timeline.sample(Instant::now(), visual.bounds);
        assert!(pose.is_valid(), "invalid popup exit pose");
        let stamp = Instant::now();
        tracing::debug!(target: "iced_exwlshell::pacing", ?id, root = ?visual.placement.root, closing = visual.native.is_none(), scale = pose.transformation.scale_factor(), "PACE_BEGIN retained");
        let presentation = compositor::FramePresentation {
            transformation: pose.transformation,
            opacity: pose.opacity,
            clip: pose.clip,
        };
        if let Some(native) = visual.native {
            if ev.popup_visual_placement(native) == Some(visual.placement) {
                if let Some(unit) = ev.get_unit_with_id(native) {
                    let scale =
                        visual.viewport.scale_factor() / (visual.placement.scale as f32 / 120.0);
                    let region = pose.visible_bounds(visual.bounds);
                    let left = (region.x * scale).floor() as i32;
                    let top = (region.y * scale).floor() as i32;
                    let right = ((region.x + region.width) * scale).ceil() as i32;
                    let bottom = ((region.y + region.height) * scale).ceil() as i32;
                    if pose.accepts_input() {
                        unit.set_visual_input_region(&[(left, top, right - left, bottom - top)]);
                    } else {
                        unit.set_visual_input_region(&[]);
                    }
                    unit.get_wlsurface().commit();
                }
            }
        }
        let presented = visual.timeline.submit(|| { compositor.present_frame(
            visual.frame.as_ref(),
            presentation,
            &mut visual.surface,
            &visual.viewport,
            || {
                tracing::debug!(target: "iced_exwlshell::pacing", ?id, elapsed_us = stamp.elapsed().as_micros(), "PACE_PRE retained");
                ev.request_next_present(id);
                ev.request_presentation(id, stamp);
            },
        ) });
        match presented {
            Ok(()) => {
                tracing::debug!(target: "iced_exwlshell::pacing", ?id, elapsed_us = stamp.elapsed().as_micros(), "PACE_END retained");
                visual.pose = pose;
                visual.finished = finished;
                visual.pending = Some(popup_visuals::Submission {
                    stamp,
                    content: visual.frame_content.unwrap_or(stamp),
                    pose,
                    placement: visual.placement,
                    epoch: visual.frame_epoch,
                    ime: visual.frame_ime.map(|(cursor, purpose)| {
                        (
                            (cursor * pose.transformation) * visual.application_scale,
                            purpose,
                        )
                    }),
                });
                ev.request_owner_wakeup(stamp + Duration::from_millis(100));
                if finished && visual.native.is_none() {
                    tracing::debug!(
                        "POPUP_EXIT_FINISHED owner={:?} host={:?} scale={}",
                        visual.owner,
                        id,
                        pose.transformation.scale_factor()
                    );
                    // The exit has no interactive state left to authorize. Waiting
                    // for this terminal frame's presentation holds its pose for
                    // another feedback round trip before the host can disappear.
                    ev.request_close(id);
                    // Close requests are drained before refreshes, so a request
                    // made during this draw needs an immediate owner turn.
                    ev.request_owner_wakeup(Instant::now());
                }
                if !finished {
                    ev.request_refresh(id, RefreshRequest::NextFrame);
                }
            }
            Err(error) => {
                if let Some(unit) = ev.get_unit_with_id(id) {
                    Self::recover_frame_surface(
                        compositor,
                        &mut visual.surface,
                        unit.gen_wrapper(),
                        &visual.viewport,
                        &error,
                    );
                }
                ev.request_refresh(
                    id,
                    RefreshRequest::At(Instant::now() + Duration::from_millis(16)),
                );
            }
        }
    }

    fn handle_closed_event(
        &mut self,
        ev: &mut WindowState<IcedId>,
        layer_shell_id: Option<LayerShellId>,
    ) {
        if let Some(id) = layer_shell_id {
            if self.visuals.remove(&id).is_some() {
                return;
            }
            if let Some((visual_id, visual)) =
                self.visuals.iter_mut().find(|(_, v)| v.native == Some(id))
            {
                visual.close();
                ev.request_refresh(*visual_id, RefreshRequest::NextFrame);
                if let Some(deadline) = visual.retire_deadline {
                    ev.request_owner_wakeup(deadline);
                }
            }
        }
        let Some(iced_id) = layer_shell_id.and_then(|lid| {
            self.window_manager
                .get_alias(lid)
                .map(|(iced_id, _)| iced_id)
                .or_else(|| ev.get_unit_with_id(lid)?.get_binding().copied())
        }) else {
            return;
        };
        self.cached_layer_dimensions.remove(&iced_id);
        self.window_manager.remove(iced_id);
        self.user_interfaces.remove(&iced_id);
        self.iced_events.retain(|(id, _)| *id != iced_id);
        self.waiting_layer_shell_actions
            .retain(|(id, _)| *id != Some(iced_id));
        self.shell_broadcast.forget(iced_id);
        self.runtime
            .broadcast(iced_futures::subscription::Event::Interaction {
                window: iced_id,
                event: IcedEvent::Window(IcedWindowEvent::Closed),
                status: iced_core::event::Status::Ignored,
            });
        // if now there is no windows now, then break the compositor, and unlink the clipboard
        // unless opted-in to keep the compositor alive
        if self.window_manager.is_empty() && !self.keep_compositor_alive {
            self.remove_compositor();
        }
    }

    fn handle_window_event(
        &mut self,
        ev: &mut WindowState<IcedId>,
        layer_shell_id: Option<LayerShellId>,
        event: ExwlShellWindowEvent,
    ) {
        if let Some(id) = layer_shell_id {
            if let Some(visual) = self.visuals.get_mut(&id) {
                if let ExwlShellWindowEvent::PresentationDiscarded { content_updated_at } = &event {
                    if visual
                        .pending
                        .as_ref()
                        .is_some_and(|submission| submission.stamp == *content_updated_at)
                    {
                        visual.pending = None;
                        visual.confirmed = None;
                        ev.retry_visual_present(id);
                        ev.request_refresh(id, RefreshRequest::NextFrame);
                    }
                    return;
                }
                if let ExwlShellWindowEvent::Presented { content_updated_at } = event {
                    if visual
                        .pending
                        .as_ref()
                        .is_some_and(|submission| submission.stamp == content_updated_at)
                    {
                        visual.confirmed = visual.pending.take();
                        tracing::debug!(
                            "POPUP_PRESENTED owner={:?} host={:?} closing={} scale={} tx={} ty={} width={} height={} x={} y={} gx={} gy={} gw={} gh={}",
                            visual.owner,
                            id,
                            visual.native.is_none(),
                            visual.pose.transformation.scale_factor(),
                            visual.pose.transformation.translation().x,
                            visual.pose.transformation.translation().y,
                            visual.bounds.width,
                            visual.bounds.height,
                            visual.placement.origin.0,
                            visual.placement.origin.1,
                            visual.visible.x,
                            visual.visible.y,
                            visual.visible.width,
                            visual.visible.height
                        );
                        if let Some(submission) = &visual.confirmed {
                            if let Some(window) = self.window_manager.get_mut(visual.owner) {
                                window.popup_pose = Some(submission.pose);
                                if visual.native.is_some()
                                    && (window.interaction_epoch != submission.epoch
                                        || self.user_interfaces.content_updated_at(&visual.owner)
                                            != Some(submission.content))
                                {
                                    ev.request_refresh(window.id, RefreshRequest::NextFrame);
                                }
                                if ev.keyboard_focus_id() == visual.native
                                    && visual.native.is_some()
                                {
                                    let enabled = submission.ime.is_some();
                                    if ev.ime_allowed() != enabled {
                                        ev.set_ime_allowed(enabled);
                                    }
                                    if let Some((cursor, purpose)) = submission.ime {
                                        ev.set_ime_purpose(conversion::ime_purpose(purpose));
                                        ev.set_ime_cursor_area(
                                            exwlshellev::dpi::LogicalPosition::new(
                                                cursor.x, cursor.y,
                                            ),
                                            exwlshellev::dpi::LogicalSize {
                                                width: cursor.width,
                                                height: cursor.height,
                                            },
                                            window.id,
                                        );
                                    }
                                }
                            }
                            if visual.native.is_some() {
                                self.shell_broadcast.send(shell::ShellEvent::Presented {
                                    window: visual.owner,
                                    content_updated_at: submission.content,
                                });
                            }
                        }
                        if !visual.finished {
                            ev.request_refresh(id, RefreshRequest::NextFrame);
                        } else if let Some(native) = visual.native {
                            ev.request_refresh(native, RefreshRequest::Wait);
                        }
                    }
                }
                let owner = visual.owner;
                self.dispatch_visual_input(ev, owner);
                return;
            }
        }
        match &event {
            ExwlShellWindowEvent::OutputAdded(info) => {
                self.shell_broadcast
                    .send(shell::ShellEvent::OutputAdded(info.clone()));
                return;
            }
            ExwlShellWindowEvent::OutputUpdated(info) => {
                self.shell_broadcast
                    .send(shell::ShellEvent::OutputUpdated(info.clone()));
                return;
            }
            ExwlShellWindowEvent::OutputRemoved(info) => {
                self.shell_broadcast
                    .send(shell::ShellEvent::OutputRemoved(info.clone()));
                return;
            }
            ExwlShellWindowEvent::Locked => {
                self.shell_broadcast.send(shell::ShellEvent::Locked);
                return;
            }
            ExwlShellWindowEvent::LockDenied => {
                self.shell_broadcast.send(shell::ShellEvent::LockDenied);
                return;
            }
            ExwlShellWindowEvent::LockFinished => {
                self.shell_broadcast.send(shell::ShellEvent::LockedFinished);
                return;
            }
            _ => {}
        }
        let id_and_window = if let Some(layer_shell_id) = layer_shell_id {
            self.window_manager.get_mut_alias(layer_shell_id)
        } else {
            self.window_manager.iter_mut().next()
        };
        let Some((iced_id, window)) = id_and_window else {
            return;
        };
        if let ExwlShellWindowEvent::Presented { content_updated_at } = &event {
            self.shell_broadcast.send(shell::ShellEvent::Presented {
                window: iced_id,
                content_updated_at: *content_updated_at,
            });
            return;
        }
        if let ExwlShellWindowEvent::OutputChanged(output) = &event {
            self.shell_broadcast
                .send(shell::ShellEvent::WindowOutputChanged {
                    window: iced_id,
                    output: output.clone(),
                });
        }
        // In previous implementation, event without layer_shell_id won't call `update` here, but
        // will broadcast to the application. I'm not sure why, but I think it is
        // reasonable to call `update` here.
        window
            .state
            .update(&event, self.user_interfaces.application());
        if let Some(event) = conversion::window_event(
            &event,
            window.state.application_scale_factor(),
            window.state.modifiers(),
        ) {
            if self.popup_transitions.is_some() && window.popup_settings.is_some() {
                let native = window.id;
                let cursor = window.state.cursor();
                if let Some(visual) = self.visuals.values_mut().find(|v| v.native == Some(native)) {
                    let event = match event {
                        IcedEvent::Mouse(iced_core::mouse::Event::WheelScrolled { delta }) => {
                            let native_scale = window.state.wayland_scale_factor() as f32;
                            let pointer_scale = visual
                                .pointer_scale
                                .unwrap_or(window.state.application_scale_factor())
                                as f32;
                            let delta = match delta {
                                iced_core::mouse::ScrollDelta::Pixels { x, y } => {
                                    iced_core::mouse::ScrollDelta::Pixels {
                                        x: x / native_scale / pointer_scale,
                                        y: y / native_scale / pointer_scale,
                                    }
                                }
                                iced_core::mouse::ScrollDelta::Lines { x, y } => {
                                    iced_core::mouse::ScrollDelta::Lines {
                                        x: x / native_scale,
                                        y: y / native_scale,
                                    }
                                }
                            };
                            IcedEvent::Mouse(iced_core::mouse::Event::WheelScrolled { delta })
                        }
                        event => event,
                    };
                    if matches!(
                        event,
                        IcedEvent::Mouse(
                            iced_core::mouse::Event::CursorEntered
                                | iced_core::mouse::Event::CursorMoved { .. }
                        ) | IcedEvent::Touch(_)
                    ) {
                        visual.pointer_placement = Some(visual.placement);
                        visual.pointer_scale = Some(window.state.application_scale_factor());
                    }
                    let motion = matches!(
                        event,
                        IcedEvent::Mouse(iced_core::mouse::Event::CursorMoved { .. })
                    );
                    if motion
                        && visual.inputs.back().is_some_and(|input| {
                            matches!(
                                input.event,
                                IcedEvent::Mouse(iced_core::mouse::Event::CursorMoved { .. })
                            )
                        })
                    {
                        visual.inputs.pop_back();
                    }
                    if visual.inputs.len() >= 256 {
                        visual.inputs.clear();
                        visual.inputs.push_back(popup_visuals::Input {
                            event: IcedEvent::Window(IcedWindowEvent::Unfocused),
                            cursor: Cursor::Unavailable,
                            placement: None,
                            application_scale: window.state.application_scale_factor(),
                        });
                    } else {
                        // This is the last committed native buffer placement,
                        // not a position proposed by an unacked configure.
                        let spatial = matches!(event, IcedEvent::Mouse(_) | IcedEvent::Touch(_));
                        visual.inputs.push_back(popup_visuals::Input {
                            event,
                            cursor: if spatial { cursor } else { Cursor::Unavailable },
                            placement: if spatial {
                                visual.pointer_placement.or(Some(visual.placement))
                            } else {
                                Some(visual.placement)
                            },
                            application_scale: if spatial {
                                visual
                                    .pointer_scale
                                    .unwrap_or(window.state.application_scale_factor())
                            } else {
                                window.state.application_scale_factor()
                            },
                        });
                    }
                }
                self.dispatch_visual_input(ev, iced_id);
            } else {
                self.iced_events.push((iced_id, event));
            }
        }
    }

    fn dispatch_visual_input(&mut self, ev: &mut WindowState<IcedId>, iced_id: IcedId) {
        let Some(window) = self.window_manager.get_mut(iced_id) else {
            return;
        };
        let content = self.user_interfaces.content_updated_at(&iced_id);
        let Some(visual) = self
            .visuals
            .values_mut()
            .find(|v| v.native == Some(window.id))
        else {
            return;
        };
        if visual.inputs.is_empty() {
            return;
        }
        if !visual.ready(
            content,
            ev.popup_visual_placement(window.id),
            window.interaction_epoch,
        ) {
            if visual.pending.is_none() {
                ev.request_refresh(window.id, RefreshRequest::NextFrame);
            }
            return;
        }
        let input = visual.inputs.pop_front().expect("queued popup input");
        let pose = visual.confirmed.as_ref().expect("confirmed frame").pose;
        let mut matching = input.placement.is_some_and(|placement| {
            placement.root == visual.placement.root
                && placement.root_epoch == visual.placement.root_epoch
                && placement.scale == visual.placement.scale
        }) && pose.accepts_input();
        let scale = window.state.application_scale_factor() as f32;
        let source = input.placement.unwrap_or(visual.placement);
        let translation = crate::popup_animation::rebase_input(
            source.origin,
            visual.placement.origin,
            input.application_scale,
            scale as f64,
        );
        let adjusted = crate::popup_animation::Pose {
            transformation: translation.inverse(),
            ..pose
        };
        let mut event = crate::popup_animation::map_event(input.event.clone(), adjusted);
        match &event {
            IcedEvent::Touch(iced_core::touch::Event::FingerPressed { position, .. })
                if !pose.clip.contains(*position) =>
            {
                matching = false
            }
            IcedEvent::Touch(iced_core::touch::Event::FingerLifted { id, position })
                if !pose.clip.contains(*position) =>
            {
                event = IcedEvent::Touch(iced_core::touch::Event::FingerLost {
                    id: *id,
                    position: *position,
                })
            }
            IcedEvent::Keyboard(_) | IcedEvent::InputMethod(_) | IcedEvent::Clipboard(_) => {
                let mut focus = FocusVisibility {
                    pose,
                    hidden: false,
                };
                self.user_interfaces
                    .ui_mut(&iced_id)
                    .expect("popup UI")
                    .operate(&window.renderer.borrow(), &mut focus);
                matching &= !focus.hidden;
            }
            _ => {}
        }
        let events = if matching {
            vec![crate::popup_animation::map_event(event, pose)]
        } else {
            // Reconfiguration/overflow invalidates the sequence, including any
            // held capture. Never apply old local coordinates to a new geometry.
            vec![
                IcedEvent::Mouse(iced_core::mouse::Event::CursorLeft),
                IcedEvent::Window(IcedWindowEvent::Unfocused),
            ]
        };
        let raw_cursor = input.cursor * translation;
        let cursor = if matching
            && raw_cursor
                .position()
                .is_some_and(|position| pose.clip.contains(position))
        {
            raw_cursor * pose.transformation.inverse()
        } else {
            Cursor::Unavailable
        };
        tracing::debug!(?input.event,matching,?input.placement,?visual.placement,?cursor,"popup input dispatch");
        window.interaction_epoch = window.interaction_epoch.wrapping_add(1);
        let (state, statuses) = self
            .user_interfaces
            .ui_mut(&iced_id)
            .expect("popup UI")
            .update(
                window.raw.as_ref(),
                &window.waker,
                &events,
                cursor,
                &mut window.renderer.borrow_mut(),
                &mut self.messages,
            );
        let status = if matching {
            statuses
                .first()
                .copied()
                .unwrap_or(iced_core::event::Status::Ignored)
        } else {
            iced_core::event::Status::Ignored
        };
        visual.confirmed = None;
        let rebuild = Self::handle_ui_state(ev, window, state, false, true);
        if rebuild {
            if let Some(cache) = self.user_interfaces.remove(&iced_id) {
                Self::build_user_interface(
                    ev,
                    &mut self.user_interfaces,
                    iced_id,
                    cache,
                    window,
                    true,
                );
            }
        }
        ev.request_refresh(window.id, RefreshRequest::NextFrame);
        self.runtime
            .broadcast(iced_futures::subscription::Event::Interaction {
                window: iced_id,
                event: input.event,
                status,
            });
    }

    fn handle_user_action(&mut self, ev: &mut WindowState<IcedId>, action: Action<P::Message>) {
        if matches!(&action, Action::Widget(_) | Action::Reload) {
            for (_, window) in self.window_manager.iter_mut() {
                window.interaction_epoch = window.interaction_epoch.wrapping_add(1);
            }
            for visual in self.visuals.values_mut() {
                visual.confirmed = None;
            }
        }
        let mut should_exit = false;
        run_action(
            &mut self.user_interfaces,
            &mut self.compositor,
            action,
            &mut self.messages,
            &mut self.iced_events,
            &mut self.clipboard,
            &mut self.waiting_layer_shell_actions,
            &mut should_exit,
            &mut self.window_manager,
            &mut self.system_theme,
            &mut self.runtime,
            &mut self.renderer_settings,
            ev,
        );
        if should_exit {
            ev.append_return_data(ReturnData::RequestExit);
        }
    }

    fn handle_layer_shell_action(
        &mut self,
        ev: &mut WindowState<IcedId>,
        mut iced_id: Option<IcedId>,
        action: ExwlShellCustomAction,
    ) {
        let exshell_window;
        macro_rules! ref_mut_exshell_window {
            ($ev: ident, $iced_id: ident, $exshell_id: ident, $layer_shell_window: ident) => {
                if $iced_id.is_none() {
                    // Make application also works
                    if let Some(window) = self.window_manager.first() {
                        $iced_id = Some(window.iced_id);
                        $exshell_id = Some(window.id);
                    }
                    if $iced_id.is_none() {
                        tracing::error!(
                            "Here should be an id, it is a bug, please report an issue for us"
                        );
                        return;
                    }
                }
                if let Some(ls_window) =
                    $exshell_id.and_then(|exshell_id| $ev.get_mut_unit_with_id(exshell_id))
                {
                    exshell_window = ls_window;
                } else {
                    return;
                }
            };
        }
        // check if window is ready
        let mut ex_shell_id = iced_id
            .and_then(|iced_id| self.window_manager.get(iced_id))
            .map(|window| window.id);
        if iced_id.is_some() && ex_shell_id.is_none() {
            // still waiting
            self.waiting_layer_shell_actions.push((iced_id, action));
            return;
        }
        match action {
            ExwlShellCustomAction::BlurOptionChange(blur_option) => {
                ref_mut_exshell_window!(ev, iced_id, ex_shell_id, layer_shell_window);
                exshell_window.set_blur_option(blur_option);
            }
            ExwlShellCustomAction::LayoutChange { anchor, size } => {
                ref_mut_exshell_window!(ev, iced_id, ex_shell_id, layer_shell_window);
                exshell_window.set_layout(anchor, size);
            }
            ExwlShellCustomAction::LayerChange(layer) => {
                ref_mut_exshell_window!(ev, iced_id, ex_shell_id, layer_shell_window);
                exshell_window.set_layer(layer);
            }
            ExwlShellCustomAction::MarginChange(margin) => {
                ref_mut_exshell_window!(ev, iced_id, ex_shell_id, layer_shell_window);
                exshell_window.set_margin(margin);
            }
            ExwlShellCustomAction::ExclusiveZoneChange(zone_size) => {
                ref_mut_exshell_window!(ev, iced_id, ex_shell_id, layer_shell_window);
                exshell_window.set_exclusive_zone(zone_size);
            }
            ExwlShellCustomAction::KeyboardInteractivityChange(keyboard_interactivity) => {
                ref_mut_exshell_window!(ev, iced_id, ex_shell_id, layer_shell_window);
                exshell_window.set_keyboard_interactivity(keyboard_interactivity);
            }
            ExwlShellCustomAction::SetInputRegion(set_region) => {
                ref_mut_exshell_window!(ev, iced_id, ex_shell_id, layer_shell_window);
                let set_region = set_region.0;
                let Some(region) = &self.wl_input_region else {
                    tracing::warn!(
                        "wl_input_region is not set, ignore SetInputRegion, window_id: {:?}",
                        iced_id
                    );
                    return;
                };

                let window_size = exshell_window.get_size();
                let width: i32 = window_size.0.try_into().unwrap_or_default();
                let height: i32 = window_size.1.try_into().unwrap_or_default();

                region.subtract(0, 0, width, height);
                set_region(region);

                exshell_window
                    .get_wlsurface()
                    .set_input_region(self.wl_input_region.as_ref());
                exshell_window.get_wlsurface().commit();
            }
            ExwlShellCustomAction::VirtualKeyboardPressed { key } => {
                use exwlshellev::reexport::wayland_client::KeyState;
                let ky = ev.get_virtual_keyboard().unwrap();
                let current_time = Instant::now();
                let time_stamp = (current_time - self.time).as_millis() as u32;
                ky.key(time_stamp, key, KeyState::Pressed.into());
                // NOTE: add delay time
                let time = time_stamp + 100;
                ev.set_virtual_key_release(exwlshellev::VirtualKeyRelease {
                    delay: Duration::from_micros(100),
                    time,
                    key,
                });
            }
            ExwlShellCustomAction::NewLayerShell {
                settings,
                id: iced_id,
                ..
            } => {
                let layer_shell_id = exwlshellev::id::Id::unique();
                ev.append_return_data(ReturnData::NewLayerShell((
                    settings,
                    layer_shell_id,
                    Some(iced_id),
                )));
            }
            ExwlShellCustomAction::Lock => {
                ev.append_return_data(ReturnData::RequestLock);
            }
            ExwlShellCustomAction::UnLock => {
                ev.append_return_data(ReturnData::RequestUnLock);
            }
            ExwlShellCustomAction::NewBaseWindow {
                settings,
                id: iced_id,
                ..
            } => {
                let layer_shell_id = exwlshellev::id::Id::unique();
                ev.append_return_data(ReturnData::NewXdgBase((
                    settings.into(),
                    layer_shell_id,
                    Some(iced_id),
                )));
            }
            ExwlShellCustomAction::RemoveWindow => {
                if let Some(layer_shell_id) = ex_shell_id {
                    ev.request_close(layer_shell_id)
                }
            }
            ExwlShellCustomAction::NewPopUp {
                settings,
                id: iced_id,
            } => {
                let parent_layer_id = match settings.parent {
                    Some(parent) => self.window_manager.get(parent).map(|w| w.id),
                    None => ev.popup_parent_id(),
                };
                let Some(parent_layer_id) = parent_layer_id else {
                    return;
                };
                if let Some((parent_id, parent)) = self.window_manager.get_alias(parent_layer_id) {
                    if let Some(visual) = self
                        .visuals
                        .values()
                        .find(|v| v.native == Some(parent_layer_id))
                    {
                        if !visual.ready(
                            self.user_interfaces.content_updated_at(&parent_id),
                            ev.popup_visual_placement(parent_layer_id),
                            parent.interaction_epoch,
                        ) {
                            ev.request_refresh(parent_layer_id, RefreshRequest::NextFrame);
                            let settings = IcedNewPopupSettings {
                                parent: Some(parent_id),
                                ..settings
                            };
                            self.waiting_layer_shell_actions.push((
                                None,
                                ExwlShellCustomAction::NewPopUp {
                                    settings,
                                    id: iced_id,
                                },
                            ));
                            return;
                        }
                    }
                }
                let parent_scale = match settings.parent {
                    Some(parent_id) => self
                        .window_manager
                        .get(parent_id)
                        .map(|w| w.state.application_scale_factor()),
                    None => self
                        .window_manager
                        .get_alias(parent_layer_id)
                        .map(|(_, w)| w.state.application_scale_factor()),
                }
                .unwrap_or(1.0);

                let mut settings = settings;
                if self.popup_transitions.is_some() {
                    if let Some((_, parent)) = self.window_manager.get_alias(parent_layer_id) {
                        if let Some(pose) = parent.popup_pose {
                            let geometry = parent.popup_geometry.unwrap_or_default();
                            let map = |bounds: iced_core::Rectangle| {
                                let mut bounds = bounds * pose.transformation;
                                bounds.x -= geometry.x;
                                bounds.y -= geometry.y;
                                bounds
                            };
                            settings.placement = match settings.placement {
                                PopupPlacement::Anchored {
                                    position: (x, y),
                                    size,
                                } => {
                                    let (w, h) = size.to_set();
                                    let rect = map(iced_core::Rectangle {
                                        x: x as f32,
                                        y: y as f32,
                                        width: w as f32,
                                        height: h as f32,
                                    });
                                    PopupPlacement::Anchored {
                                        position: (rect.x.round() as i32, rect.y.round() as i32),
                                        size: PixelSize::px(
                                            rect.width.round().max(1.0) as u32,
                                            rect.height.round().max(1.0) as u32,
                                        ),
                                    }
                                }
                                PopupPlacement::Position((x, y)) => {
                                    let rect = map(iced_core::Rectangle {
                                        x: x as f32,
                                        y: y as f32,
                                        width: 1.0,
                                        height: 1.0,
                                    });
                                    PopupPlacement::Position((
                                        rect.x.round() as i32,
                                        rect.y.round() as i32,
                                    ))
                                }
                            };
                        }
                    }
                }
                let scaled_settings = settings.scale(parent_scale);
                self.pending_popup_settings.insert(iced_id, scaled_settings);

                let IcedNewPopupSettings {
                    size,
                    placement,
                    anchor,
                    gravity,
                    constraint_adjustment,
                    ..
                } = scaled_settings;

                let grab_serial = ev.take_popup_grab_serial();
                let popup_settings = NewPopUpSettings {
                    size,
                    id: parent_layer_id,
                    placement,
                    anchor,
                    gravity,
                    constraint_adjustment,
                    grab_serial,
                };
                let layer_shell_id = exwlshellev::id::Id::unique();
                ev.append_return_data(ReturnData::NewPopUp((
                    popup_settings,
                    layer_shell_id,
                    Some(iced_id),
                )));
            }
            ExwlShellCustomAction::PopUpReposition { settings } => {
                let Some(ex_shell_id) = ex_shell_id else {
                    return;
                };
                let parent_scale = match settings.parent {
                    Some(parent_id) => self
                        .window_manager
                        .get(parent_id)
                        .map(|w| w.state.application_scale_factor()),
                    None => self
                        .window_manager
                        .get_alias(ex_shell_id)
                        .map(|(_, w)| w.state.application_scale_factor()),
                }
                .unwrap_or(1.0);

                let scaled_settings = settings.scale(parent_scale);
                if let Some(iced_id) = iced_id
                    && let Some(window) = self.window_manager.get_mut(iced_id)
                    && window.popup_settings.is_some()
                {
                    window.popup_settings = Some(scaled_settings);
                }
                let IcedNewPopupSettings {
                    size,
                    placement,
                    anchor,
                    gravity,
                    constraint_adjustment,
                    ..
                } = scaled_settings;
                ev.append_return_data(ReturnData::PopUpReposition((
                    PopUpRepositionSettings {
                        size,
                        placement,
                        anchor,
                        gravity,
                        constraint_adjustment,
                    },
                    ex_shell_id,
                )));
            }
            ExwlShellCustomAction::NewMenu {
                settings: menu_setting,
                id: iced_id,
            } => {
                let Some(parent_layer_shell_id) =
                    ev.pointer_surface_id().or_else(|| ev.popup_parent_id())
                else {
                    return;
                };
                let Some((parent_iced_id, window)) =
                    self.window_manager.get_alias(parent_layer_shell_id)
                else {
                    return;
                };
                let Some(point) = window.state.mouse_position().copied() else {
                    return;
                };
                let scale = window.state.application_scale_factor();
                let point = if self.popup_transitions.is_some() {
                    let mut point = point;
                    if let Some(visual) = self
                        .visuals
                        .values()
                        .find(|v| v.native == Some(parent_layer_shell_id))
                    {
                        let source = visual.pointer_placement.unwrap_or(visual.placement);
                        point = point
                            * crate::popup_animation::rebase_input(
                                source.origin,
                                visual.placement.origin,
                                visual.pointer_scale.unwrap_or(scale),
                                scale,
                            );
                    }
                    if let Some(geometry) = window.popup_geometry {
                        point.x -= geometry.x;
                        point.y -= geometry.y;
                    }
                    point
                } else {
                    point
                };
                let (x, y) = (
                    (point.x as f64 * scale).round() as i32,
                    (point.y as f64 * scale).round() as i32,
                );
                let (mw, mh) = menu_setting.size.to_set();
                let size = PixelSize::px(
                    (f64::from(mw) * scale).round() as u32,
                    (f64::from(mh) * scale).round() as u32,
                );
                let popup_settings = NewPopUpSettings {
                    size,
                    id: parent_layer_shell_id,
                    placement: PopupPlacement::Position((x, y)),
                    anchor: PopupAnchor::TopLeft,
                    gravity: menu_setting.gravity,
                    constraint_adjustment: PopupConstraintAdjustment::FlipX
                        | PopupConstraintAdjustment::FlipY
                        | PopupConstraintAdjustment::SlideX
                        | PopupConstraintAdjustment::SlideY,
                    grab_serial: None,
                };
                if self.popup_transitions.is_some() {
                    self.pending_popup_settings.insert(
                        iced_id,
                        IcedNewPopupSettings {
                            parent: Some(parent_iced_id),
                            size,
                            placement: popup_settings.placement,
                            anchor: popup_settings.anchor,
                            gravity: popup_settings.gravity,
                            constraint_adjustment: popup_settings.constraint_adjustment,
                        },
                    );
                }
                let layer_shell_id = exwlshellev::id::Id::unique();
                ev.append_return_data(ReturnData::NewPopUp((
                    popup_settings,
                    layer_shell_id,
                    Some(iced_id),
                )));
            }
            ExwlShellCustomAction::NewInputPanel {
                settings,
                id: iced_id,
            } => {
                let layer_shell_id = exwlshellev::id::Id::unique();
                ev.append_return_data(ReturnData::NewInputPanel((
                    settings,
                    layer_shell_id,
                    Some(iced_id),
                )));
            }
            ExwlShellCustomAction::ForgetLastOutput => {
                ev.forget_last_output();
            }
        }
    }

    fn handle_normal_dispatch(&mut self, ev: &mut WindowState<IcedId>) {
        let mut popup_events = Vec::new();
        self.iced_events.retain(|(owner, event)| {
            if self
                .visuals
                .values()
                .any(|v| v.native.is_some() && v.owner == *owner)
            {
                popup_events.push((*owner, event.clone()));
                false
            } else {
                true
            }
        });
        for (owner, event) in popup_events {
            if let Some(window) = self.window_manager.get(owner) {
                if let Some(visual) = self
                    .visuals
                    .values_mut()
                    .find(|v| v.native == Some(window.id))
                {
                    visual.inputs.push_back(popup_visuals::Input {
                        event,
                        cursor: window.state.cursor(),
                        placement: visual.pointer_placement.or(Some(visual.placement)),
                        application_scale: visual
                            .pointer_scale
                            .unwrap_or(window.state.application_scale_factor()),
                    });
                }
            }
        }
        let owners = self
            .visuals
            .values()
            .filter(|v| v.native.is_some() && !v.inputs.is_empty())
            .map(|v| v.owner)
            .collect::<Vec<_>>();
        for owner in owners {
            self.dispatch_visual_input(ev, owner);
        }
        let now = Instant::now();
        for (id, visual) in &mut self.visuals {
            if visual.retire_deadline.is_some_and(|at| at <= now) {
                // Output/feedback loss is not permission to retain resources
                // forever. Native input has already been released.
                ev.request_close(*id);
                continue;
            }
            if visual
                .pending
                .as_ref()
                .is_some_and(|submission| submission.stamp.elapsed() >= Duration::from_millis(100))
            {
                tracing::debug!(?id, "popup presentation timeout; retrying visual slot");
                visual.pending = None;
                visual.confirmed = None;
                ev.retry_visual_present(*id);
                ev.request_refresh(*id, RefreshRequest::NextFrame);
            }
            if let Some(submission) = &visual.pending {
                ev.request_owner_wakeup(submission.stamp + Duration::from_millis(100));
            }
            if let Some(deadline) = visual.retire_deadline {
                ev.request_owner_wakeup(deadline);
            }
        }
        // Do not replace a UI borrowed from the application while its submitted
        // visual is still awaiting presentation. Native events are dispatched
        // individually above, using their own cursor and presented generation.
        if self.popup_transitions.is_some()
            && self.visuals.values().any(|v| {
                v.native.is_some()
                    && v.pending.as_ref().is_some_and(|submission| {
                        submission.stamp.elapsed() < Duration::from_millis(100)
                    })
            })
        {
            return;
        }
        if self.iced_events.is_empty() && self.messages.is_empty() {
            return;
        }

        let mut rebuilds = Vec::new();
        for (iced_id, window) in self.window_manager.iter_mut() {
            if self.popup_transitions.is_some() && window.popup_settings.is_some() {
                continue;
            }
            let interact_span = iced_debug::interact(iced_id);
            let mut window_events = vec![];

            self.iced_events.retain(|(window_id, event)| {
                if *window_id == iced_id {
                    window_events.push(event.clone());
                    false
                } else {
                    true
                }
            });

            if window_events.is_empty() && self.messages.is_empty() {
                continue;
            }

            let (ui_state, statuses) = self
                .user_interfaces
                .ui_mut(&iced_id)
                .expect("Get user interface")
                .update(
                    window.raw.as_ref(),
                    &window.waker,
                    &window_events,
                    window.state.cursor(),
                    &mut window.renderer.borrow_mut(),
                    &mut self.messages,
                );
            run_clipboard(
                &mut self.clipboard,
                &ui_state,
                iced_id,
                &mut self.iced_events,
            );

            #[cfg(feature = "unconditional-rendering")]
            let unconditional_rendering = true;
            #[cfg(not(feature = "unconditional-rendering"))]
            let unconditional_rendering = false;
            if Self::handle_ui_state(ev, window, ui_state, unconditional_rendering, false) {
                rebuilds.push((iced_id, window));
            }

            for (event, status) in window_events.drain(..).zip(statuses) {
                self.runtime
                    .broadcast(iced_futures::subscription::Event::Interaction {
                        window: iced_id,
                        event,
                        status,
                    });
            }
            interact_span.finish();
        }

        if !self.messages.is_empty() {
            match self.redraw_policy.targets(&self.messages) {
                Targets::All => {
                    ev.request_refresh_all(RefreshRequest::NextFrame);
                }
                Targets::None => {}
                Targets::Window(id) => {
                    if let Some(window) = self.window_manager.get(id) {
                        ev.request_refresh(window.id, RefreshRequest::NextFrame);
                    }
                }
                Targets::Windows(windows) => {
                    windows.into_iter().for_each(|id| {
                        if let Some(window) = self.window_manager.get(id) {
                            ev.request_refresh(window.id, RefreshRequest::NextFrame);
                        }
                    });
                }
            }
            let (caches, application) = self.user_interfaces.extract_all();

            // Update application
            update(
                application,
                &mut self.runtime,
                &mut self.messages,
                &mut self.waiting_layer_shell_actions,
            );

            for (_, window) in self.window_manager.iter_mut() {
                window.state.synchronize(application);
            }
            iced_debug::theme_changed(|| {
                self.window_manager
                    .first()
                    .and_then(|window| <P::Theme as theme::Base>::seed(window.state.theme()))
            });

            for (iced_id, cache) in caches {
                let Some(window) = self.window_manager.get_mut(iced_id) else {
                    continue;
                };
                let measure_popup = window.popup_settings.is_some();
                Self::build_user_interface(
                    ev,
                    &mut self.user_interfaces,
                    iced_id,
                    cache,
                    window,
                    measure_popup,
                );
            }
        } else {
            for (iced_id, window) in rebuilds {
                if let Some(cache) = self.user_interfaces.remove(&iced_id) {
                    self.user_interfaces.build(
                        iced_id,
                        cache,
                        &mut window.renderer.borrow_mut(),
                        window.state.viewport().logical_size(),
                    );
                }
            }
        }
    }

    fn handle_ui_state(
        ev: &mut WindowState<IcedId>,
        window: &mut Window<P, C>,
        ui_state: user_interface::State,
        unconditional_rendering: bool,
        update_ime: bool,
    ) -> bool {
        match ui_state {
            user_interface::State::Outdated => true,
            user_interface::State::Updated {
                redraw_request,
                input_method,
                mouse_interaction,
                ..
            } => {
                if unconditional_rendering {
                    ev.request_refresh(window.id, RefreshRequest::NextFrame);
                } else {
                    match redraw_request {
                        RedrawRequest::NextFrame => {
                            ev.request_refresh(window.id, RefreshRequest::NextFrame)
                        }
                        RedrawRequest::At(instant) => {
                            ev.request_refresh(window.id, RefreshRequest::At(instant))
                        }
                        RedrawRequest::Wait => {}
                    }
                }

                if update_ime {
                    let ime_flags = window.request_input_method(input_method.clone());
                    if !window.visual_popup {
                        match input_method {
                            iced_core::InputMethod::Disabled => {
                                if ime_flags.contains(ImeState::Disabled) {
                                    ev.set_ime_allowed(false);
                                }
                            }
                            iced_core::InputMethod::Enabled {
                                purpose,
                                preedit: _,
                                cursor,
                            } => {
                                if ime_flags.contains(ImeState::Allowed) {
                                    ev.set_ime_allowed(true);
                                }

                                if ime_flags.contains(ImeState::Update) {
                                    ev.set_ime_purpose(conversion::ime_purpose(purpose));
                                    let cursor = if window.popup_pose.is_some() {
                                        window.presented_ime_cursor().unwrap_or(cursor)
                                    } else {
                                        cursor
                                    };
                                    ev.set_ime_cursor_area(
                                        exwlshellev::dpi::LogicalPosition::new(cursor.x, cursor.y),
                                        exwlshellev::dpi::LogicalSize {
                                            width: cursor.width,
                                            height: cursor.height,
                                        },
                                        window.id,
                                    );
                                }
                            }
                        }
                    }
                }

                if mouse_interaction != window.mouse_interaction {
                    // Only the window that contains the pointer can change cursor
                    if ev.pointer_surface_id() == Some(window.id) {
                        for pointer in ev.get_pointers() {
                            ev.append_return_data(ReturnData::RequestSetCursorShape((
                                conversion::mouse_interaction(mouse_interaction),
                                pointer,
                            )));
                        }
                    }
                    window.mouse_interaction = mouse_interaction;
                }
                false
            }
        }
    }
}

fn run_clipboard(
    clipboard: &mut ExwlShellClipboard,
    state: &user_interface::State,
    window: IcedId,
    events: &mut Vec<(IcedId, IcedEvent)>,
) {
    let user_interface::State::Updated {
        clipboard: requests,
        ..
    } = state
    else {
        return;
    };

    for kind in &requests.reads {
        events.push((
            window,
            IcedEvent::Clipboard(iced_core::clipboard::Event::Read(
                clipboard.read(*kind).map(Arc::new),
            )),
        ));
    }

    if let Some(content) = requests.write.clone() {
        events.push((
            window,
            IcedEvent::Clipboard(iced_core::clipboard::Event::Written(
                clipboard.write(content),
            )),
        ));
    }
}

pub(crate) fn update<P: IcedProgram, E: Executor>(
    application: &mut Instance<P>,
    runtime: &mut MultiRuntime<E, P::Message>,
    messages: &mut Vec<P::Message>,
    waiting_layer_shell_actions: &mut Vec<(Option<iced_core::window::Id>, ExwlShellCustomAction)>,
) where
    P::Theme: DefaultStyle,
    P::Message: 'static + TryInto<ExwlShellCustomActionWithId, Error = P::Message>,
{
    for message in messages.drain(..) {
        // NOTE: avoid something like
        // match message {
        // Message::Unlock => Task::done(message)
        // }
        // Now if it is a unique command, it will be operated immediately
        let message = match message.try_into() {
            Ok(action) => {
                let ExwlShellCustomActionWithId(id, action) = action;
                waiting_layer_shell_actions.push((id, action));
                continue;
            }
            Err(message) => message,
        };
        let task = runtime.enter(|| application.update(message));

        if let Some(stream) = iced_runtime::task::into_stream(task) {
            runtime.run(stream);
        }
    }

    let subscription = runtime.enter(|| application.subscription());
    let recipes = iced_futures::subscription::into_recipes(subscription.map(Action::Output));

    iced_debug::subscriptions_tracked(recipes.len());
    runtime.track(recipes);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_action<P, C, E: Executor>(
    user_interfaces: &mut UserInterfaces<P>,
    compositor: &mut Option<C>,
    event: Action<P::Message>,
    messages: &mut Vec<P::Message>,
    iced_events: &mut Vec<(IcedId, IcedEvent)>,
    clipboard: &mut ExwlShellClipboard,
    waiting_layer_shell_actions: &mut Vec<(Option<iced_core::window::Id>, ExwlShellCustomAction)>,
    should_exit: &mut bool,
    window_manager: &mut WindowManager<P, C>,
    system_theme: &mut iced_core::theme::Mode,
    runtime: &mut MultiRuntime<E, P::Message>,
    renderer_settings: &mut iced_core::renderer::Settings,
    ev: &mut WindowState<IcedId>,
) where
    P: IcedProgram + 'static,
    C: Compositor<Renderer = P::Renderer> + 'static,
    P::Theme: DefaultStyle,
    P::Message: 'static + TryInto<ExwlShellCustomActionWithId, Error = P::Message>,
{
    use iced_core::widget::operation;
    use iced_runtime::Action;
    use iced_runtime::clipboard;
    use iced_runtime::{backend, font};

    use iced_runtime::window::Action as WindowAction;
    match event {
        Action::Output(stream) => match stream.try_into() {
            Ok(action) => {
                let ExwlShellCustomActionWithId(id, action) = action;
                waiting_layer_shell_actions.push((id, action));
            }
            Err(stream) => {
                messages.push(stream);
            }
        },
        Action::Image(action) => match action {
            iced_runtime::image::Action::Allocate(handle, sender) => {
                use iced_core::Renderer as _;

                // TODO: Shared image cache in compositor
                if let Some((_id, window)) = window_manager.iter_mut().next() {
                    window
                        .renderer
                        .borrow_mut()
                        .allocate_image(&handle, move |allocation| {
                            let _ = sender.send(allocation);
                        });
                }
            }
        },
        Action::Font(action) => match action {
            font::Action::Load { bytes, channel } => {
                if let Some(compositor) = compositor {
                    let _ = channel.send(compositor.load_font(bytes));
                }
            }
            font::Action::List { channel } => {
                if let Some(compositor) = compositor {
                    let _ = channel.send(compositor.list_fonts());
                }
            }
            font::Action::SetDefaults { font, text_size } => {
                renderer_settings.font = font;
                renderer_settings.text_size = text_size;

                if let Some(compositor) = compositor {
                    // With shared renderer, create the new renderer once
                    if let Some((_, first_window)) = window_manager.iter_mut().next() {
                        *first_window.renderer.borrow_mut() =
                            compositor.create_renderer(*renderer_settings);
                    }
                    for (id, window) in window_manager.iter_mut() {
                        if let Some(cache) = user_interfaces.remove(&id) {
                            user_interfaces.build(
                                id,
                                cache,
                                &mut window.renderer.borrow_mut(),
                                window.state.viewport().logical_size(),
                            );
                        }
                    }
                }
            }
        },
        Action::Backend(backend::Action::Configure(_, channel)) => {
            let _ = channel.send(Err(iced_core::backend::Error::BackendError(
                "runtime backend reconfiguration is not supported".into(),
            )));
        }
        Action::Event { window, event } => iced_events.push((window, event)),
        Action::Tick => {
            use iced_core::Renderer as _;

            for (_, window) in window_manager.iter_mut() {
                window.renderer.borrow_mut().tick();
            }
        }
        Action::Clipboard(action) => match action {
            clipboard::Action::Read { kind, channel } => {
                let _ = channel.send(clipboard.read(kind));
            }
            clipboard::Action::Write { content, channel } => {
                let _ = channel.send(clipboard.write(content));
            }
        },
        Action::Widget(action) => {
            let mut current_operation = Some(action);

            while let Some(mut operation) = current_operation.take() {
                // kind of suboptimal that we have to iterate over all windows, but since an operation does not have
                // a window id associated with it, this is the best we can do for now
                for (id, window) in window_manager.iter_mut() {
                    if let Some(mut ui) = user_interfaces.ui_mut(&id) {
                        ui.operate(&window.renderer.borrow(), operation.as_mut());
                    }
                }

                match operation.finish() {
                    operation::Outcome::None => {}
                    operation::Outcome::Some(()) => {}
                    operation::Outcome::Chain(next) => {
                        current_operation = Some(next);
                    }
                }
            }
            ev.request_refresh_all(RefreshRequest::NextFrame);
        }
        Action::Window(action) => match action {
            WindowAction::Close(id) => {
                waiting_layer_shell_actions.push((Some(id), ExwlShellCustomAction::RemoveWindow));
            }
            WindowAction::GetOldest(channel) => {
                let _ = channel.send(window_manager.first_window().map(|(id, _)| *id));
            }
            WindowAction::GetLatest(channel) => {
                let _ = channel.send(window_manager.last_window().map(|(id, _)| *id));
            }
            WindowAction::GetSize(id, channel) => 'out: {
                let Some(window) = window_manager.get(id) else {
                    break 'out;
                };
                let _ = channel.send(window.state.window_size_f32());
            }
            WindowAction::Screenshot(id, channel) => 'out: {
                let Some(window) = window_manager.get_mut(id) else {
                    break 'out;
                };
                let Some(compositor) = compositor else {
                    break 'out;
                };
                let bytes = compositor.screenshot(
                    &mut window.renderer.borrow_mut(),
                    window.state.viewport(),
                    window.state.background_color(),
                );

                let _ = channel.send(iced_core::window::Screenshot::new(
                    bytes,
                    window.state.viewport().physical_size(),
                    window.state.viewport().scale_factor(),
                ));
            }
            WindowAction::GetScaleFactor(id, channel) => {
                if let Some(window) = window_manager.get_mut(id) {
                    let _ = channel.send(window.state.wayland_scale_factor() as f32);
                };
            }
            _ => {}
        },
        Action::System(action) => match action {
            iced_runtime::system::Action::GetTheme(channel) => {
                let _ = channel.send(*system_theme);
            }
            iced_runtime::system::Action::NotifyTheme(mode) => {
                if mode != *system_theme {
                    *system_theme = mode;

                    runtime.broadcast(iced_futures::subscription::Event::SystemThemeChanged(mode));
                }

                for (_id, window) in window_manager.iter_mut() {
                    window.state.update(
                        &ExwlShellWindowEvent::ThemeChanged(mode),
                        user_interfaces.application(),
                    );
                }

                ev.request_refresh_all(RefreshRequest::NextFrame);
            }

            _ => {}
        },
        Action::Exit => {
            *should_exit = true;
        }
        Action::Reload => {
            for (iced_id, window) in window_manager.iter_mut() {
                if let Some(cache) = user_interfaces.remove(&iced_id) {
                    user_interfaces.build(
                        iced_id,
                        cache,
                        &mut window.renderer.borrow_mut(),
                        window.state.viewport().logical_size(),
                    );
                }
            }
            ev.request_refresh_all(RefreshRequest::NextFrame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::scaled_popup_size;
    use iced_core::Size;

    #[test]
    fn popup_geometry_marker_excludes_margins_and_scales_once() {
        use iced_core::{
            Rectangle,
            widget::{Id, Operation},
        };
        let bounds = Rectangle {
            x: 128.0,
            y: 128.0,
            width: 444.0,
            height: 307.25,
        };
        let mut operation = super::PopupWindowGeometry::default();
        operation.container(Some(&Id::new("unrelated")), bounds);
        assert_eq!(operation.0, None);
        operation.container(
            Some(&Id::new(crate::actions::POPUP_WINDOW_GEOMETRY_ID)),
            bounds,
        );
        assert_eq!(operation.0, Some(bounds));
        assert_eq!(
            super::scaled_popup_geometry(bounds, 1.25),
            (160, 160, 555, 385)
        );
    }

    #[test]
    fn popup_size_is_scaled_and_rounded_up() {
        assert_eq!(
            scaled_popup_size(Size::new(320.0, 241.25), 1.25),
            Some(Size::new(400, 302))
        );
    }

    #[test]
    fn popup_size_rejects_empty_dimensions() {
        assert_eq!(scaled_popup_size(Size::new(320.0, 0.0), 1.0), None);
        assert_eq!(scaled_popup_size(Size::new(0.0, 240.0), 1.0), None);
    }
}
