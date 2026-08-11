use ansi_term::{
    AnsiString, AnsiStrings,
    Color::{Fixed, Rgb},
    Style, unstyled_len,
};

use crate::LinePart;
use zellij_tile::prelude::*;
use zellij_tile_utils::palette_match;

macro_rules! strings {
    ($AnsiStrings:expr) => {{
        let strings: &[AnsiString] = $AnsiStrings;

        let ansi_strings = AnsiStrings(strings);

        LinePart {
            part: format!("{}", ansi_strings),
            len: unstyled_len(&ansi_strings),
        }
    }};
}

pub fn zellij_setup_check_full(help: &ModeInfo) -> LinePart {
    // Tip: Having issues with vc-frame? Try running "vc-frame setup --check"
    let orange_color = palette_match!(help.style.colors.text_unselected.emphasis_0);

    strings!(&[
        Style::new().paint(" Tip: "),
        Style::new().paint("Having issues with vc-frame? Try running "),
        Style::new()
            .fg(orange_color)
            .bold()
            .paint("vc-frame setup --check"),
    ])
}

pub fn zellij_setup_check_medium(help: &ModeInfo) -> LinePart {
    // Tip: Run "vc-frame setup --check" to find issues
    let orange_color = palette_match!(help.style.colors.text_unselected.emphasis_0);

    strings!(&[
        Style::new().paint(" Tip: "),
        Style::new().paint("Run "),
        Style::new()
            .fg(orange_color)
            .bold()
            .paint("vc-frame setup --check"),
        Style::new().paint(" to find issues"),
    ])
}

pub fn zellij_setup_check_short(help: &ModeInfo) -> LinePart {
    // Run "vc-frame setup --check" to find issues
    let orange_color = palette_match!(help.style.colors.text_unselected.emphasis_0);

    strings!(&[
        Style::new().paint(" Run "),
        Style::new()
            .fg(orange_color)
            .bold()
            .paint("vc-frame setup --check"),
        Style::new().paint(" to find issues"),
    ])
}
