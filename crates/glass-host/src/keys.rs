//! Key names for agents ("cmd+shift+4", "return") and US-layout typing, as macOS virtual
//! keycodes (kVK_*), the codes every input sink takes (Linux maps them to evdev).

pub const COMMAND: u16 = 0x37;
pub const SHIFT: u16 = 0x38;
pub const OPTION: u16 = 0x3A;
pub const CONTROL: u16 = 0x3B;
pub const FUNCTION: u16 = 0x3F;

/// A key by name: letters, digits, punctuation, and names like `return`, `left`, `f5`.
pub fn keycode(name: &str) -> Option<u16> {
    let name = name.trim().to_ascii_lowercase();
    if let [c] = name.as_bytes() {
        return char_key(*c as char).map(|(k, _)| k);
    }
    Some(match name.as_str() {
        "cmd" | "command" | "meta" | "super" | "win" => COMMAND,
        "shift" => SHIFT,
        "alt" | "option" | "opt" => OPTION,
        "ctrl" | "control" => CONTROL,
        "fn" => FUNCTION,
        "return" | "enter" => 0x24,
        "tab" => 0x30,
        "space" => 0x31,
        "backspace" | "delete" => 0x33,
        "escape" | "esc" => 0x35,
        "forwarddelete" | "del" => 0x75,
        "capslock" => 0x39,
        "home" => 0x73,
        "end" => 0x77,
        "pageup" => 0x74,
        "pagedown" => 0x79,
        "left" => 0x7B,
        "right" => 0x7C,
        "down" => 0x7D,
        "up" => 0x7E,
        "f1" => 0x7A,
        "f2" => 0x78,
        "f3" => 0x63,
        "f4" => 0x76,
        "f5" => 0x60,
        "f6" => 0x61,
        "f7" => 0x62,
        "f8" => 0x64,
        "f9" => 0x65,
        "f10" => 0x6D,
        "f11" => 0x67,
        "f12" => 0x6F,
        "minus" => 0x1B,
        "equal" | "plus" => 0x18,
        "comma" => 0x2B,
        "period" => 0x2F,
        "slash" => 0x2C,
        _ => return None,
    })
}

/// "cmd+shift+4" → [Command, Shift, 4], modifiers first. `None` names the unknown key.
pub fn combo(keys: &str) -> Result<Vec<u16>, String> {
    let mut codes: Vec<u16> = keys
        .split('+')
        .map(|k| keycode(k).ok_or_else(|| format!("unknown key {k:?}")))
        .collect::<Result<_, _>>()?;
    if codes.is_empty() {
        return Err("no keys".into());
    }
    // Modifiers go down first whatever order they were written in.
    codes.sort_by_key(|c| ![COMMAND, SHIFT, OPTION, CONTROL, FUNCTION].contains(c));
    Ok(codes)
}

/// The key for a character on a US keyboard, and whether it needs Shift.
pub fn char_key(c: char) -> Option<(u16, bool)> {
    const LETTERS: [u16; 26] = [
        0x00, 0x0B, 0x08, 0x02, 0x0E, 0x03, 0x05, 0x04, 0x22, 0x26, 0x28, 0x25, 0x2E, 0x2D, 0x1F,
        0x23, 0x0C, 0x0F, 0x01, 0x11, 0x20, 0x09, 0x0D, 0x07, 0x10, 0x06,
    ];
    const DIGITS: [u16; 10] = [0x1D, 0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1A, 0x1C, 0x19];
    Some(match c {
        'a'..='z' => (LETTERS[c as usize - 'a' as usize], false),
        'A'..='Z' => (LETTERS[c as usize - 'A' as usize], true),
        '0'..='9' => (DIGITS[c as usize - '0' as usize], false),
        ' ' => (0x31, false),
        '\n' => (0x24, false),
        '\t' => (0x30, false),
        '-' => (0x1B, false),
        '_' => (0x1B, true),
        '=' => (0x18, false),
        '+' => (0x18, true),
        '[' => (0x21, false),
        '{' => (0x21, true),
        ']' => (0x1E, false),
        '}' => (0x1E, true),
        '\\' => (0x2A, false),
        '|' => (0x2A, true),
        ';' => (0x29, false),
        ':' => (0x29, true),
        '\'' => (0x27, false),
        '"' => (0x27, true),
        ',' => (0x2B, false),
        '<' => (0x2B, true),
        '.' => (0x2F, false),
        '>' => (0x2F, true),
        '/' => (0x2C, false),
        '?' => (0x2C, true),
        '`' => (0x32, false),
        '~' => (0x32, true),
        '!' => (0x12, true),
        '@' => (0x13, true),
        '#' => (0x14, true),
        '$' => (0x15, true),
        '%' => (0x17, true),
        '^' => (0x16, true),
        '&' => (0x1A, true),
        '*' => (0x1C, true),
        '(' => (0x19, true),
        ')' => (0x1D, true),
        _ => return None,
    })
}

/// The key presses that type `c` on a US keyboard (with Shift if it needs it).
pub fn char_events(c: char) -> Option<Vec<glass_proto::InputEvent>> {
    use glass_proto::InputEvent::{KeyDown, KeyUp};
    let (key, shift) = char_key(c)?;
    let mut events = Vec::with_capacity(4);
    if shift {
        events.push(KeyDown { keycode: SHIFT });
    }
    events.extend([KeyDown { keycode: key }, KeyUp { keycode: key }]);
    if shift {
        events.push(KeyUp { keycode: SHIFT });
    }
    Some(events)
}

/// `text` as key presses on a US keyboard, for hosts that can't type text as is (Linux),
/// and how many characters had no key and were left out.
pub fn text_key_events(text: &str) -> (Vec<glass_proto::InputEvent>, usize) {
    let mut missing = 0;
    let mut events = Vec::new();
    for c in text.chars() {
        match char_events(c) {
            Some(e) => events.extend(e),
            None => missing += 1,
        }
    }
    (events, missing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_without_a_text_path_is_typed_key_by_key() {
        use glass_proto::InputEvent::{KeyDown, KeyUp};
        let (events, missing) = text_key_events("Aé");
        assert_eq!(events, [KeyDown { keycode: SHIFT }, KeyDown { keycode: 0 }, KeyUp { keycode: 0 }, KeyUp { keycode: SHIFT }]);
        assert_eq!(missing, 1);
    }

    #[test]
    fn combos_put_modifiers_first() {
        assert_eq!(combo("4+shift+cmd"), Ok(vec![SHIFT, COMMAND, 0x15]));
        assert_eq!(combo("Return"), Ok(vec![0x24]));
        assert!(combo("cmd+nope").is_err());
    }

    #[test]
    fn typing_needs_shift_for_capitals_and_symbols() {
        assert_eq!(char_key('a'), Some((0x00, false)));
        assert_eq!(char_key('A'), Some((0x00, true)));
        assert_eq!(char_key('?'), Some((0x2C, true)));
        assert_eq!(char_key('é'), None);
    }
}
