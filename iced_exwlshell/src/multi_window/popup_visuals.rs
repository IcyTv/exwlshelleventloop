use crate::{
    actions::IcedNewPopupSettings,
    popup_animation::{Pose, Profile, Timeline},
};
use exwlshellev::{id::Id, visual::Placement};
use iced_core::{Rectangle, window};
use iced_graphics::{Compositor, Viewport, compositor::Frame};
use std::collections::VecDeque;
use std::time::Instant;

pub(super) struct Input {
    pub event: iced_core::Event,
    pub cursor: iced_core::mouse::Cursor,
    pub placement: Option<Placement>,
    pub application_scale: f64,
}

pub(super) struct Submission {
    pub stamp: Instant,
    pub content: Instant,
    pub pose: Pose,
    pub placement: Placement,
    pub epoch: u64,
    pub ime: Option<(Rectangle, iced_core::input_method::Purpose)>,
}

pub(super) struct Visual<C: Compositor> {
    pub native: Option<Id>,
    pub owner: window::Id,
    pub settings: IcedNewPopupSettings,
    pub surface: C::Surface,
    pub frame: Option<Frame>,
    pub frame_content: Option<Instant>,
    pub frame_epoch: u64,
    pub frame_ime: Option<(Rectangle, iced_core::input_method::Purpose)>,
    pub application_scale: f32,
    pub viewport: Viewport,
    pub placement: Placement,
    pub bounds: Rectangle,
    pub visible: Rectangle,
    pub timeline: Timeline,
    pub pose: Pose,
    pub pending: Option<Submission>,
    /// A child redraw consumed while waiting for the previous frame's proof.
    pub deferred_refresh: bool,
    pub confirmed: Option<Submission>,
    pub native_capacity: Option<(u32, u32, u32)>,
    pub finished: bool,
    pub inputs: VecDeque<Input>,
    pub pointer_placement: Option<Placement>,
    pub pointer_scale: Option<f64>,
    pub retire_deadline: Option<Instant>,
}
impl<C: Compositor> Visual<C> {
    pub fn new(
        native: Id,
        owner: window::Id,
        settings: IcedNewPopupSettings,
        surface: C::Surface,
        viewport: Viewport,
        placement: Placement,
        profile: Profile,
    ) -> Self {
        let bounds = Rectangle::with_size(viewport.logical_size());
        let timeline = Timeline::new(profile, bounds);
        let pose = timeline.from;
        Self {
            native: Some(native),
            owner,
            settings,
            surface,
            frame: None,
            frame_content: None,
            frame_epoch: 0,
            frame_ime: None,
            application_scale: 1.0,
            viewport,
            placement,
            bounds,
            visible: bounds,
            timeline,
            pose,
            pending: None,
            deferred_refresh: false,
            confirmed: None,
            native_capacity: None,
            finished: false,
            inputs: VecDeque::new(),
            pointer_placement: None,
            pointer_scale: None,
            retire_deadline: None,
        }
    }
    pub fn ready(
        &self,
        content: Option<Instant>,
        placement: Option<Placement>,
        epoch: u64,
    ) -> bool {
        self.native.is_some()
            && self.pending.is_none()
            && self.confirmed.as_ref().is_some_and(|submission| {
                submission.epoch == epoch
                    && Some(submission.content) == content
                    && Some(submission.placement) == placement
                    && submission.pose.is_valid()
            })
    }
    pub fn close(&mut self) {
        self.native = None;
        self.deferred_refresh = false;
        self.inputs.clear();
        self.timeline.retarget(self.pose, true);
        self.retire_deadline = Instant::now()
            .checked_add(self.timeline.profile.exit.duration)
            .and_then(|at| at.checked_add(std::time::Duration::from_secs(1)));
        self.finished = false;
    }
}
