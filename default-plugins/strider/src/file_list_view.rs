use crate::platform::Platform;
use crate::shared::{calculate_list_bounds, refresh_directory, render_list_tip};
use pretty_bytes::converter::convert as pretty_bytes;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use unicode_width::UnicodeWidthStr;
use zellij_tile::prelude::*;

#[derive(Debug, Clone)]
pub struct FileListView {
    pub path: PathBuf,
    pub path_is_dir: bool,
    pub files: Vec<FsEntry>,
    pub cursor_hist: HashMap<PathBuf, usize>,
    pub platform: Platform,
    pub mode: FileListMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileListMode {
    #[default]
    Default,
    MarkdownNewest,
}

impl Default for FileListView {
    fn default() -> Self {
        FileListView {
            path_is_dir: true,
            path: PathBuf::new(),
            files: Default::default(),
            cursor_hist: Default::default(),
            platform: Platform::default(),
            mode: FileListMode::default(),
        }
    }
}

impl FileListView {
    pub fn descend_to_previous_path(&mut self) {
        if Platform::is_root(&self.path, self.platform) {
            return;
        }
        if let Some(parent) = self.path.parent() {
            self.path = Platform::ensure_drive_root(parent.to_path_buf(), self.platform);
        } else {
            self.path = PathBuf::new();
        }
        self.path_is_dir = true;
        self.files.clear();
        self.clear_selected();
        refresh_directory(&self.path);
    }

    pub fn descend_to_root_path(&mut self, initial_cwd: &Path) {
        self.path = initial_cwd.to_path_buf();
        self.path_is_dir = true;
        self.files.clear();
        self.clear_selected();
    }

    pub fn enter_dir(&mut self, entry: &FsEntry) {
        let is_dir = entry.is_folder();
        let path = entry.get_full_pathbuf();
        self.path = path;
        self.path_is_dir = is_dir;
        self.files.clear();
        self.clear_selected();
    }

    pub fn clear_selected(&mut self) {
        self.cursor_hist.remove(&self.path);
    }

    pub fn update_files(
        &mut self,
        paths: Vec<(PathBuf, Option<FileMetadata>)>,
        hide_hidden_files: bool,
    ) {
        if self.mode == FileListMode::MarkdownNewest {
            self.files = collect_markdown_files_newest_first(
                &markdown_scan_root(&self.path),
                &self.path,
                hide_hidden_files,
            );
            return;
        }

        let mut files = vec![];
        for (entry, entry_metadata) in paths {
            let entry = Platform::normalize(&entry);
            let entry = self
                .path
                .join(entry.strip_prefix("/host").unwrap_or(&entry));
            if entry_metadata.map(|e| e.is_symlink).unwrap_or(false) {
                continue;
            }
            let entry = if entry_metadata.map(|e| e.is_dir).unwrap_or(false) {
                FsEntry::Dir(entry)
            } else {
                let size = entry_metadata.map(|e| e.len).unwrap_or(0);
                FsEntry::File(entry, size)
            };
            if !entry.is_hidden_file() || !hide_hidden_files {
                files.push(entry);
            }
        }
        self.files = files;
        self.files.sort_unstable();
    }

    pub fn get_selected_entry(&self) -> Option<FsEntry> {
        self.selected().and_then(|f| self.files.get(f).cloned())
    }

    pub fn selected_mut(&mut self) -> &mut usize {
        self.cursor_hist.entry(self.path.clone()).or_default()
    }

    pub fn selected(&self) -> Option<usize> {
        self.cursor_hist.get(&self.path).copied()
    }

    pub fn move_selection_up(&mut self) {
        if let Some(selected) = self.selected() {
            *self.selected_mut() = selected.saturating_sub(1);
        }
    }

    pub fn move_selection_down(&mut self) {
        if let Some(selected) = self.selected() {
            let next = selected.saturating_add(1);
            *self.selected_mut() = std::cmp::min(self.files.len().saturating_sub(1), next);
        } else {
            *self.selected_mut() = 0;
        }
    }

    pub fn render(&mut self, rows: usize, cols: usize) {
        let (start_index, selected_index_in_range, end_index) =
            calculate_list_bounds(self.files.len(), rows.saturating_sub(1), self.selected());

        render_list_tip(3, cols);
        for i in start_index..end_index {
            if let Some(entry) = self.files.get(i) {
                let is_selected = Some(i) == selected_index_in_range;
                let mut file_or_folder_name = entry.name();
                let size = entry
                    .size()
                    .map(|s| pretty_bytes(s as f64))
                    .unwrap_or("".to_owned());
                if entry.is_folder() {
                    file_or_folder_name.push(self.platform.separator());
                }
                let file_or_folder_name_width = file_or_folder_name.width();
                let size_width = size.width();
                let text = if file_or_folder_name_width + size_width < cols {
                    let padding = " ".repeat(
                        cols.saturating_sub(file_or_folder_name_width)
                            .saturating_sub(size_width),
                    );
                    format!("{}{}{}", file_or_folder_name, padding, size)
                } else {
                    let padding = " ".repeat(cols.saturating_sub(file_or_folder_name_width));
                    format!("{}{}", file_or_folder_name, padding)
                };
                let mut text_element = if is_selected {
                    Text::new(text).selected()
                } else {
                    Text::new(text)
                };
                if entry.is_folder() {
                    text_element = text_element.color_range(0, ..);
                }
                print_text_with_coordinates(
                    text_element,
                    0,
                    4 + i.saturating_sub(start_index),
                    Some(cols),
                    None,
                );
            }
        }
    }
}

fn markdown_scan_root(path: &Path) -> PathBuf {
    #[cfg(target_family = "wasm")]
    {
        let _ = path;
        PathBuf::from("/host")
    }
    #[cfg(not(target_family = "wasm"))]
    {
        path.to_path_buf()
    }
}

fn collect_markdown_files_newest_first(
    scan_root: &Path,
    display_root: &Path,
    hide_hidden_files: bool,
) -> Vec<FsEntry> {
    let mut files = Vec::new();
    collect_markdown_files(
        scan_root,
        scan_root,
        display_root,
        hide_hidden_files,
        &mut files,
    );
    files.sort_by(|(a_entry, a_modified), (b_entry, b_modified)| {
        b_modified
            .cmp(a_modified)
            .then_with(|| a_entry.get_full_pathbuf().cmp(&b_entry.get_full_pathbuf()))
    });
    files.into_iter().map(|(entry, _modified)| entry).collect()
}

fn collect_markdown_files(
    scan_path: &Path,
    scan_root: &Path,
    display_root: &Path,
    hide_hidden_files: bool,
    files: &mut Vec<(FsEntry, Option<SystemTime>)>,
) {
    let Ok(entries) = std::fs::read_dir(scan_path) else {
        return;
    };
    for entry in entries.flatten() {
        let scan_path = entry.path();
        if hide_hidden_files && is_hidden_path(&scan_path) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_markdown_files(
                &scan_path,
                scan_root,
                display_root,
                hide_hidden_files,
                files,
            );
        } else if metadata.is_file() && scan_path.extension().and_then(|e| e.to_str()) == Some("md")
        {
            let display_path =
                display_root.join(scan_path.strip_prefix(scan_root).unwrap_or(&scan_path));
            files.push((
                FsEntry::File(display_path, metadata.len()),
                metadata.modified().ok(),
            ));
        }
    }
}

fn is_hidden_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.starts_with('.'))
        .unwrap_or(false)
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Debug)]
pub enum FsEntry {
    Dir(PathBuf),
    File(PathBuf, u64),
}

impl FsEntry {
    pub fn name(&self) -> String {
        let path = match self {
            FsEntry::Dir(p) => p,
            FsEntry::File(p, _) => p,
        };
        path.file_name().unwrap().to_string_lossy().into_owned()
    }

    pub fn size(&self) -> Option<u64> {
        match self {
            FsEntry::Dir(_p) => None,
            FsEntry::File(_, size) => Some(*size),
        }
    }

    pub fn get_full_pathbuf(&self) -> PathBuf {
        match self {
            FsEntry::Dir(p) => p.clone(),
            FsEntry::File(p, _) => p.clone(),
        }
    }

    pub fn is_hidden_file(&self) -> bool {
        self.name().starts_with('.')
    }

    pub fn is_folder(&self) -> bool {
        matches!(self, FsEntry::Dir(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

    fn unique_temp_dir(test_name: &str) -> PathBuf {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "strider-{}-{}-{}",
            test_name,
            std::process::id(),
            id
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn listed_relative_paths(view: &FileListView, root: &Path) -> Vec<String> {
        view.files
            .iter()
            .map(|entry| {
                entry
                    .get_full_pathbuf()
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn markdown_newest_mode_filters_markdown_recursively_and_sorts_newest_first() {
        let root = unique_temp_dir("markdown-newest");
        let old = root.join("2026_0614").join("old.md");
        let ignored = root.join("2026_0614").join("ignored.txt");
        let newest = root.join("2026_0615").join("newest.md");

        write_file(&old, "old");
        std::thread::sleep(Duration::from_millis(20));
        write_file(&ignored, "ignored");
        std::thread::sleep(Duration::from_millis(20));
        write_file(&newest, "newest");

        let mut view = FileListView {
            path: root.clone(),
            mode: FileListMode::MarkdownNewest,
            ..Default::default()
        };

        view.update_files(Vec::new(), true);

        assert_eq!(
            listed_relative_paths(&view, &root),
            vec!["2026_0615/newest.md", "2026_0614/old.md"]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn default_mode_keeps_existing_directory_entries_and_alphabetical_sorting() {
        let root = unique_temp_dir("default-mode");
        let mut view = FileListView {
            path: root.clone(),
            ..Default::default()
        };

        view.update_files(
            vec![
                (
                    PathBuf::from("/host/zeta.md"),
                    Some(FileMetadata {
                        is_dir: false,
                        is_file: true,
                        is_symlink: false,
                        len: 1,
                    }),
                ),
                (
                    PathBuf::from("/host/a.txt"),
                    Some(FileMetadata {
                        is_dir: false,
                        is_file: true,
                        is_symlink: false,
                        len: 2,
                    }),
                ),
            ],
            true,
        );

        assert_eq!(
            listed_relative_paths(&view, &root),
            vec!["a.txt", "zeta.md"]
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
