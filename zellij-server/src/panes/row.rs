use crate::panes::terminal_character::{AnsiCode, TerminalCharacter, EMPTY_TERMINAL_CHARACTER};
use std::{
    cmp::Ordering,
    collections::VecDeque,
    fmt::{self, Debug, Formatter},
};

#[derive(Clone)]
pub struct Row {
    pub columns: VecDeque<TerminalCharacter>,
    pub is_canonical: bool,
    width: Option<usize>,
    pub bg_color: Option<AnsiCode>,
}

impl Debug for Row {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for character in &self.columns {
            write!(f, "{:?}", character)?;
        }
        Ok(())
    }
}

impl Default for Row {
    fn default() -> Self {
        Self::new()
    }
}

impl Row {
    pub fn new() -> Self {
        Row {
            columns: VecDeque::new(),
            is_canonical: false,
            width: None,
            bg_color: None,
        }
    }
    pub fn from_columns(columns: VecDeque<TerminalCharacter>) -> Self {
        Row {
            columns,
            is_canonical: false,
            width: None,
            bg_color: None,
        }
    }
    pub fn from_rows(mut rows: Vec<Row>) -> Self {
        if rows.is_empty() {
            Row::new()
        } else {
            let mut first_row = rows.remove(0);
            for row in &mut rows {
                first_row.append(&mut row.columns);
            }
            first_row
        }
    }
    pub fn with_character(mut self, terminal_character: TerminalCharacter) -> Self {
        self.columns.push_back(terminal_character);
        self.width = None;
        self
    }
    pub fn canonical(mut self) -> Self {
        self.is_canonical = true;
        self
    }
    pub fn with_bg_color(mut self, bg_color: Option<AnsiCode>) -> Self {
        self.bg_color = bg_color;
        self
    }
    pub fn width_cached(&mut self) -> usize {
        if let Some(width) = self.width {
            width
        } else {
            let mut width = 0;
            for terminal_character in &self.columns {
                width += terminal_character.width();
            }
            self.width = Some(width);
            width
        }
    }
    pub fn width(&self) -> usize {
        let mut width = 0;
        for terminal_character in &self.columns {
            width += terminal_character.width();
        }
        width
    }
    pub fn excess_width(&self) -> usize {
        let mut acc = 0;
        for terminal_character in &self.columns {
            if terminal_character.width() > 1 {
                acc += terminal_character.width() - 1;
            }
        }
        acc
    }
    pub fn excess_width_until(&self, x: usize) -> usize {
        let mut acc = 0;
        for terminal_character in self.columns.iter().take(x) {
            if terminal_character.width() > 1 {
                acc += terminal_character.width() - 1;
            }
        }
        acc
    }
    pub fn absolute_character_index(&self, x: usize) -> usize {
        let mut absolute_index = x;
        for (i, terminal_character) in self.columns.iter().enumerate().take(x) {
            if i == absolute_index {
                break;
            }
            if terminal_character.width() > 1 {
                absolute_index = absolute_index.saturating_sub(1);
            }
        }
        absolute_index
    }
    pub fn absolute_character_index_and_position_in_char(&self, x: usize) -> (usize, usize) {
        let mut accumulated_width = 0;
        let mut absolute_index = x;
        let mut position_inside_character = 0;
        for (i, terminal_character) in self.columns.iter().enumerate() {
            accumulated_width += terminal_character.width();
            absolute_index = i;
            if accumulated_width > x {
                let character_start_position = accumulated_width - terminal_character.width();
                position_inside_character = x - character_start_position;
                break;
            }
        }
        (absolute_index, position_inside_character)
    }
    pub fn add_character_at(&mut self, terminal_character: TerminalCharacter, x: usize) {
        match self.width_cached().cmp(&x) {
            Ordering::Equal => {
                *self.width.as_mut().unwrap() += terminal_character.width();
                self.columns.push_back(terminal_character);
            },
            Ordering::Less => {
                let width_offset = self.excess_width_until(x);
                let mut gap_fill = EMPTY_TERMINAL_CHARACTER;
                if let Some(bg_color) = self.bg_color {
                    gap_fill
                        .styles
                        .update(|styles| styles.background = Some(bg_color));
                }
                self.columns
                    .resize(x.saturating_sub(width_offset), gap_fill);
                self.columns.push_back(terminal_character);
                self.width = None;
            },
            Ordering::Greater => {
                let (absolute_x_index, position_inside_character) =
                    self.absolute_character_index_and_position_in_char(x);
                let character_width = terminal_character.width();
                let replaced_character =
                    std::mem::replace(&mut self.columns[absolute_x_index], terminal_character);
                match character_width.cmp(&replaced_character.width()) {
                    Ordering::Greater => {
                        let position_to_remove = absolute_x_index + 1;
                        if let Some(removed) = self.columns.remove(position_to_remove) {
                            if removed.width() > 1 {
                                self.columns
                                    .insert(position_to_remove, EMPTY_TERMINAL_CHARACTER);
                            }
                        }
                    },
                    Ordering::Less => {
                        if position_inside_character > 0 {
                            self.columns
                                .insert(absolute_x_index, EMPTY_TERMINAL_CHARACTER);
                        } else {
                            self.columns
                                .insert(absolute_x_index + 1, EMPTY_TERMINAL_CHARACTER);
                        }
                    },
                    _ => {},
                }
                self.width = None;
            },
        }
    }
    pub fn insert_character_at(&mut self, terminal_character: TerminalCharacter, x: usize) {
        let insert_position = self.absolute_character_index(x);
        match self.columns.len().cmp(&insert_position) {
            Ordering::Equal => self.columns.push_back(terminal_character),
            Ordering::Less => {
                self.columns
                    .resize(insert_position, EMPTY_TERMINAL_CHARACTER);
                self.columns.push_back(terminal_character);
            },
            Ordering::Greater => {
                self.columns.insert(insert_position, terminal_character);
            },
        }
        self.width = None;
    }
    pub fn replace_character_at(&mut self, terminal_character: TerminalCharacter, x: usize) {
        let absolute_x_index = self.absolute_character_index(x);
        if let Some(character) = self.columns.get_mut(absolute_x_index) {
            let terminal_character_width = terminal_character.width();
            let character = std::mem::replace(character, terminal_character);
            let excess_width = character.width().saturating_sub(terminal_character_width);
            for _ in 0..excess_width {
                self.columns
                    .insert(absolute_x_index, EMPTY_TERMINAL_CHARACTER);
            }
        }
        self.width = None;
    }
    pub fn replace_columns(&mut self, columns: VecDeque<TerminalCharacter>) {
        self.columns = columns;
        self.width = None;
    }
    pub fn push(&mut self, terminal_character: TerminalCharacter) {
        self.columns.push_back(terminal_character);
        self.width = None;
    }
    pub fn truncate(&mut self, x: usize) {
        let width_offset = self.excess_width_until(x);
        let truncate_position = x.saturating_sub(width_offset);
        if truncate_position < self.columns.len() {
            self.columns.truncate(truncate_position);
        }
        self.width = None;
    }
    pub fn position_accounting_for_widechars(&self, x: usize) -> usize {
        let mut position = x;
        for (index, terminal_character) in self.columns.iter().enumerate() {
            if index == position {
                break;
            }
            if terminal_character.width() > 1 {
                position = position.saturating_sub(terminal_character.width().saturating_sub(1));
            }
        }
        position
    }
    pub fn replace_and_pad_end(
        &mut self,
        from: usize,
        to: usize,
        terminal_character: TerminalCharacter,
    ) {
        let from_position_accounting_for_widechars = self.position_accounting_for_widechars(from);
        let to_position_accounting_for_widechars = self.position_accounting_for_widechars(to);
        let replacement_length = to_position_accounting_for_widechars
            .saturating_sub(from_position_accounting_for_widechars);
        let mut replace_with = VecDeque::from(vec![terminal_character; replacement_length]);
        self.columns
            .truncate(from_position_accounting_for_widechars);
        self.columns.append(&mut replace_with);
        self.width = None;
    }
    pub fn append(&mut self, to_append: &mut VecDeque<TerminalCharacter>) {
        self.columns.append(to_append);
        self.width = None;
    }
    pub fn drain_until(&mut self, x: usize) -> VecDeque<TerminalCharacter> {
        let mut drained_part_len = 0;
        let mut split_pos = 0;
        for next_character in self.columns.iter() {
            if drained_part_len + next_character.width() <= x || drained_part_len == 0 {
                drained_part_len += next_character.width();
                split_pos += 1
            } else {
                break;
            }
        }
        let drained_part = self.columns.drain(..split_pos).collect();
        self.width = None;
        drained_part
    }
    pub fn replace_and_pad_beginning(&mut self, to: usize, terminal_character: TerminalCharacter) {
        let to_position_accounting_for_widechars = self.position_accounting_for_widechars(to);
        let width_of_current_character = self
            .columns
            .get(to_position_accounting_for_widechars)
            .map(|character| character.width())
            .unwrap_or(1);
        let mut replace_with =
            VecDeque::from(vec![terminal_character; to + width_of_current_character]);
        if to_position_accounting_for_widechars > self.columns.len() {
            self.columns.clear();
        } else if to_position_accounting_for_widechars >= self.columns.len() {
            drop(self.columns.drain(0..to_position_accounting_for_widechars));
        } else {
            drop(self.columns.drain(0..=to_position_accounting_for_widechars));
        }
        replace_with.append(&mut self.columns);
        self.width = None;
        self.columns = replace_with;
    }
    pub fn len(&self) -> usize {
        self.columns.len()
    }
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }
    pub fn delete_and_return_character(&mut self, x: usize) -> Option<TerminalCharacter> {
        let erase_position = self.absolute_character_index(x);
        if erase_position < self.columns.len() {
            self.width = None;
            self.columns.remove(erase_position)
        } else {
            None
        }
    }
    pub fn split_to_rows_of_length(&mut self, max_row_length: usize) -> Vec<Row> {
        let mut parts: Vec<Row> = vec![];
        let mut current_part: VecDeque<TerminalCharacter> = VecDeque::new();
        let mut current_part_len = 0;
        for character in self.columns.drain(..) {
            if current_part_len + character.width() > max_row_length {
                parts.push(Row::from_columns(current_part).with_bg_color(self.bg_color));
                current_part = VecDeque::new();
                current_part_len = 0;
            }
            current_part_len += character.width();
            current_part.push_back(character);
        }
        if !current_part.is_empty() {
            parts.push(Row::from_columns(current_part).with_bg_color(self.bg_color))
        };
        if !parts.is_empty() && self.is_canonical {
            if let Some(part) = parts.get_mut(0) {
                part.is_canonical = true;
            }
        }
        if parts.is_empty() {
            parts.push(self.clone());
        }
        self.width = None;
        parts
    }
    pub fn last_index_in_line(&self) -> usize {
        self.columns.len()
    }
    pub fn word_indices_around_character_index(&self, index: usize) -> Option<(usize, usize)> {
        let absolute_character_index = self.absolute_character_index(index);
        let character_at_index = self.columns.get(absolute_character_index)?;
        if is_selection_boundary_character(character_at_index.character) {
            return Some((index, index + 1));
        }
        let mut end_position = self
            .columns
            .iter()
            .enumerate()
            .skip(absolute_character_index)
            .find_map(|(i, t_c)| {
                if is_selection_boundary_character(t_c.character) {
                    Some(i + self.excess_width_until(i))
                } else {
                    None
                }
            })
            .unwrap_or_else(|| self.columns.len() + self.excess_width());
        let start_position = self
            .columns
            .iter()
            .enumerate()
            .take(absolute_character_index)
            .rev()
            .find_map(|(i, t_c)| {
                if is_selection_boundary_character(t_c.character) {
                    Some(i + 1 + self.excess_width_until(i))
                } else {
                    None
                }
            })
            .unwrap_or(0);
        if start_position == end_position {
            end_position += 1;
        }
        Some((start_position, end_position))
    }
    pub fn word_start_index_of_last_character(&self) -> usize {
        self.columns
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, t_c)| {
                if is_selection_boundary_character(t_c.character) {
                    Some(self.absolute_character_index(i + 1))
                } else {
                    None
                }
            })
            .unwrap_or(0)
    }
    pub fn word_end_index_of_first_character(&self) -> usize {
        self.columns
            .iter()
            .enumerate()
            .find_map(|(i, t_c)| {
                if is_selection_boundary_character(t_c.character) {
                    Some(self.absolute_character_index(i))
                } else {
                    None
                }
            })
            .unwrap_or(self.columns.len())
    }
}

fn is_selection_boundary_character(character: char) -> bool {
    character.is_ascii_whitespace()
        || character == '['
        || character == ']'
        || character == '{'
        || character == '}'
        || character == '<'
        || character == '>'
        || character == '('
        || character == ')'
}
