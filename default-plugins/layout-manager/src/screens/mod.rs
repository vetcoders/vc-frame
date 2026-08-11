mod import_layout;
mod layout_list;
mod new_layout_from_session;
mod rename_layout;

use crate::ui::{ErrorMessage, MultiLineErrorMessage, truncate_line_with_ansi, wrap_text_to_width};
use zellij_tile::prelude::{KeyWithModifier, LayoutMetadata, Text, print_text_with_coordinates};

pub use import_layout::ImportLayoutScreen;
pub use layout_list::LayoutListScreen;
pub use new_layout_from_session::NewLayoutFromCurrentSessionScreen;
pub use rename_layout::RenameLayoutScreen;

#[derive(Clone)]
pub enum Screen {
    LayoutList(LayoutListScreen),
    NewLayoutFromSession(NewLayoutFromCurrentSessionScreen),
    ImportLayout(ImportLayoutScreen),
    RenameLayout(RenameLayoutScreen),
    Error(ErrorScreen),
    ErrorDetail(ErrorDetailScreen),
}

impl Default for Screen {
    fn default() -> Self {
        Screen::LayoutList(LayoutListScreen::default())
    }
}

/// Optimistic state updates to apply before Zellij confirms
#[derive(Clone, Debug)]
pub enum OptimisticUpdate {
    Delete(String), // file_name
    Rename {
        old_name: String,
        new_name: String,
    },
    Add {
        name: String,
        metadata: LayoutMetadata,
    },
}

/// Response from screen key handlers
#[derive(Default)]
pub struct KeyResponse {
    pub should_render: bool,
    pub new_screen: Option<Screen>,
    pub optimistic_update: Option<OptimisticUpdate>,
}

impl KeyResponse {
    pub fn render() -> Self {
        KeyResponse {
            should_render: true,
            new_screen: None,
            optimistic_update: None,
        }
    }

    pub fn new_screen(screen: Screen) -> Self {
        KeyResponse {
            should_render: true,
            new_screen: Some(screen),
            optimistic_update: None,
        }
    }

    pub fn with_optimistic(mut self, update: OptimisticUpdate) -> Self {
        self.optimistic_update = Some(update);
        self
    }

    pub fn none() -> Self {
        KeyResponse::default()
    }
}

#[derive(Clone)]
pub struct ErrorScreen {
    pub message: String,
    pub return_to_screen: Box<Screen>,
}

impl ErrorScreen {
    pub fn handle_key(&mut self, _key: KeyWithModifier) -> KeyResponse {
        KeyResponse::new_screen((*self.return_to_screen).clone())
    }

    pub fn render(&self, rows: usize, cols: usize) {
        if self.message.chars().count() > cols.saturating_sub(4) {
            let max_width = cols.saturating_sub(4);
            let max_rows = rows.saturating_sub(4);
            let lines = wrap_text_to_width(&self.message, max_width);
            let base_x = 2;
            let base_y = rows.saturating_sub(4 + lines.len()) / 2;
            MultiLineErrorMessage::new(lines).render(base_x, base_y, max_rows);
        } else {
            let desired_width = self.message.chars().count();
            let base_y = rows.saturating_sub(5) / 2;
            let base_x = cols.saturating_sub(desired_width) / 2;
            ErrorMessage::new(&self.message).render(base_x, base_y);
        }
    }
}

#[derive(Clone)]
pub struct ErrorDetailScreen {
    pub layout_name: String,
    pub detailed_error: String,
    pub return_to_screen: Box<Screen>,
}

impl ErrorDetailScreen {
    pub fn new(layout_name: String, detailed_error: String, return_to_screen: Box<Screen>) -> Self {
        Self {
            layout_name,
            detailed_error,
            return_to_screen,
        }
    }

    pub fn handle_key(&mut self, _key: KeyWithModifier) -> KeyResponse {
        KeyResponse::new_screen((*self.return_to_screen).clone())
    }

    pub fn render(&self, rows: usize, cols: usize) {
        let header = format!("Error in layout: {}", self.layout_name);
        let header_text = Text::new(&header).error_color_all();
        print_text_with_coordinates(header_text, 1, 0, None, None);

        let header_height = 2;
        let available_rows = rows.saturating_sub(header_height);
        let available_cols = cols.saturating_sub(2);
        let error_lines: Vec<&str> = self.detailed_error.lines().collect();
        let total_lines = error_lines.len();

        if total_lines <= available_rows {
            for (i, line) in error_lines.iter().enumerate() {
                let truncated = truncate_line_with_ansi(line, available_cols);
                print!("\u{1b}[{};{}H{}", header_height + i + 1, 2, truncated);
            }
            return;
        }

        let omitted_indicator_lines = 1;
        let lines_for_content = available_rows.saturating_sub(omitted_indicator_lines);
        let beginning_lines = (lines_for_content as f32 * 0.6).ceil() as usize;
        let end_lines = lines_for_content.saturating_sub(beginning_lines);
        let omitted_count = total_lines.saturating_sub(beginning_lines + end_lines);
        let mut current_row = 0;

        for line in error_lines.iter().take(beginning_lines) {
            let truncated = truncate_line_with_ansi(line, available_cols);
            print!(
                "\u{1b}[{};{}H{}",
                header_height + current_row + 1,
                2,
                truncated
            );
            current_row += 1;
        }

        let indicator = format!("... {} lines omitted ...", omitted_count);
        let indicator_text = Text::new(&indicator).color_range(0, ..);
        print_text_with_coordinates(
            indicator_text,
            (cols.saturating_sub(indicator.chars().count())) / 2,
            header_height + current_row,
            None,
            None,
        );
        current_row += 1;

        let start_index = total_lines.saturating_sub(end_lines);
        for line in error_lines.iter().skip(start_index) {
            let truncated = truncate_line_with_ansi(line, available_cols);
            print!(
                "\u{1b}[{};{}H{}",
                header_height + current_row + 1,
                2,
                truncated
            );
            current_row += 1;
        }
    }
}
