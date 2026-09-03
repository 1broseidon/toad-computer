//! What the agent has once the display is up: the screen as a stream for
//! viewers, the hands that press keys and buttons, and the clipboard.

use std::sync::Mutex;

use crate::clipboard::Clipboard;
use crate::screen::Screen;
use crate::xtest::Hands;

pub struct Display {
    pub screen: Screen,
    pub hands: Mutex<Hands>,
    pub clipboard: Clipboard,
}

impl Display {
    pub fn open(display: &str) -> Result<Self, String> {
        Ok(Self {
            screen: Screen::start(display)?,
            hands: Mutex::new(Hands::new(display)?),
            clipboard: Clipboard::start(display)?,
        })
    }
}
