//! The person's hands on the desktop: pointer and keyboard events injected
//! into the X server with XTEST.
//!
//! A browser key event names a key ("a", "Enter", "Shift"); the server wants
//! a keycode. Keysyms bridge the two: the name becomes a keysym, the keyboard
//! mapping says which keycode carries it, and a keysym no keycode carries is
//! given a spare keycode for the life of the session, the way xdotool does.
//! Modifiers arrive as their own key events, so pressing the keycode that
//! carries `a` while Shift is down is how `A` is typed.

use std::collections::HashMap;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ConnectionExt, KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
    Keycode, Keysym, MOTION_NOTIFY_EVENT, Window,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

pub struct Hands {
    connection: RustConnection,
    root: Window,
    min_keycode: Keycode,
    per_keycode: usize,
    keysyms: Vec<Keysym>,
    spare: Vec<Keycode>,
    remapped: HashMap<Keysym, Keycode>,
}

impl Hands {
    pub fn new(display: &str) -> Result<Self, String> {
        let (connection, screen_number) =
            x11rb::connect(Some(display)).map_err(|error| format!("x11 connect: {error}"))?;
        let root = connection.setup().roots[screen_number].root;
        let min_keycode = connection.setup().min_keycode;
        let max_keycode = connection.setup().max_keycode;
        let mapping = connection
            .get_keyboard_mapping(min_keycode, max_keycode - min_keycode + 1)
            .map_err(|error| error.to_string())?
            .reply()
            .map_err(|error| format!("keyboard mapping: {error}"))?;
        let per_keycode = usize::from(mapping.keysyms_per_keycode);
        let spare = (min_keycode..=max_keycode)
            .filter(|keycode| {
                let start = usize::from(keycode - min_keycode) * per_keycode;
                mapping.keysyms[start..start + per_keycode]
                    .iter()
                    .all(|keysym| *keysym == 0)
            })
            .collect();
        Ok(Self {
            connection,
            root,
            min_keycode,
            per_keycode,
            keysyms: mapping.keysyms,
            spare,
            remapped: HashMap::new(),
        })
    }

    pub fn move_to(&self, x: i16, y: i16) -> Result<(), String> {
        self.fake(MOTION_NOTIFY_EVENT, 0, x, y)
    }

    pub fn button(&self, button: u8, down: bool) -> Result<(), String> {
        let kind = if down {
            BUTTON_PRESS_EVENT
        } else {
            BUTTON_RELEASE_EVENT
        };
        self.fake(kind, button, 0, 0)
    }

    pub fn key(&mut self, name: &str, down: bool) -> Result<(), String> {
        let Some(keysym) = keysym_for(name) else {
            return Ok(());
        };
        let keycode = self.keycode_for(keysym)?;
        let kind = if down {
            KEY_PRESS_EVENT
        } else {
            KEY_RELEASE_EVENT
        };
        self.fake(kind, keycode, 0, 0)
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<(), String> {
        self.connection
            .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0)
            .map_err(|error| error.to_string())?;
        self.connection.flush().map_err(|error| error.to_string())
    }

    fn keycode_for(&mut self, keysym: Keysym) -> Result<Keycode, String> {
        if let Some(keycode) = self.remapped.get(&keysym) {
            return Ok(*keycode);
        }
        let found = self
            .keysyms
            .chunks(self.per_keycode)
            .position(|columns| columns.contains(&keysym))
            .map(|index| self.min_keycode + index as u8);
        if let Some(keycode) = found {
            return Ok(keycode);
        }
        let keycode = self
            .spare
            .pop()
            .ok_or_else(|| format!("no spare keycode for keysym {keysym:#x}"))?;
        let columns = vec![keysym; self.per_keycode];
        self.connection
            .change_keyboard_mapping(1, keycode, self.per_keycode as u8, &columns)
            .map_err(|error| error.to_string())?
            .check()
            .map_err(|error| format!("remap keycode: {error}"))?;
        let start = usize::from(keycode - self.min_keycode) * self.per_keycode;
        self.keysyms[start..start + self.per_keycode].copy_from_slice(&columns);
        self.remapped.insert(keysym, keycode);
        Ok(keycode)
    }
}

/// The X keysym for a browser `KeyboardEvent.key`. A single character is
/// its own keysym below U+0100 and a Unicode keysym above; a named key is
/// looked up. Dead and unidentified keys are nothing to press.
pub fn keysym_for(name: &str) -> Option<Keysym> {
    let mut chars = name.chars();
    if let (Some(character), None) = (chars.next(), chars.next()) {
        let code = character as u32;
        if code < 0x20 {
            return None;
        }
        return Some(if code < 0x100 {
            code
        } else {
            0x0100_0000 | code
        });
    }
    if let Some(number) = name.strip_prefix('F')
        && let Ok(number) = number.parse::<u32>()
        && (1..=12).contains(&number)
    {
        return Some(0xffbe + number - 1);
    }
    Some(match name {
        "Enter" => 0xff0d,
        "Tab" => 0xff09,
        "Backspace" => 0xff08,
        "Escape" => 0xff1b,
        "Delete" => 0xffff,
        "Insert" => 0xff63,
        "Home" => 0xff50,
        "End" => 0xff57,
        "PageUp" => 0xff55,
        "PageDown" => 0xff56,
        "ArrowLeft" => 0xff51,
        "ArrowUp" => 0xff52,
        "ArrowRight" => 0xff53,
        "ArrowDown" => 0xff54,
        "Shift" => 0xffe1,
        "Control" => 0xffe3,
        "Alt" => 0xffe9,
        "AltGraph" => 0xfe03,
        "Meta" => 0xffeb,
        "CapsLock" => 0xffe5,
        "NumLock" => 0xff7f,
        "ScrollLock" => 0xff14,
        "ContextMenu" => 0xff67,
        "PrintScreen" => 0xff61,
        "Pause" => 0xff13,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn characters_are_their_own_keysyms() {
        assert_eq!(keysym_for("a"), Some(0x61));
        assert_eq!(keysym_for("A"), Some(0x41));
        assert_eq!(keysym_for(" "), Some(0x20));
        assert_eq!(keysym_for("é"), Some(0xe9));
        assert_eq!(keysym_for("€"), Some(0x0100_20ac));
    }

    #[test]
    fn named_keys_are_looked_up() {
        assert_eq!(keysym_for("Enter"), Some(0xff0d));
        assert_eq!(keysym_for("F5"), Some(0xffc2));
        assert_eq!(keysym_for("F13"), None);
        assert_eq!(keysym_for("Dead"), None);
    }
}
