pub fn range_to_render(
    table_rows: usize,
    results_len: usize,
    selected_index: Option<usize>,
) -> (usize, usize) {
    let data_rows = table_rows.saturating_sub(1); // 1 for the title
    if data_rows >= results_len {
        (0, results_len)
    } else {
        let anchor = selected_index.unwrap_or(0);
        let half = data_rows / 2;
        let mut s = anchor.saturating_sub(half);
        let mut e = s + data_rows;
        if e > results_len {
            e = results_len;
            s = results_len.saturating_sub(data_rows);
        }
        (s, e)
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
