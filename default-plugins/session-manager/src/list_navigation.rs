pub fn range_to_render(
    table_rows: usize,
    results_len: usize,
    selected_index: Option<usize>,
) -> (usize, usize) {
    if table_rows <= results_len {
        let row_count_to_render = table_rows.saturating_sub(1); // 1 for the title
        let first_row_index_to_render = selected_index
            .unwrap_or(0)
            .saturating_sub(row_count_to_render / 2);
        let last_row_index_to_render = first_row_index_to_render + row_count_to_render;
        (first_row_index_to_render, last_row_index_to_render)
    } else {
        (0, results_len)
    }
}

pub fn move_wrapping_selection_up(selected_index: &mut Option<usize>, results_len: usize) {
    if results_len == 0 {
        *selected_index = None;
        return;
    }
    *selected_index = Some(match selected_index {
        Some(0) | None => results_len.saturating_sub(1),
        Some(selected_index) => selected_index.saturating_sub(1),
    });
}

pub fn move_wrapping_selection_down(selected_index: &mut Option<usize>, results_len: usize) {
    if results_len == 0 {
        *selected_index = None;
        return;
    }
    *selected_index = Some(match selected_index {
        Some(selected_index) if *selected_index + 1 < results_len => *selected_index + 1,
        Some(_) | None => 0,
    });
}

pub fn move_wrapping_index_up(selected_index: &mut usize, results_len: usize) {
    if results_len == 0 {
        *selected_index = 0;
        return;
    }
    if *selected_index == 0 {
        *selected_index = results_len.saturating_sub(1);
    } else {
        *selected_index = selected_index.saturating_sub(1);
    }
}

pub fn move_wrapping_index_down(selected_index: &mut usize, results_len: usize) {
    if results_len == 0 {
        *selected_index = 0;
        return;
    }
    if *selected_index + 1 < results_len {
        *selected_index += 1;
    } else {
        *selected_index = 0;
    }
}
