//! Input-empty visual subsurfaces anchored to a layer, independent of popup grabs.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub root: id::Id,
    pub root_epoch: u64,
    pub origin: (i32, i32),
    pub capacity: (u32, u32),
    pub scale: u32,
}

impl<T: 'static> WindowState<T> {
    /// Retry an unconfirmed visual even if its frame callback was withheld.
    /// Late callbacks from the superseded commit cannot release a newer slot.
    pub fn retry_visual_present(&mut self, id: id::Id) {
        if let Some(unit) = self.get_mut_unit_with_id(id) {
            if matches!(unit.shell, Shell::Visual(_)) {
                unit.frame_callback = None;
                unit.present_available_state = PresentAvailableState::Available;
            }
        }
    }
    /// Schedules an owner turn even when an occluded surface has no frame callback.
    pub fn request_owner_wakeup(&mut self, at: Instant) {
        self.owner_wakeup = Some(self.owner_wakeup.map_or(at, |old| old.min(at)));
    }

    pub fn supports_presentation(&self) -> bool {
        self.presentation.is_some()
    }
    /// Resolves the buffer origin using configured geometry throughout ancestry.
    pub fn popup_visual_placement(&self, popup: id::Id) -> Option<Placement> {
        let source = self.get_unit_with_id(popup)?;
        if !matches!(source.shell, Shell::PopUp(_))
            || !source.configured
            || source.pending_reposition.is_some()
        {
            return None;
        }
        let mut current = source;
        let mut origin = (0i32, 0i32);
        loop {
            if matches!(current.shell, Shell::LayerShell(_)) {
                return Some(Placement {
                    root: current.id,
                    root_epoch: current.coordinate_epoch.get(),
                    origin,
                    capacity: source.size,
                    scale: source.scale,
                });
            }
            if !matches!(current.shell, Shell::PopUp(_)) {
                return None;
            }
            let parent = self.get_unit_with_id(current.parent?)?;
            let geometry = current
                .popup_surface_geometry
                .map_or((0, 0), |(r, _)| (r.0, r.1));
            let parent_geometry = parent
                .popup_surface_geometry
                .map_or((0, 0), |(r, _)| (r.0, r.1));
            origin.0 = origin
                .0
                .checked_add(current.popup_position.0)?
                .checked_add(parent_geometry.0)?
                .checked_sub(geometry.0)?;
            origin.1 = origin
                .1
                .checked_add(current.popup_position.1)?
                .checked_add(parent_geometry.1)?
                .checked_sub(geometry.1)?;
            current = parent;
        }
    }

    /// Creates a host without an xdg role, keyboard focus, or application binding.
    pub fn create_visual_host(
        &mut self,
        placement: Placement,
        above: Option<id::Id>,
    ) -> Option<id::Id> {
        let manager = self.subcompositor.as_ref()?;
        let viewporter = self.viewporter.as_ref()?;
        let display = self.display.clone()?;
        let root = self.get_unit_with_id(placement.root)?;
        let qh = root.qh.clone();
        let compositor = root.wmcompositor.clone();
        let parent = root.window.wl_surface.clone();
        let surface = compositor.create_surface(&qh, ());
        let empty = compositor.create_region(&qh, ());
        surface.set_input_region(Some(&empty));
        empty.destroy();
        let child = manager.get_subsurface(&surface, &parent, &qh, ());
        let sibling = above
            .and_then(|id| self.get_unit_with_id(id))
            .map(|unit| unit.get_wlsurface())
            .unwrap_or(&parent);
        child.place_above(sibling);
        child.set_position(placement.origin.0, placement.origin.1);
        child.set_desync();
        let viewport = viewporter.get_viewport(&surface, &qh, ());
        viewport.set_destination(placement.capacity.0 as i32, placement.capacity.1 as i32);
        parent.commit();
        let id = id::Id::unique();
        let mut unit =
            WindowStateUnitBuilder::new(id, qh, display, surface, compositor, Shell::Visual(child))
                .parent(Some(placement.root))
                .size(placement.capacity)
                .viewport(Some(viewport))
                .build();
        unit.scale = placement.scale;
        self.units.push(unit);
        Some(id)
    }

    /// Moves/resizes the host using the same placement as its interactive role.
    pub fn configure_visual_host(&mut self, id: id::Id, placement: Placement) -> Option<()> {
        let unit = self.get_mut_unit_with_id(id)?;
        let Shell::Visual(child) = &unit.shell else {
            return None;
        };
        child.set_position(placement.origin.0, placement.origin.1);
        unit.size = placement.capacity;
        unit.scale = placement.scale;
        unit.try_set_viewport_destination(placement.capacity.0 as i32, placement.capacity.1 as i32);
        self.get_unit_with_id(placement.root)?
            .get_wlsurface()
            .commit();
        Some(())
    }
}

impl<T: 'static> WindowStateUnit<T> {
    /// Replaces input with explicit rectangles; an empty slice disables input.
    /// This never installs a null (infinite) region.
    pub fn set_visual_input_region(&self, rectangles: &[(i32, i32, i32, i32)]) {
        let region = self.wmcompositor.create_region(&self.qh, ());
        for &(x, y, w, h) in rectangles {
            if w > 0 && h > 0 {
                region.add(x, y, w, h);
            }
        }
        self.window.wl_surface.set_input_region(Some(&region));
        region.destroy();
    }
}
