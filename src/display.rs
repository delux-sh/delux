use unicode_general_category::{GeneralCategory, get_general_category};

/// Bounds work by input characters and escapes controls and invisible characters.
pub fn display(value: &str, limit: usize) -> String {
    render(value, limit, false, &mut false)
}

/// Preserves emoji presentation and joiners only within a graphic sequence.
pub(crate) fn preview_display(value: &str, following_graphic: &mut bool) -> String {
    render(value, usize::MAX, true, following_graphic)
}

fn render(value: &str, limit: usize, preview: bool, following_graphic: &mut bool) -> String {
    let mut rendered = String::new();
    for (count, character) in value.chars().enumerate() {
        if count == limit {
            rendered.push('…');
            break;
        }
        let code = character as u32;
        let graphic = !matches!(
            get_general_category(character),
            GeneralCategory::Control
                | GeneralCategory::Format
                | GeneralCategory::Unassigned
                | GeneralCategory::PrivateUse
                | GeneralCategory::Surrogate
                | GeneralCategory::LineSeparator
                | GeneralCategory::ParagraphSeparator
        );
        let invisible = matches!(
            code,
            0x034f
                | 0x115f..=0x1160
                | 0x17b4..=0x17b5
                | 0x2065
                | 0x3164
                | 0xffa0
                | 0xfff0..=0xfff8
                | 0xe0000
                | 0xe0002..=0xe001f
                | 0xe0080..=0xe00ff
                | 0xe01f0..=0xe0fff
                | 0x180b..=0x180d
                | 0x180f
                | 0xfe00..=0xfe0f
                | 0xe0100..=0xe01ef
        );
        if preview
            && *following_graphic
            && matches!(character, '\u{fe0e}' | '\u{fe0f}' | '\u{200d}')
        {
            rendered.push(character);
            *following_graphic = character != '\u{200d}';
            continue;
        }
        if graphic && !invisible {
            rendered.push(character);
            *following_graphic = !character.is_whitespace();
            continue;
        }
        *following_graphic = false;
        match character {
            '\n' => rendered.push_str("\\n"),
            '\r' => rendered.push_str("\\r"),
            '\t' => rendered.push_str("\\t"),
            _ => {
                let (prefix, width) = if code > 0xffff {
                    ('U', 8)
                } else if code > 0xff {
                    ('u', 4)
                } else {
                    ('x', 2)
                };
                rendered.push('\\');
                rendered.push(prefix);
                const HEX: &[u8; 16] = b"0123456789abcdef";
                for digit in (0..width).rev() {
                    rendered.push(HEX[((code >> (digit * 4)) & 15) as usize] as char);
                }
            }
        }
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::display;

    #[test]
    fn display_escapes_controls_and_invisible_characters() {
        for (input, expected) in [
            (
                "normal\x1b[2J\nother\u{202e}file",
                r"normal\x1b[2J\nother\u202efile",
            ),
            ("\x00\t\r\x7f\u{0085}", r"\x00\t\r\x7f\x85"),
            (
                "\u{200b}\u{200d}\u{2066}\u{feff}\u{034f}",
                r"\u200b\u200d\u2066\ufeff\u034f",
            ),
            ("\u{e000}\u{0378}\u{f0000}", r"\ue000\u0378\U000f0000"),
            ("\u{2028}\u{2029}", r"\u2028\u2029"),
            ("développeur 東京 🐈", "développeur 東京 🐈"),
            (
                "\u{115f}\u{3164}\u{180f}\u{fe0f}\u{e0100}",
                r"\u115f\u3164\u180f\ufe0f\U000e0100",
            ),
            (
                "⚠\u{fe0e} ⚠\u{fe0f} 👩\u{200d}💻",
                r"⚠\ufe0e ⚠\ufe0f 👩\u200d💻",
            ),
        ] {
            assert_eq!(display(input, 200), expected, "input: {input:?}");
        }
    }

    #[test]
    fn display_caps_input_characters_without_breaking_utf8() {
        assert_eq!(display("東京\x1b猫", 3), r"東京\x1b…");
        assert_eq!(display("東京猫", 3), "東京猫");
        assert_eq!(display(&"東".repeat(1024 * 1024), 2), "東東…");
        assert_eq!(display("text", 0), "…");
        assert_eq!(display("", 0), "");
    }
}
