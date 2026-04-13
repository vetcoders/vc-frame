use humantime::format_duration;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use zellij_tile::prelude::*;

use crate::single_screen_data::UnifiedSearchResult;
use crate::ui::components::{compute_reduction_tier, Colors};

#[derive(Clone)]
pub struct CachedRowData {
    pub session_name: String,
    pub indices: Vec<usize>,
    pub original_index: usize,
    pub kind: CachedRowKind,
    pub full_details: String,
    pub abbr_details: String,
    pub full_tag: &'static str,
    pub abbr_tag: &'static str,
    pub name_width: usize,
    pub full_details_width: usize,
    pub abbr_details_width: usize,
    pub full_tag_width: usize,
    pub details_color_ranges: DetailsColorRanges,
    pub abbr_details_color_ranges: DetailsColorRanges,
}

#[derive(Clone)]
pub enum CachedRowKind {
    Active,
    Resurrectable,
}

#[derive(Clone, Default)]
pub struct DetailsColorRanges {
    pub ranges: Vec<(usize, std::ops::Range<usize>)>,
}

#[derive(Default)]
pub struct UnifiedResultsRenderCache {
    pub rows: Vec<CachedRowData>,
    pub full_name_width: usize,
    pub full_details_width: usize,
    pub abbr_details_width: usize,
    pub full_tag_width: usize,
}

impl UnifiedResultsRenderCache {
    pub fn rebuild(&mut self, results: &[UnifiedSearchResult]) {
        self.rows.clear();
        self.full_name_width = 0;
        self.full_details_width = 0;
        self.abbr_details_width = 0;
        self.full_tag_width = 0;

        for (orig_i, result) in results.iter().enumerate() {
            let is_current = matches!(
                result,
                UnifiedSearchResult::ActiveSession {
                    is_current_session: true,
                    ..
                }
            );
            if is_current {
                continue;
            }

            let row = match result {
                UnifiedSearchResult::ActiveSession {
                    indices,
                    session_name,
                    connected_users,
                    tab_count,
                    pane_count,
                    ..
                } => {
                    let client_word = if *connected_users == 1 {
                        "client"
                    } else {
                        "clients"
                    };
                    let tab_str = format!("{tab_count}");
                    let pane_str = format!("{pane_count}");
                    let conn_str = format!("{connected_users}");
                    let full_details =
                        format!("{tab_str} tabs, {pane_str} panes, {conn_str} {client_word}");
                    let full_details_ranges = {
                        let tab_end = tab_str.len();
                        let pane_offset = tab_str.len() + " tabs, ".len();
                        let pane_end = pane_offset + pane_str.len();
                        let conn_offset = pane_end + " panes, ".len();
                        let conn_end = conn_offset + conn_str.len();
                        DetailsColorRanges {
                            ranges: vec![
                                (1, 0..tab_end),
                                (2, pane_offset..pane_end),
                                (2, conn_offset..conn_end),
                            ],
                        }
                    };
                    let abbr_details = format!("{tab_str}t, {pane_str}p, {conn_str}c");
                    let abbr_details_ranges = {
                        let tab_end = tab_str.len();
                        let pane_offset = tab_str.len() + "t, ".len();
                        let pane_end = pane_offset + pane_str.len();
                        let conn_offset = pane_end + "p, ".len();
                        let conn_end = conn_offset + conn_str.len();
                        DetailsColorRanges {
                            ranges: vec![
                                (1, 0..tab_end),
                                (2, pane_offset..pane_end),
                                (2, conn_offset..conn_end),
                            ],
                        }
                    };

                    CachedRowData {
                        session_name: session_name.clone(),
                        indices: indices.clone(),
                        original_index: orig_i,
                        kind: CachedRowKind::Active,
                        full_details_width: full_details.width(),
                        abbr_details_width: abbr_details.width(),
                        name_width: session_name.width(),
                        full_tag_width: "[ATTACH]".len(),
                        full_details,
                        abbr_details,
                        full_tag: "[ATTACH]",
                        abbr_tag: "[A]",
                        details_color_ranges: full_details_ranges,
                        abbr_details_color_ranges: abbr_details_ranges,
                    }
                },
                UnifiedSearchResult::ResurrectableSession {
                    indices,
                    session_name,
                    ctime,
                    ..
                } => {
                    let duration = format_duration(*ctime).to_string();
                    let mut formatted_duration = String::new();
                    for part in duration.split_whitespace() {
                        if !part.ends_with('s') {
                            if !formatted_duration.is_empty() {
                                formatted_duration.push(' ');
                            }
                            formatted_duration.push_str(part);
                        }
                    }
                    if formatted_duration.is_empty() {
                        formatted_duration.push_str("<1m");
                    }
                    let full_details = format!("Created {formatted_duration} ago");
                    let full_details_ranges = {
                        let created_len = "Created ".len();
                        let duration_end = created_len + formatted_duration.len();
                        DetailsColorRanges {
                            ranges: vec![(2, created_len..duration_end)],
                        }
                    };
                    let abbr_details = format!("{formatted_duration} ago");
                    let abbr_details_ranges = DetailsColorRanges {
                        ranges: vec![(2, 0..formatted_duration.len())],
                    };

                    CachedRowData {
                        session_name: session_name.clone(),
                        indices: indices.clone(),
                        original_index: orig_i,
                        kind: CachedRowKind::Resurrectable,
                        full_details_width: full_details.width(),
                        abbr_details_width: abbr_details.width(),
                        name_width: session_name.width(),
                        full_tag_width: "[RESURRECT]".len(),
                        full_details,
                        abbr_details,
                        full_tag: "[RESURRECT]",
                        abbr_tag: "[R]",
                        details_color_ranges: full_details_ranges,
                        abbr_details_color_ranges: abbr_details_ranges,
                    }
                },
            };

            self.full_name_width = self.full_name_width.max(row.name_width);
            self.full_details_width = self.full_details_width.max(row.full_details_width);
            self.abbr_details_width = self.abbr_details_width.max(row.abbr_details_width);
            self.full_tag_width = self.full_tag_width.max(row.full_tag_width);
            self.rows.push(row);
        }
    }
}

pub fn render_unified_results(
    cache: &UnifiedResultsRenderCache,
    selected_index: Option<usize>,
    max_rows: usize,
    max_cols: usize,
    _colors: Colors,
    x: usize,
    y: usize,
) {
    if cache.rows.is_empty() {
        return;
    }

    let filtered_selected =
        selected_index.and_then(|sel| cache.rows.iter().position(|r| r.original_index == sel));

    let total = cache.rows.len();
    let data_rows = max_rows.saturating_sub(1);
    let (start, end) = if data_rows >= total {
        (0, total)
    } else {
        let anchor = filtered_selected.unwrap_or(0);
        let half = data_rows / 2;
        let mut s = anchor.saturating_sub(half);
        let mut e = s + data_rows;
        if e > total {
            e = total;
            s = total.saturating_sub(data_rows);
        }
        (s, e)
    };

    let (above_active, above_resurrectable) = count_by_kind(&cache.rows[..start]);
    let (below_active, below_resurrectable) = count_by_kind(&cache.rows[end..]);
    let has_hidden_above = above_active > 0 || above_resurrectable > 0;
    let has_hidden_below = below_active > 0 || below_resurrectable > 0;
    let has_hidden = has_hidden_above || has_hidden_below;
    let tab_header_full = "<TAB> Complete";
    let tab_header_short = "<TAB>";

    let above_summary_full = if has_hidden_above {
        format!("[+{above_active} Active] [+{above_resurrectable} Exited]")
    } else {
        String::new()
    };
    let above_summary_short = if has_hidden_above {
        format!("[+{above_active}] [+{above_resurrectable}]")
    } else {
        String::new()
    };
    let below_summary_full = if has_hidden_below {
        format!("[+{below_active} Active] [+{below_resurrectable} Exited]")
    } else {
        String::new()
    };
    let below_summary_short = if has_hidden_below {
        format!("[+{below_active}] [+{below_resurrectable}]")
    } else {
        String::new()
    };

    let max_summary_full_width =
        std::cmp::max(above_summary_full.width(), below_summary_full.width());
    let max_summary_short_width =
        std::cmp::max(above_summary_short.width(), below_summary_short.width());
    let full_fourth_col_width = std::cmp::max(
        tab_header_full.width(),
        if has_hidden {
            max_summary_full_width
        } else {
            1
        },
    );
    let short_fourth_col_width = std::cmp::max(
        tab_header_short.width(),
        if has_hidden {
            max_summary_short_width
        } else {
            1
        },
    );

    let (abbreviate_details, abbreviate_tags, abbreviate_fourth_col, name_max_width) =
        compute_reduction_tier(
            cache.full_name_width,
            cache.full_details_width,
            cache.full_tag_width,
            full_fourth_col_width,
            cache.abbr_details_width,
            short_fourth_col_width,
            max_cols,
        );

    let mut table = Table::new().add_styled_row(vec![
        Text::new(" "),
        Text::new(" "),
        Text::new(" "),
        Text::new(" "),
    ]);

    let visible_count = end - start;
    for (row_index, row) in cache.rows[start..end].iter().enumerate() {
        let is_selected = filtered_selected == Some(start + row_index);
        let display_name = match name_max_width {
            Some(max_w) => truncate_to_width(&row.session_name, max_w),
            None => row.session_name.clone(),
        };
        let display_indices: Vec<usize> = row
            .indices
            .iter()
            .filter(|&&i| i < display_name.chars().count())
            .cloned()
            .collect();

        let mut name_cell = Text::new(display_name).color_range(1, ..);
        if !display_indices.is_empty() {
            name_cell = name_cell.color_indices(3, display_indices);
        }

        let color_ranges = if abbreviate_details {
            &row.abbr_details_color_ranges
        } else {
            &row.details_color_ranges
        };
        let details_text = if abbreviate_details {
            &row.abbr_details
        } else {
            &row.full_details
        };
        let mut details_cell = Text::new(details_text);
        for (color_idx, range) in &color_ranges.ranges {
            details_cell = details_cell.color_range(*color_idx, range.clone());
        }

        let tag_text = if abbreviate_tags {
            row.abbr_tag
        } else {
            row.full_tag
        };
        let tag_cell = Text::new(tag_text).color_range(0, ..);

        let fourth_cell = if row_index == 0 && has_hidden_above {
            let (summary_text, active_count, resurrectable_count) = if abbreviate_fourth_col {
                (&above_summary_short, above_active, above_resurrectable)
            } else {
                (&above_summary_full, above_active, above_resurrectable)
            };
            Text::new(summary_text)
                .color_substring(2, &format!("+{active_count}"))
                .color_substring(2, &format!("+{resurrectable_count}"))
        } else if row_index == 0 && selected_index.is_none() {
            let tab_hint_text = if abbreviate_fourth_col {
                tab_header_short
            } else {
                tab_header_full
            };
            Text::new(tab_hint_text).color_substring(3, "<TAB>")
        } else if row_index == visible_count - 1 && has_hidden_below {
            let (summary_text, active_count, resurrectable_count) = if abbreviate_fourth_col {
                (&below_summary_short, below_active, below_resurrectable)
            } else {
                (&below_summary_full, below_active, below_resurrectable)
            };
            Text::new(summary_text)
                .color_substring(2, &format!("+{active_count}"))
                .color_substring(2, &format!("+{resurrectable_count}"))
        } else {
            Text::new(" ")
        };

        table = if is_selected {
            table.add_styled_row(vec![
                name_cell.selected(),
                details_cell.selected(),
                tag_cell.selected(),
                fourth_cell,
            ])
        } else {
            table.add_styled_row(vec![name_cell, details_cell, tag_cell, fourth_cell])
        };
    }

    print_table_with_coordinates(table, x, y, Some(max_cols), Some(max_rows));
}

fn count_by_kind(rows: &[CachedRowData]) -> (usize, usize) {
    let mut active = 0;
    let mut resurrectable = 0;
    for row in rows {
        match row.kind {
            CachedRowKind::Active => active += 1,
            CachedRowKind::Resurrectable => resurrectable += 1,
        }
    }
    (active, resurrectable)
}

fn truncate_to_width(text: &str, max_width: usize) -> String {
    let mut result = String::new();
    let mut current_width = 0;
    for ch in text.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if current_width + ch_width > max_width {
            break;
        }
        result.push(ch);
        current_width += ch_width;
    }
    result
}
