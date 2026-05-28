use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use iced_core::window::Id;

static OUTPUT_INFOS: OnceLock<Mutex<HashMap<Id, OutputInfo>>> = OnceLock::new();

fn output_infos() -> &'static Mutex<HashMap<Id, OutputInfo>> {
    OUTPUT_INFOS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Information about the output associated with an Iced layer-shell window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputInfo {
    /// The compositor-provided xdg-output name.
    pub name: String,
    /// The compositor-provided xdg-output description.
    pub description: String,
    /// Logical output size in compositor coordinates.
    pub logical_size: (i32, i32),
    /// Logical output position in compositor coordinates.
    pub position: (i32, i32),
}

pub(crate) fn set_output_info(
    id: Id,
    name: impl Into<String>,
    description: impl Into<String>,
    logical_size: (i32, i32),
    position: (i32, i32),
) {
    output_infos()
        .lock()
        .expect("output info registry lock poisoned")
        .insert(
            id,
            OutputInfo {
                name: name.into(),
                description: description.into(),
                logical_size,
                position,
            },
        );
}

pub(crate) fn remove_output_info(id: Id) {
    output_infos()
        .lock()
        .expect("output info registry lock poisoned")
        .remove(&id);
}

/// Returns the output information associated with an Iced layer-shell window.
pub fn output_info(id: Id) -> Option<OutputInfo> {
    output_infos()
        .lock()
        .expect("output info registry lock poisoned")
        .get(&id)
        .cloned()
}

/// Returns the xdg-output logical size associated with an Iced layer-shell window.
pub fn output_logical_size(id: Id) -> Option<(i32, i32)> {
    output_info(id).map(|info| info.logical_size)
}

/// Returns the xdg-output name associated with an Iced layer-shell window.
pub fn output_name(id: Id) -> Option<String> {
    output_info(id).map(|info| info.name)
}
