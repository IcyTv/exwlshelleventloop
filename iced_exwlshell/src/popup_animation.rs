//! Whole-surface transitions, independent of popup content and native input grabs.
use iced_core::{Rectangle, Transformation, Vector};
use std::time::{Duration, Instant};

/// A presentation pose shared by rendering and inverse input mapping.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pose {
    pub transformation: Transformation,
    pub opacity: f32,
    pub clip: Rectangle,
}
impl Pose {
    pub fn identity(bounds: Rectangle) -> Self {
        Self {
            transformation: Transformation::IDENTITY,
            opacity: 1.0,
            clip: bounds,
        }
    }
    pub fn is_valid(self) -> bool {
        self.transformation.as_ref().iter().all(|v| v.is_finite())
            && self.opacity.is_finite()
            && self.clip.x.is_finite()
            && self.clip.y.is_finite()
            && self.clip.width.is_finite()
            && self.clip.height.is_finite()
            && self.clip.width >= 0.0
            && self.clip.height >= 0.0
    }
    pub fn accepts_input(self) -> bool {
        self.is_valid()
            && self.opacity > 0.0
            && self.clip.width > 0.0
            && self.clip.height > 0.0
            && self
                .transformation
                .inverse()
                .as_ref()
                .iter()
                .all(|v| v.is_finite())
    }
    pub fn visible_bounds(self, bounds: Rectangle) -> Rectangle {
        use iced_core::Point;
        let corners = [
            Point::new(bounds.x, bounds.y),
            Point::new(bounds.x + bounds.width, bounds.y),
            Point::new(bounds.x, bounds.y + bounds.height),
            Point::new(bounds.x + bounds.width, bounds.y + bounds.height),
        ]
        .map(|p| p * self.transformation);
        let left = corners.iter().map(|p| p.x).fold(f32::INFINITY, f32::min);
        let right = corners
            .iter()
            .map(|p| p.x)
            .fold(f32::NEG_INFINITY, f32::max);
        let top = corners.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
        let bottom = corners
            .iter()
            .map(|p| p.y)
            .fold(f32::NEG_INFINITY, f32::max);
        Rectangle {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        }
        .intersection(&self.clip)
        .unwrap_or_default()
    }
}

pub(crate) fn rebase_input(
    source_origin: (i32, i32),
    target_origin: (i32, i32),
    source_scale: f64,
    target_scale: f64,
) -> Transformation {
    let scale = target_scale as f32;
    Transformation::translate(
        (source_origin.0 - target_origin.0) as f32 / scale,
        (source_origin.1 - target_origin.1) as f32 / scale,
    ) * Transformation::scale(source_scale as f32 / scale)
}

pub(crate) fn map_event(event: iced_core::Event, pose: Pose) -> iced_core::Event {
    use iced_core::{Event, mouse, touch};
    let inverse = pose.transformation.inverse();
    match event {
        Event::Mouse(mouse::Event::CursorMoved { position }) => {
            Event::Mouse(mouse::Event::CursorMoved {
                position: position * inverse,
            })
        }
        Event::Mouse(mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Pixels { x, y },
        }) => {
            let delta = Vector::new(x, y) * inverse;
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels {
                    x: delta.x,
                    y: delta.y,
                },
            })
        }
        Event::Touch(event) => Event::Touch(match event {
            touch::Event::FingerPressed { id, position } => touch::Event::FingerPressed {
                id,
                position: position * inverse,
            },
            touch::Event::FingerMoved { id, position } => touch::Event::FingerMoved {
                id,
                position: position * inverse,
            },
            touch::Event::FingerLifted { id, position } => touch::Event::FingerLifted {
                id,
                position: position * inverse,
            },
            touch::Event::FingerLost { id, position } => touch::Event::FingerLost {
                id,
                position: position * inverse,
            },
        }),
        event => event,
    }
}

/// A finite transition. Custom samplers may animate transform, opacity and clip.
/// `from` is the last submitted pose, so interrupted transitions do not jump.
#[derive(Debug, Clone, Copy)]
pub struct Transition {
    pub duration: Duration,
    pub sample: fn(f32, Pose, Rectangle) -> Pose,
}
#[derive(Debug, Clone, Copy)]
pub struct Profile {
    pub initial: fn(Rectangle) -> Pose,
    pub enter: Transition,
    pub exit: Transition,
}
fn grow(bounds: Rectangle) -> Pose {
    Pose {
        transformation: Transformation::translate(bounds.center_x(), bounds.y)
            * Transformation::scale(0.8)
            * Transformation::translate(-bounds.center_x(), -bounds.y),
        ..Pose::identity(bounds)
    }
}
fn interpolate(from: Pose, target: Pose, amount: f32) -> Pose {
    let scale = from.transformation.scale_factor()
        + (target.transformation.scale_factor() - from.transformation.scale_factor()) * amount;
    let a = from.transformation.translation();
    let b = target.transformation.translation();
    let offset = Vector::new(a.x + (b.x - a.x) * amount, a.y + (b.y - a.y) * amount);
    Pose {
        transformation: Transformation::translate(offset.x, offset.y)
            * Transformation::scale(scale),
        opacity: from.opacity + (target.opacity - from.opacity) * amount,
        clip: target.clip,
    }
}
impl Default for Profile {
    fn default() -> Self {
        Self {
            initial: grow,
            enter: Transition {
                duration: Duration::from_millis(50),
                sample: |t, from, bounds| {
                    interpolate(from, Pose::identity(bounds), 1.0 - (1.0 - t).powi(4))
                },
            },
            exit: Transition {
                duration: Duration::from_millis(50),
                sample: |t, from, bounds| interpolate(from, grow(bounds), t.powi(4)),
            },
        }
    }
}

pub(crate) struct Timeline {
    pub profile: Profile,
    pub from: Pose,
    pub started: Option<Instant>,
    pub closing: bool,
}
impl Timeline {
    pub fn new(profile: Profile, bounds: Rectangle) -> Self {
        Self {
            profile,
            from: (profile.initial)(bounds),
            started: None,
            closing: false,
        }
    }
    pub fn retarget(&mut self, from: Pose, closing: bool) {
        self.from = from;
        self.closing = closing;
        self.started = None;
    }
    /// Start only on the first successful submission. Surface recovery is not
    /// an animation retarget: retries keep the original trajectory/deadline.
    pub fn submit<E>(&mut self, submit: impl FnOnce() -> Result<(), E>) -> Result<(), E> {
        let result = submit();
        if result.is_ok() {
            self.started.get_or_insert(Instant::now());
        }
        result
    }
    pub fn sample(&self, now: Instant, bounds: Rectangle) -> (Pose, bool) {
        let transition = if self.closing {
            self.profile.exit
        } else {
            self.profile.enter
        };
        let progress = self.started.map_or(0.0, |start| {
            if transition.duration.is_zero() {
                1.0
            } else {
                (now.saturating_duration_since(start).as_secs_f32()
                    / transition.duration.as_secs_f32())
                .min(1.0)
            }
        });
        let mut pose = (transition.sample)(progress, self.from, bounds);
        if pose.is_valid() {
            pose.clip = pose.clip.intersection(&bounds).unwrap_or_default();
        }
        (pose, progress >= 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pointer_touch_and_pixel_scroll_share_the_inverse_pose() {
        use iced_core::{Event, Point, Size, mouse, touch};
        let pose = Pose {
            transformation: Transformation::translate(12.0, 40.0) * Transformation::scale(0.5),
            ..Pose::identity(Rectangle::with_size(Size::new(400.0, 300.0)))
        };
        let point = Point::new(60.0, 80.0);
        let rendered = point * pose.transformation;
        assert_eq!(
            map_event(
                Event::Mouse(mouse::Event::CursorMoved { position: rendered }),
                pose
            ),
            Event::Mouse(mouse::Event::CursorMoved { position: point })
        );
        let id = touch::Finger(7);
        for event in [
            touch::Event::FingerPressed {
                id,
                position: rendered,
            },
            touch::Event::FingerMoved {
                id,
                position: rendered,
            },
            touch::Event::FingerLifted {
                id,
                position: rendered,
            },
            touch::Event::FingerLost {
                id,
                position: rendered,
            },
        ] {
            let mapped = map_event(Event::Touch(event), pose);
            let Event::Touch(
                touch::Event::FingerPressed { position, .. }
                | touch::Event::FingerMoved { position, .. }
                | touch::Event::FingerLifted { position, .. }
                | touch::Event::FingerLost { position, .. },
            ) = mapped
            else {
                panic!("touch event changed kind")
            };
            assert_eq!(position, point);
        }
        assert_eq!(
            map_event(
                Event::Mouse(mouse::Event::WheelScrolled {
                    delta: mouse::ScrollDelta::Pixels { x: 3.0, y: 5.0 }
                }),
                pose
            ),
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels { x: 6.0, y: 10.0 }
            })
        );
        let lines = Event::Mouse(mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Lines { x: 1.0, y: 2.0 },
        });
        assert_eq!(map_event(lines.clone(), pose), lines);
    }

    #[test]
    fn custom_clip_is_limited_to_the_actual_viewport() {
        let bounds = Rectangle::with_size(iced_core::Size::new(100.0, 80.0));
        let mut profile = Profile::default();
        profile.enter.sample = |_, _, _| {
            Pose::identity(Rectangle {
                x: -20.0,
                y: -20.0,
                width: 200.0,
                height: 200.0,
            })
        };
        let timeline = Timeline::new(profile, bounds);
        assert_eq!(timeline.sample(Instant::now(), bounds).0.clip, bounds);
    }
    #[test]
    fn default_reversal_is_continuous_and_finite() {
        let bounds = Rectangle::with_size(iced_core::Size::new(400.0, 300.0));
        let mut timeline = Timeline::new(Profile::default(), bounds);
        let now = Instant::now();
        assert!((timeline.sample(now, bounds).0.transformation.scale_factor() - 0.8).abs() < 0.001);
        timeline.started = Some(now);
        let open = timeline.sample(now + Duration::from_millis(20), bounds).0;
        timeline.retarget(open, true);
        assert_eq!(timeline.sample(now, bounds).0, open);
        timeline.started = Some(now);
        let closing = timeline.sample(now + Duration::from_millis(20), bounds).0;
        timeline.retarget(closing, false);
        assert_eq!(timeline.sample(now, bounds).0, closing);
        assert!(closing.is_valid());
    }

    #[test]
    fn surface_retry_preserves_opening_progress_and_original_deadline() {
        let bounds = Rectangle::with_size(iced_core::Size::new(400.0, 300.0));
        let mut timeline = Timeline::new(Profile::default(), bounds);
        let start = Instant::now();
        timeline.started = Some(start);
        let before = timeline.sample(start + Duration::from_millis(25), bounds).0;
        assert!(timeline.submit(|| Err(())).is_err());
        let (after, finished) = timeline.sample(start + Duration::from_millis(34), bounds);
        assert_eq!(
            timeline.started,
            Some(start),
            "a retry must not restart the animation clock"
        );
        assert!(after.transformation.scale_factor() >= before.transformation.scale_factor());
        assert!(!finished);
        assert!(timeline.sample(start + Duration::from_millis(50), bounds).1);
    }

    #[test]
    fn only_the_first_successful_submission_starts_the_clock() {
        let bounds = Rectangle::with_size(iced_core::Size::new(400.0, 300.0));
        let mut timeline = Timeline::new(Profile::default(), bounds);
        assert!(timeline.submit(|| Err(())).is_err());
        assert!(timeline.started.is_none());
        assert_eq!(timeline.submit(|| Ok::<_, ()>(())), Ok(()));
        let started = timeline
            .started
            .expect("first successful submission starts the clock");
        assert_eq!(timeline.submit(|| Ok::<_, ()>(())), Ok(()));
        assert_eq!(timeline.started, Some(started));
    }

    #[test]
    fn surface_retry_preserves_closing_progress_and_unstarted_submissions_stay_unstarted() {
        let bounds = Rectangle::with_size(iced_core::Size::new(400.0, 300.0));
        let mut timeline = Timeline::new(Profile::default(), bounds);
        assert!(timeline.submit(|| Err(())).is_err());
        assert!(timeline.started.is_none());
        timeline.retarget(Pose::identity(bounds), true);
        let start = Instant::now();
        timeline.started = Some(start);
        let before = timeline.sample(start + Duration::from_millis(30), bounds).0;
        assert!(timeline.submit(|| Err(())).is_err());
        let (after, _) = timeline.sample(start + Duration::from_millis(40), bounds);
        assert_eq!(timeline.started, Some(start));
        assert!(after.transformation.scale_factor() <= before.transformation.scale_factor());
        assert!(timeline.sample(start + Duration::from_millis(50), bounds).1);
    }
    #[test]
    fn queued_coordinates_keep_the_same_root_point_across_resize_and_app_scale() {
        use iced_core::Point;
        for source_scale in [1.0, 1.25, 2.0] {
            for target_scale in [1.0, 1.5, 2.0] {
                let source = (-30, -71);
                let target = (-99, 57);
                let point = Point::new(612.0, 246.0);
                let mapped = point * rebase_input(source, target, source_scale, target_scale);
                assert!(
                    (point.x * source_scale as f32 + source.0 as f32
                        - (mapped.x * target_scale as f32 + target.0 as f32))
                        .abs()
                        < 0.001
                );
                assert!(
                    (point.y * source_scale as f32 + source.1 as f32
                        - (mapped.y * target_scale as f32 + target.1 as f32))
                        .abs()
                        < 0.001
                );
            }
        }
    }
    #[test]
    fn zero_scale_is_renderable_but_never_interactive() {
        let bounds = Rectangle::with_size(iced_core::Size::new(100.0, 100.0));
        let pose = Pose {
            transformation: Transformation::scale(0.0),
            ..Pose::identity(bounds)
        };
        assert!(pose.is_valid());
        assert!(!pose.accepts_input());
        let mirrored = Pose {
            transformation: Transformation::translate(100.0, 100.0) * Transformation::scale(-1.0),
            ..Pose::identity(bounds)
        };
        assert_eq!(mirrored.visible_bounds(bounds), bounds);
    }
}
