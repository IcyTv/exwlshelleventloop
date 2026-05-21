use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};

use iced_core::window::Id;

static OUTPUT_NAMES: OnceLock<Mutex<HashMap<Id, String>>> = OnceLock::new();

fn output_names() -> &'static Mutex<HashMap<Id, String>> {
    OUTPUT_NAMES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn set_output_name(id: Id, output_name: impl Into<String>) {
    output_names()
        .lock()
        .expect("output name registry lock poisoned")
        .insert(id, output_name.into());
}

pub(crate) fn remove_output_name(id: Id) {
    output_names()
        .lock()
        .expect("output name registry lock poisoned")
        .remove(&id);
}

/// Returns the xdg-output name associated with an Iced layer-shell window.
pub fn output_name(id: Id) -> Option<String> {
    output_names()
        .lock()
        .expect("output name registry lock poisoned")
        .get(&id)
        .cloned()
}
