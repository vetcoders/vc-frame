use super::PluginInstruction;
use std::path::PathBuf;

use crate::thread_bus::ThreadSenders;
use std::path::Path;
use std::time::Duration;

use notify_debouncer_full::{
    DebounceEventResult, Debouncer, FileIdMap, new_debouncer,
    notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher},
};
use zellij_utils::{data::Event, errors::prelude::Result};

const DEBOUNCE_DURATION_MS: u64 = 400;

/// Directory names whose events never reach plugins. The watcher observes the
/// whole session cwd recursively; a `cargo build` or `npm install` inside it
/// emits tens of thousands of events under these trees per burst, and every
/// debounce batch fans out to every plugin. Plugins that genuinely browse
/// build artifacts lose live refresh under these dirs — an acceptable trade
/// against melting the plugin thread on every compile.
const IGNORED_DIR_NAMES: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "__pycache__",
    ".venv",
    "venv",
];

fn path_is_ignored(path: &Path, root: &Path) -> bool {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative.components().any(|component| {
        matches!(
            component,
            std::path::Component::Normal(name)
                if IGNORED_DIR_NAMES.iter().any(|ignored| name == *ignored)
        )
    })
}

pub fn watch_filesystem(
    senders: ThreadSenders,
    zellij_cwd: &Path,
) -> Result<Debouncer<RecommendedWatcher, FileIdMap>> {
    let path_prefix_in_plugins = PathBuf::from("/host");
    let current_dir = PathBuf::from(zellij_cwd);
    let mut debouncer = new_debouncer(
        Duration::from_millis(DEBOUNCE_DURATION_MS),
        None,
        move |result: DebounceEventResult| match result {
            Ok(events) => {
                let mut create_events = vec![];
                let mut read_events = vec![];
                let mut update_events = vec![];
                let mut delete_events = vec![];
                for event in events {
                    if event
                        .paths
                        .iter()
                        .all(|path| path_is_ignored(path, &current_dir))
                    {
                        continue;
                    }
                    match event.kind {
                        EventKind::Access(_) => read_events.push(event),
                        EventKind::Create(_) => create_events.push(event),
                        EventKind::Modify(_) => update_events.push(event),
                        EventKind::Remove(_) => delete_events.push(event),
                        _ => {},
                    }
                }
                let create_paths: Vec<PathBuf> = create_events
                    .drain(..)
                    .map(|e| {
                        e.paths
                            .iter()
                            .map(|p| {
                                let stripped_prefix_path =
                                    p.strip_prefix(&current_dir).unwrap_or_else(|_| p);
                                path_prefix_in_plugins.join(stripped_prefix_path)
                            })
                            .collect()
                    })
                    .collect();
                let read_paths: Vec<PathBuf> = read_events
                    .drain(..)
                    .map(|e| {
                        e.paths
                            .iter()
                            .map(|p| {
                                let stripped_prefix_path =
                                    p.strip_prefix(&current_dir).unwrap_or_else(|_| p);
                                path_prefix_in_plugins.join(stripped_prefix_path)
                            })
                            .collect()
                    })
                    .collect();
                let update_paths: Vec<PathBuf> = update_events
                    .drain(..)
                    .map(|e| {
                        e.paths
                            .iter()
                            .map(|p| {
                                let stripped_prefix_path =
                                    p.strip_prefix(&current_dir).unwrap_or_else(|_| p);
                                path_prefix_in_plugins.join(stripped_prefix_path)
                            })
                            .collect()
                    })
                    .collect();
                let delete_paths: Vec<PathBuf> = delete_events
                    .drain(..)
                    .map(|e| {
                        e.paths
                            .iter()
                            .map(|p| {
                                let stripped_prefix_path =
                                    p.strip_prefix(&current_dir).unwrap_or_else(|_| p);
                                path_prefix_in_plugins.join(stripped_prefix_path)
                            })
                            .collect()
                    })
                    .collect();
                // TODO: at some point we might want to add FileMetadata to these, but right now
                // the API is a bit unstable, so let's not rock the boat too much by adding another
                // expensive syscall
                if create_paths.is_empty()
                    && read_paths.is_empty()
                    && update_paths.is_empty()
                    && delete_paths.is_empty()
                {
                    // Everything in this batch was ignored — don't fan an
                    // empty four-event Update out to every plugin.
                    return;
                }
                let _ = senders.send_to_plugin(PluginInstruction::Update(vec![
                    (
                        None,
                        None,
                        Event::FileSystemRead(read_paths.into_iter().map(|p| (p, None)).collect()),
                    ),
                    (
                        None,
                        None,
                        Event::FileSystemCreate(
                            create_paths.into_iter().map(|p| (p, None)).collect(),
                        ),
                    ),
                    (
                        None,
                        None,
                        Event::FileSystemUpdate(
                            update_paths.into_iter().map(|p| (p, None)).collect(),
                        ),
                    ),
                    (
                        None,
                        None,
                        Event::FileSystemDelete(
                            delete_paths.into_iter().map(|p| (p, None)).collect(),
                        ),
                    ),
                ]));
            },
            Err(errors) => errors
                .iter()
                .for_each(|error| log::error!("watch error: {error:?}")),
        },
    )?;

    debouncer
        .watcher()
        .watch(zellij_cwd, RecursiveMode::Recursive)?;
    Ok(debouncer)
}
