use std::sync::atomic::{AtomicBool, Ordering};

use iced_core::clipboard::{Content, Error, Kind};
use layershellev::WindowWrapper;

static DISABLED: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_disabled() {
    DISABLED.store(true, Ordering::Relaxed);
}

pub(crate) fn is_disabled() -> bool {
    DISABLED.load(Ordering::Relaxed)
}

pub struct LayerShellClipboard {
    state: State,
}

enum State {
    Connected(window_clipboard::Clipboard),
    Unavailable,
}

impl LayerShellClipboard {
    /// Creates a new [`Clipboard`] for the given window.
    pub fn connect(window: &WindowWrapper) -> Self {
        #[allow(unsafe_code)]
        let state = unsafe { window_clipboard::Clipboard::connect(window) }
            .ok()
            .map(State::Connected)
            .unwrap_or(State::Unavailable);

        Self { state }
    }

    /// Creates a new [`Clipboard`] that isn't associated with a window.
    /// This clipboard will never contain a copied value.
    #[allow(unused)]
    pub fn unconnected() -> Self {
        Self {
            state: State::Unavailable,
        }
    }

    /// Reads the current content of the clipboard.
    pub fn read(&self, kind: Kind) -> Result<Content, Error> {
        match &self.state {
            State::Connected(clipboard) => match kind {
                Kind::Text => clipboard
                    .read()
                    .map(Content::Text)
                    .map_err(|_| Error::ClipboardUnavailable),
                _ => Err(Error::ContentNotAvailable),
            },
            State::Unavailable => Err(Error::ClipboardUnavailable),
        }
    }

    /// Writes the given contents to the clipboard.
    pub fn write(&mut self, contents: Content) -> Result<(), Error> {
        match &mut self.state {
            State::Connected(clipboard) => match contents {
                Content::Text(contents) => clipboard
                    .write(contents)
                    .map_err(|_| Error::ClipboardUnavailable),
                _ => Err(Error::ContentNotAvailable),
            },
            State::Unavailable => Err(Error::ClipboardUnavailable),
        }
    }
}

pub(crate) fn process_requests(
    requests: iced_core::Clipboard,
    clipboard: &mut LayerShellClipboard,
) {
    for kind in requests.reads {
        let _ = clipboard.read(kind);
    }

    if let Some(content) = requests.write {
        if let Err(error) = clipboard.write(content) {
            log::warn!("error writing to clipboard: {error:?}");
        }
    }
}
