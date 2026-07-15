//! Mapping from the upstream sky `press_key` / `type_text` contract (X keysym-style
//! names and arbitrary text) to X11 keysym numbers, which the xdg-desktop-portal
//! RemoteDesktop `NotifyKeyboardKeysym` call consumes directly.

use anyhow::{bail, Result};

/// Convert a character to an X11 keysym.
///
/// Latin-1 maps 1:1 to its codepoint; everything else uses the X11 Unicode
/// keysym convention (`0x01000000 + codepoint`). The compositor resolves the
/// keysym to the correct physical key and modifier level, so uppercase and
/// shifted symbols need no explicit Shift handling.
pub fn char_to_keysym(c: char) -> u32 {
    let cp = c as u32;
    if cp == 0x08 {
        0xff08 // BackSpace
    } else if cp == 0x09 {
        0xff09 // Tab
    } else if cp == 0x0a || cp == 0x0d {
        0xff0d // Return
    } else if cp == 0x1b {
        0xff1b // Escape
    } else if (0x20..=0xff).contains(&cp) {
        cp
    } else {
        0x0100_0000 + cp
    }
}

/// Resolve a single X keysym-style name to its keysym number.
pub fn name_to_keysym(name: &str) -> Result<u32> {
    let n = name.trim();
    if n.is_empty() {
        bail!("empty key name");
    }
    // Single printable character: use the character mapping directly.
    let mut chars = n.chars();
    let first = chars.next().unwrap();
    if chars.next().is_none() && (first as u32) >= 0x20 {
        return Ok(char_to_keysym(first));
    }
    let ks = match n {
        "Return" | "Enter" => 0xff0d,
        "KP_Enter" => 0xff8d,
        "Tab" => 0xff09,
        "space" | "Space" => 0x20,
        "BackSpace" | "Backspace" => 0xff08,
        "Escape" | "Esc" => 0xff1b,
        "Delete" | "Del" => 0xffff,
        "Insert" => 0xff63,
        "Home" => 0xff50,
        "End" => 0xff57,
        "Prior" | "Page_Up" | "PageUp" => 0xff55,
        "Next" | "Page_Down" | "PageDown" => 0xff56,
        "Up" => 0xff52,
        "Down" => 0xff54,
        "Left" => 0xff51,
        "Right" => 0xff53,
        "Menu" => 0xff67,
        "Print" => 0xff61,
        "Pause" => 0xff13,
        "Caps_Lock" | "CapsLock" => 0xffe5,
        "Num_Lock" | "NumLock" => 0xff7f,
        "Scroll_Lock" | "ScrollLock" => 0xff14,
        "Control" | "control" | "Ctrl" | "ctrl" | "Control_L" => 0xffe3,
        "Control_R" => 0xffe4,
        "Shift" | "shift" | "Shift_L" => 0xffe1,
        "Shift_R" => 0xffe2,
        "Alt" | "alt" | "Alt_L" | "Meta" | "meta" | "Meta_L" => 0xffe9,
        "Alt_R" | "Meta_R" => 0xffea,
        "ISO_Level3_Shift" | "AltGr" => 0xfe03,
        "Super" | "super" | "Super_L" | "Win" | "win" | "Windows" | "Cmd" | "cmd" | "Command" => {
            0xffeb
        }
        "Super_R" => 0xffec,
        "F1" => 0xffbe,
        "F2" => 0xffbf,
        "F3" => 0xffc0,
        "F4" => 0xffc1,
        "F5" => 0xffc2,
        "F6" => 0xffc3,
        "F7" => 0xffc4,
        "F8" => 0xffc5,
        "F9" => 0xffc6,
        "F10" => 0xffc7,
        "F11" => 0xffc8,
        "F12" => 0xffc9,
        "KP_0" => 0xffb0,
        "KP_1" => 0xffb1,
        "KP_2" => 0xffb2,
        "KP_3" => 0xffb3,
        "KP_4" => 0xffb4,
        "KP_5" => 0xffb5,
        "KP_6" => 0xffb6,
        "KP_7" => 0xffb7,
        "KP_8" => 0xffb8,
        "KP_9" => 0xffb9,
        "KP_Add" => 0xffab,
        "KP_Subtract" => 0xffad,
        "KP_Multiply" => 0xffaa,
        "KP_Divide" => 0xffaf,
        "KP_Decimal" => 0xffae,
        _ => bail!("unsupported key name: {n}"),
    };
    Ok(ks)
}

/// Parse a `+`-separated chord (e.g. `Control_L+a`, `super+d`) into an ordered
/// list of keysyms. Whitespace around `+` is ignored.
pub fn parse_chord(spec: &str) -> Result<Vec<u32>> {
    let mut out = Vec::new();
    for part in spec.split('+') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        out.push(name_to_keysym(p)?);
    }
    if out.is_empty() {
        bail!("empty key chord: {spec}");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{char_to_keysym, name_to_keysym, parse_chord};

    #[test]
    fn maps_ascii_and_unicode_characters() {
        assert_eq!(char_to_keysym('A'), 'A' as u32);
        assert_eq!(char_to_keysym('\n'), 0xff0d);
        assert_eq!(char_to_keysym('λ'), 0x0100_0000 + 'λ' as u32);
    }

    #[test]
    fn supports_documented_lowercase_modifier_aliases() {
        assert_eq!(parse_chord("super+d").unwrap(), vec![0xffeb, 'd' as u32]);
        assert_eq!(parse_chord("ctrl + a").unwrap(), vec![0xffe3, 'a' as u32]);
    }

    #[test]
    fn rejects_unknown_names_and_empty_chords() {
        assert!(name_to_keysym("NotARealKey").is_err());
        assert!(parse_chord(" + ").is_err());
    }
}
