use crate::ClientId;
use crate::panes::PaneId;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use zellij_utils::common_path::common_path_all;
use zellij_utils::pane_size::PaneGeom;
use zellij_utils::{
    data::{LayoutMetadata, PaneMetadata, TabMetadata},
    input::command::RunCommand,
    input::layout::{Layout, Run, RunPlugin, RunPluginOrAlias},
    input::plugins::PluginAliases,
    session_serialization::{
        GlobalLayoutManifest, PaneLayoutManifest, TabLayoutManifest, extract_command_and_args,
        extract_edit_and_line_number, extract_plugin_and_config,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListClientCommandStatus {
    /// PTY returned a current process command for this focused terminal.
    Confirmed,
    /// PTY was queried (or the shared deadline expired) and did not confirm.
    Unavailable,
    /// Plugin identity from Screen; there is no PTY process to confirm.
    Identity,
}

#[derive(Default, Debug, Clone)]
pub struct SessionLayoutMetadata {
    default_layout: Box<Layout>,
    global_cwd: Option<PathBuf>,
    pub default_shell: Option<PathBuf>,
    pub default_editor: Option<PathBuf>,
    tabs: Vec<TabLayoutMetadata>,
    list_client_unconfirmed_terminals: HashSet<u32>,
}

impl SessionLayoutMetadata {
    pub fn new(default_layout: Box<Layout>) -> Self {
        SessionLayoutMetadata {
            default_layout,
            ..Default::default()
        }
    }
    pub fn update_default_shell(&mut self, default_shell: PathBuf) {
        if self.default_shell.is_none() {
            self.default_shell = Some(default_shell);
        }
        for tab in self.tabs.iter_mut() {
            for tiled_pane in tab.tiled_panes.iter_mut() {
                if let Some(Run::Command(run_command)) = tiled_pane.run.as_mut()
                    && Self::is_default_shell(
                        self.default_shell.as_ref(),
                        &run_command.command.display().to_string(),
                        &run_command.args,
                    )
                {
                    tiled_pane.run = None;
                }
            }
            for floating_pane in tab.floating_panes.iter_mut() {
                if let Some(Run::Command(run_command)) = floating_pane.run.as_mut()
                    && Self::is_default_shell(
                        self.default_shell.as_ref(),
                        &run_command.command.display().to_string(),
                        &run_command.args,
                    )
                {
                    floating_pane.run = None;
                }
            }
        }
    }
    fn list_clients_visible_panes(&self) -> impl Iterator<Item = &PaneLayoutMetadata> {
        self.tabs.iter().flat_map(|tab| {
            let panes = if tab.hide_floating_panes {
                tab.tiled_panes.as_slice()
            } else {
                tab.floating_panes.as_slice()
            };
            panes.iter()
        })
    }
    pub fn list_clients_metadata(&self) -> String {
        let mut clients_metadata: BTreeMap<ClientId, ClientMetadata> = BTreeMap::new();
        for pane in self.list_clients_visible_panes() {
            for focused_client in &pane.focused_clients {
                clients_metadata.insert(
                    *focused_client,
                    ClientMetadata {
                        pane_id: pane.id,
                        command: pane.run.clone(),
                        command_status: self.list_client_command_status(pane),
                    },
                );
            }
        }

        ClientMetadata::render_many(clients_metadata, &self.default_editor)
    }
    pub fn all_clients_metadata(&self) -> BTreeMap<ClientId, ClientMetadata> {
        let mut clients_metadata: BTreeMap<ClientId, ClientMetadata> = BTreeMap::new();
        for pane in self.list_clients_visible_panes() {
            for focused_client in &pane.focused_clients {
                clients_metadata.insert(
                    *focused_client,
                    ClientMetadata {
                        pane_id: pane.id,
                        command: pane.run.clone(),
                        // Plugin event path still reports Screen identity; CLI
                        // honesty for unconfirmed PTY lives in list_clients_metadata.
                        command_status: ListClientCommandStatus::Confirmed,
                    },
                );
            }
        }
        clients_metadata
    }
    fn list_client_command_status(&self, pane: &PaneLayoutMetadata) -> ListClientCommandStatus {
        match pane.id {
            PaneId::Plugin(_) => ListClientCommandStatus::Identity,
            PaneId::Terminal(terminal_id)
                if self
                    .list_client_unconfirmed_terminals
                    .contains(&terminal_id) =>
            {
                ListClientCommandStatus::Unavailable
            },
            PaneId::Terminal(_) => ListClientCommandStatus::Confirmed,
        }
    }
    pub fn focused_list_client_terminal_ids(&self) -> Vec<u32> {
        let mut seen = HashSet::new();
        let mut ids = Vec::new();
        for pane in self.list_clients_visible_panes() {
            if pane.focused_clients.is_empty() {
                continue;
            }
            if let PaneId::Terminal(terminal_id) = pane.id
                && seen.insert(terminal_id)
            {
                ids.push(terminal_id);
            }
        }
        ids
    }
    pub fn mark_list_client_terminal_unconfirmed(&mut self, terminal_id: u32) {
        self.list_client_unconfirmed_terminals.insert(terminal_id);
    }
    pub fn clear_list_client_unconfirmed_terminals(&mut self) {
        self.list_client_unconfirmed_terminals.clear();
    }
    fn is_default_shell(
        default_shell: Option<&PathBuf>,
        command_name: &str,
        args: &[String],
    ) -> bool {
        default_shell
            .as_ref()
            .map(|c| c.display().to_string())
            .as_deref()
            == Some(command_name)
            && args.is_empty()
    }
}

impl SessionLayoutMetadata {
    pub fn add_tab(
        &mut self,
        name: String,
        tab_instance_id: String,
        is_focused: bool,
        hide_floating_panes: bool,
        tiled_panes: Vec<PaneLayoutMetadata>,
        floating_panes: Vec<PaneLayoutMetadata>,
    ) {
        self.tabs.push(TabLayoutMetadata {
            name: Some(name),
            tab_instance_id,
            is_focused,
            hide_floating_panes,
            tiled_panes,
            floating_panes,
        })
    }
    pub fn all_terminal_ids(&self) -> Vec<u32> {
        let mut terminal_ids = vec![];
        for tab in &self.tabs {
            for pane_layout_metadata in &tab.tiled_panes {
                if let PaneId::Terminal(id) = pane_layout_metadata.id {
                    terminal_ids.push(id);
                }
            }
            for pane_layout_metadata in &tab.floating_panes {
                if let PaneId::Terminal(id) = pane_layout_metadata.id {
                    terminal_ids.push(id);
                }
            }
        }
        terminal_ids
    }
    pub fn all_plugin_ids(&self) -> Vec<u32> {
        let mut plugin_ids = vec![];
        for tab in &self.tabs {
            for pane_layout_metadata in &tab.tiled_panes {
                if let PaneId::Plugin(id) = pane_layout_metadata.id {
                    plugin_ids.push(id);
                }
            }
            for pane_layout_metadata in &tab.floating_panes {
                if let PaneId::Plugin(id) = pane_layout_metadata.id {
                    plugin_ids.push(id);
                }
            }
        }
        plugin_ids
    }
    /// Plugin panes whose metadata carries no `run` identity at all. Only these
    /// lose information when the WASM bridge cannot resolve their command —
    /// parked or not-yet-activated chrome keeps the pane's own `invoked_with`.
    pub fn plugin_ids_missing_run(&self) -> HashSet<u32> {
        let mut plugin_ids = HashSet::new();
        for tab in &self.tabs {
            for pane_layout_metadata in tab.tiled_panes.iter().chain(tab.floating_panes.iter()) {
                if let PaneId::Plugin(id) = pane_layout_metadata.id
                    && pane_layout_metadata.run.is_none()
                {
                    plugin_ids.insert(id);
                }
            }
        }
        plugin_ids
    }
    pub fn remove_plugin_from_layout(&mut self, plugin_id_to_remove: u32) {
        for tab in &mut self.tabs {
            // Filter tiled panes
            tab.tiled_panes.retain(|pane| {
                if let PaneId::Plugin(id) = pane.id {
                    id != plugin_id_to_remove
                } else {
                    true
                }
            });

            // Filter floating panes
            tab.floating_panes.retain(|pane| {
                if let PaneId::Plugin(id) = pane.id {
                    id != plugin_id_to_remove
                } else {
                    true
                }
            });
        }
    }
    pub fn update_terminal_commands(
        &mut self,
        mut terminal_ids_to_commands: HashMap<u32, Vec<String>>,
    ) {
        let mut update_cmd_in_pane_metadata = |pane_layout_metadata: &mut PaneLayoutMetadata| {
            if let PaneId::Terminal(id) = pane_layout_metadata.id
                && let Some(command) = terminal_ids_to_commands.remove(&id)
            {
                let mut command_line = command.iter();
                if let Some(command_name) = command_line.next() {
                    let args: Vec<String> = command_line.map(|c| c.to_owned()).collect();
                    if Self::is_default_shell(self.default_shell.as_ref(), command_name, &args) {
                        pane_layout_metadata.run = None;
                    } else {
                        let mut run_command = RunCommand::new(PathBuf::from(command_name));
                        run_command.args = args;
                        pane_layout_metadata.run = Some(Run::Command(run_command));
                    }
                }
            }
        };
        for tab in self.tabs.iter_mut() {
            for pane_layout_metadata in tab.tiled_panes.iter_mut() {
                update_cmd_in_pane_metadata(pane_layout_metadata);
            }
            for pane_layout_metadata in tab.floating_panes.iter_mut() {
                update_cmd_in_pane_metadata(pane_layout_metadata);
            }
        }
    }
    pub fn update_terminal_cwds(&mut self, mut terminal_ids_to_cwds: HashMap<u32, PathBuf>) {
        if let Some(common_path_between_cwds) =
            common_path_all(terminal_ids_to_cwds.values().map(|p| p.as_path()))
        {
            terminal_ids_to_cwds.values_mut().for_each(|p| {
                if let Ok(stripped) = p.strip_prefix(&common_path_between_cwds) {
                    *p = PathBuf::from(stripped)
                }
            });
            self.global_cwd = Some(common_path_between_cwds);
        }
        let mut update_cwd_in_pane_metadata = |pane_layout_metadata: &mut PaneLayoutMetadata| {
            if let PaneId::Terminal(id) = pane_layout_metadata.id
                && let Some(cwd) = terminal_ids_to_cwds.remove(&id)
            {
                pane_layout_metadata.cwd = Some(cwd);
            }
        };
        for tab in self.tabs.iter_mut() {
            for pane_layout_metadata in tab.tiled_panes.iter_mut() {
                update_cwd_in_pane_metadata(pane_layout_metadata);
            }
            for pane_layout_metadata in tab.floating_panes.iter_mut() {
                update_cwd_in_pane_metadata(pane_layout_metadata);
            }
        }
    }
    pub fn update_plugin_cmds(&mut self, mut plugin_ids_to_run_plugins: HashMap<u32, RunPlugin>) {
        let mut update_cmd_in_pane_metadata = |pane_layout_metadata: &mut PaneLayoutMetadata| {
            if let PaneId::Plugin(id) = pane_layout_metadata.id
                && let Some(run_plugin) = plugin_ids_to_run_plugins.remove(&id)
            {
                pane_layout_metadata.run =
                    Some(Run::Plugin(RunPluginOrAlias::RunPlugin(run_plugin)));
            }
        };
        for tab in self.tabs.iter_mut() {
            for pane_layout_metadata in tab.tiled_panes.iter_mut() {
                update_cmd_in_pane_metadata(pane_layout_metadata);
            }
            for pane_layout_metadata in tab.floating_panes.iter_mut() {
                update_cmd_in_pane_metadata(pane_layout_metadata);
            }
        }
    }
    pub fn update_default_editor(&mut self, default_editor: &Option<PathBuf>) {
        let default_editor = default_editor.clone().unwrap_or_else(|| {
            PathBuf::from(
                std::env::var("EDITOR")
                    .unwrap_or_else(|_| std::env::var("VISUAL").unwrap_or_else(|_| "vi".into())),
            )
        });
        self.default_editor = Some(default_editor);
    }
    pub fn detect_editor_panes(&mut self) {
        let default_editor = match &self.default_editor {
            Some(e) => e.clone(),
            None => return,
        };
        let editor_binary_name = default_editor
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        let is_vim_family = |name: &str| matches!(name, "vim" | "nvim" | "emacs" | "nano" | "kak");
        let is_helix = |name: &str| matches!(name, "hx" | "helix");
        // Narrow vi/vim lineage used for cross-matching.
        // These are argument-compatible and commonly aliased to one another.
        let is_vi_vim = |name: &str| matches!(name, "vi" | "vim" | "nvim");

        let configured_is_vi_vim = is_vi_vim(&editor_binary_name);
        let configured_is_helix = is_helix(&editor_binary_name);

        let upgrade_pane = |pane: &mut PaneLayoutMetadata| {
            if let Some(Run::Command(run_command)) = &pane.run {
                let command_binary_name = run_command
                    .command
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                let is_editor = !editor_binary_name.is_empty()
                    && (command_binary_name == editor_binary_name
                        || run_command.command == default_editor
                        || (configured_is_vi_vim && is_vi_vim(&command_binary_name))
                        || (configured_is_helix && is_helix(&command_binary_name)));
                if !is_editor {
                    return;
                }

                let args = &run_command.args;
                let binary = &command_binary_name;

                let edit_file: Option<(PathBuf, Option<usize>)> = if is_vim_family(binary) {
                    match args.len() {
                        1 => args
                            .first()
                            .filter(|f| !f.starts_with('-'))
                            .map(|f| (PathBuf::from(f), None)),
                        2 => match (args.first(), args.get(1)) {
                            (Some(line_arg), Some(file)) => line_arg
                                .strip_prefix('+')
                                .and_then(|n| n.parse::<usize>().ok())
                                .map(|line| (PathBuf::from(file), Some(line))),
                            _ => None,
                        },
                        _ => None,
                    }
                } else if is_helix(binary) {
                    if args.len() == 1 {
                        args.first().map(|arg| {
                            if let Some(colon_pos) = arg.rfind(':') {
                                let file_part = &arg[..colon_pos];
                                if let Some(line) = arg
                                    .get(colon_pos + 1..)
                                    .and_then(|s| s.parse::<usize>().ok())
                                {
                                    return (PathBuf::from(file_part), Some(line));
                                }
                            }
                            (PathBuf::from(arg.as_str()), None)
                        })
                    } else {
                        None
                    }
                } else if args.len() == 1 {
                    args.first()
                        .filter(|f| !f.starts_with('-'))
                        .map(|f| (PathBuf::from(f), None))
                } else {
                    None
                };

                if let Some((file_path, line_number)) = edit_file {
                    pane.run = Some(Run::EditFile(file_path, line_number, None));
                }
            }
        };

        for tab in self.tabs.iter_mut() {
            for pane in tab.tiled_panes.iter_mut() {
                upgrade_pane(pane);
            }
            for pane in tab.floating_panes.iter_mut() {
                upgrade_pane(pane);
            }
        }
    }
    pub fn update_plugin_aliases_in_default_layout(&mut self, plugin_aliases: &PluginAliases) {
        self.default_layout
            .populate_plugin_aliases_in_layout(plugin_aliases);
    }
    pub fn to_layout_metadata(&self) -> LayoutMetadata {
        // Get current timestamp for both creation and update time
        let current_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_default();

        // Convert all tabs
        let tabs = self.tabs.iter().map(|tab| tab.to_tab_metadata()).collect();

        LayoutMetadata {
            tabs,
            creation_time: current_time.clone(),
            update_time: current_time,
        }
    }
}

impl From<SessionLayoutMetadata> for GlobalLayoutManifest {
    fn from(val: SessionLayoutMetadata) -> Self {
        GlobalLayoutManifest {
            default_layout: val.default_layout,
            default_shell: val.default_shell,
            global_cwd: val.global_cwd,
            tabs: val
                .tabs
                .into_iter()
                .map(|t| (t.name.clone().unwrap_or_default(), t.into()))
                .collect(),
        }
    }
}

impl From<TabLayoutMetadata> for TabLayoutManifest {
    fn from(val: TabLayoutMetadata) -> Self {
        TabLayoutManifest {
            tab_instance_id: val.tab_instance_id,
            tiled_panes: val.tiled_panes.into_iter().map(|t| t.into()).collect(),
            floating_panes: val.floating_panes.into_iter().map(|t| t.into()).collect(),
            is_focused: val.is_focused,
            hide_floating_panes: val.hide_floating_panes,
        }
    }
}

impl TabLayoutMetadata {
    fn to_tab_metadata(&self) -> TabMetadata {
        let mut panes = Vec::new();

        // Extract pane metadata from tiled panes
        for pane in &self.tiled_panes {
            panes.push(pane.to_pane_metadata());
        }

        // Extract pane metadata from floating panes
        for pane in &self.floating_panes {
            panes.push(pane.to_pane_metadata());
        }

        TabMetadata {
            panes,
            name: self.name.clone(),
        }
    }
}

impl From<PaneLayoutMetadata> for PaneLayoutManifest {
    fn from(val: PaneLayoutMetadata) -> Self {
        PaneLayoutManifest {
            geom: val.geom,
            run: val.run,
            cwd: val.cwd,
            is_borderless: val.is_borderless,
            title: val.title,
            is_focused: val.is_focused,
            pane_contents: val.pane_contents,
            default_fg: val.default_fg,
            default_bg: val.default_bg,
        }
    }
}

#[derive(Default, Debug, Clone)]
pub struct TabLayoutMetadata {
    name: Option<String>,
    tab_instance_id: String,
    tiled_panes: Vec<PaneLayoutMetadata>,
    floating_panes: Vec<PaneLayoutMetadata>,
    is_focused: bool,
    hide_floating_panes: bool,
}

#[derive(Debug, Clone)]
pub struct PaneLayoutMetadata {
    pub(crate) id: PaneId,
    pub(crate) geom: PaneGeom,
    pub(crate) run: Option<Run>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) is_borderless: bool,
    pub(crate) title: Option<String>,
    pub(crate) is_focused: bool,
    pub(crate) pane_contents: Option<String>,
    pub(crate) focused_clients: Vec<ClientId>,
    pub(crate) default_fg: Option<String>,
    pub(crate) default_bg: Option<String>,
}

impl PaneLayoutMetadata {
    fn to_pane_metadata(&self) -> PaneMetadata {
        // Try to extract a meaningful name from the pane
        // Priority: explicit title > command name > file name > plugin location
        let name = self.title.clone().or_else(|| {
            self.run.as_ref().and_then(|run| match run {
                Run::Command(cmd) => Some(cmd.command.display().to_string()),
                Run::EditFile(path, _, _) => {
                    path.file_name().map(|n| n.to_string_lossy().to_string())
                },
                Run::Plugin(plugin) => Some(plugin.location_string()),
                Run::Cwd(_) => None,
            })
        });

        let is_plugin = matches!(self.id, PaneId::Plugin(_));

        // Detect if this is a builtin plugin
        let is_builtin_plugin = self
            .run
            .as_ref()
            .map(|run| match run {
                Run::Plugin(plugin) => plugin.is_builtin_plugin(),
                _ => false,
            })
            .unwrap_or(false);

        PaneMetadata {
            name,
            is_plugin,
            is_builtin_plugin,
        }
    }
}

pub struct ClientMetadata {
    pane_id: PaneId,
    command: Option<Run>,
    command_status: ListClientCommandStatus,
}
impl ClientMetadata {
    pub fn stringify_pane_id(&self) -> String {
        match self.pane_id {
            PaneId::Terminal(terminal_id) => format!("terminal_{}", terminal_id),
            PaneId::Plugin(plugin_id) => format!("plugin_{}", plugin_id),
        }
    }
    pub fn stringify_command(&self, editor: &Option<PathBuf>) -> String {
        match self.command_status {
            ListClientCommandStatus::Unavailable => self.stringify_unavailable_command(editor),
            ListClientCommandStatus::Confirmed | ListClientCommandStatus::Identity => {
                self.stringify_known_command(editor)
                    .unwrap_or_else(|| "N/A".to_owned())
            },
        }
    }
    fn stringify_unavailable_command(&self, editor: &Option<PathBuf>) -> String {
        match self.stringify_known_command(editor) {
            Some(last) if !last.is_empty() && last != "N/A" => {
                format!("UNAVAILABLE (last: {last})")
            },
            _ => "UNAVAILABLE".to_owned(),
        }
    }
    fn stringify_known_command(&self, editor: &Option<PathBuf>) -> Option<String> {
        match &self.command {
            Some(Run::Command(..)) => {
                let (command, args) = extract_command_and_args(&self.command);
                command.map(|c| format!("{} {}", c, args.join(" ")))
            },
            Some(Run::EditFile(..)) => {
                let (file_to_edit, _line_number) = extract_edit_and_line_number(&self.command);
                editor.as_ref().and_then(|editor| {
                    file_to_edit
                        .map(|file_to_edit| format!("{} {}", editor.display(), file_to_edit))
                })
            },
            Some(Run::Plugin(..)) => {
                let (plugin, _plugin_config) = extract_plugin_and_config(&self.command);
                plugin.map(|p| p.to_string())
            },
            _ => None,
        }
    }
    pub fn get_pane_id(&self) -> PaneId {
        self.pane_id
    }
    pub fn render_many(
        clients_metadata: BTreeMap<ClientId, ClientMetadata>,
        default_editor: &Option<PathBuf>,
    ) -> String {
        let mut lines = vec![];
        lines.push(String::from("CLIENT_ID ZELLIJ_PANE_ID RUNNING_COMMAND"));

        for (client_id, client_metadata) in clients_metadata.iter() {
            // 9 - CLIENT_ID, 14 - ZELLIJ_PANE_ID, 15 - RUNNING_COMMAND
            lines.push(format!(
                "{0: <9} {1: <14} {2: <15}",
                client_id,
                client_metadata.stringify_pane_id(),
                client_metadata.stringify_command(default_editor)
            ));
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zellij_utils::pane_size::PaneGeom;

    fn make_command_pane(terminal_id: u32, command: &str, args: Vec<&str>) -> PaneLayoutMetadata {
        let mut run_command = RunCommand::new(PathBuf::from(command));
        run_command.args = args.into_iter().map(|s| s.to_string()).collect();
        PaneLayoutMetadata {
            id: PaneId::Terminal(terminal_id),
            geom: PaneGeom::default(),
            run: Some(Run::Command(run_command)),
            cwd: None,
            is_borderless: false,
            title: None,
            is_focused: false,
            pane_contents: None,
            focused_clients: vec![],
            default_fg: None,
            default_bg: None,
        }
    }

    fn make_edit_file_pane(
        terminal_id: u32,
        path: &str,
        line_number: Option<usize>,
    ) -> PaneLayoutMetadata {
        PaneLayoutMetadata {
            id: PaneId::Terminal(terminal_id),
            geom: PaneGeom::default(),
            run: Some(Run::EditFile(PathBuf::from(path), line_number, None)),
            cwd: None,
            is_borderless: false,
            title: None,
            is_focused: false,
            pane_contents: None,
            focused_clients: vec![],
            default_fg: None,
            default_bg: None,
        }
    }

    fn session_with_editor(editor: &str, panes: Vec<PaneLayoutMetadata>) -> SessionLayoutMetadata {
        let mut meta = SessionLayoutMetadata {
            default_editor: Some(PathBuf::from(editor)),
            ..Default::default()
        };
        meta.add_tab(
            "tab1".to_string(),
            "11111111111111111111111111111111".to_string(),
            true,
            false,
            panes,
            vec![],
        );
        meta
    }

    fn get_first_tiled_run(meta: &SessionLayoutMetadata) -> Option<&Run> {
        meta.tabs[0].tiled_panes[0].run.as_ref()
    }

    #[test]
    fn detects_editor_pane_no_line_number() {
        let pane = make_command_pane(1, "nvim", vec!["file.txt"]);
        let mut meta = session_with_editor("nvim", vec![pane]);
        meta.detect_editor_panes();
        assert_eq!(
            get_first_tiled_run(&meta),
            Some(&Run::EditFile(PathBuf::from("file.txt"), None, None))
        );
    }

    #[test]
    fn detects_editor_pane_with_line_number_vim_family() {
        let pane = make_command_pane(1, "nvim", vec!["+50", "file.txt"]);
        let mut meta = session_with_editor("nvim", vec![pane]);
        meta.detect_editor_panes();
        assert_eq!(
            get_first_tiled_run(&meta),
            Some(&Run::EditFile(PathBuf::from("file.txt"), Some(50), None))
        );
    }

    #[test]
    fn detects_editor_pane_with_line_number_helix() {
        let pane = make_command_pane(1, "hx", vec!["file.txt:50"]);
        let mut meta = session_with_editor("hx", vec![pane]);
        meta.detect_editor_panes();
        assert_eq!(
            get_first_tiled_run(&meta),
            Some(&Run::EditFile(PathBuf::from("file.txt"), Some(50), None))
        );
    }

    #[test]
    fn detects_editor_pane_helix_no_line_number() {
        let pane = make_command_pane(1, "helix", vec!["file.txt"]);
        let mut meta = session_with_editor("helix", vec![pane]);
        meta.detect_editor_panes();
        assert_eq!(
            get_first_tiled_run(&meta),
            Some(&Run::EditFile(PathBuf::from("file.txt"), None, None))
        );
    }

    #[test]
    fn skips_non_editor_command() {
        let pane = make_command_pane(1, "grep", vec!["pattern", "file.txt"]);
        let mut meta = session_with_editor("nvim", vec![pane]);
        meta.detect_editor_panes();
        // Run::Command(grep ...) unchanged
        match get_first_tiled_run(&meta) {
            Some(Run::Command(rc)) => {
                assert_eq!(rc.command, PathBuf::from("grep"));
            },
            other => panic!("expected Command, got {:?}", other),
        }
    }

    #[test]
    fn skips_editor_with_no_args() {
        let pane = make_command_pane(1, "nvim", vec![]);
        let mut meta = session_with_editor("nvim", vec![pane]);
        meta.detect_editor_panes();
        match get_first_tiled_run(&meta) {
            Some(Run::Command(rc)) => {
                assert_eq!(rc.command, PathBuf::from("nvim"));
                assert!(rc.args.is_empty());
            },
            other => panic!("expected Command, got {:?}", other),
        }
    }

    #[test]
    fn skips_editor_with_multiple_files() {
        let pane = make_command_pane(1, "nvim", vec!["a.txt", "b.txt"]);
        let mut meta = session_with_editor("nvim", vec![pane]);
        meta.detect_editor_panes();
        match get_first_tiled_run(&meta) {
            Some(Run::Command(rc)) => {
                assert_eq!(rc.command, PathBuf::from("nvim"));
                assert_eq!(rc.args, vec!["a.txt", "b.txt"]);
            },
            other => panic!("expected Command, got {:?}", other),
        }
    }

    #[test]
    fn detects_editor_matching_by_binary_name() {
        // configured as /usr/bin/nvim, running as nvim
        let pane = make_command_pane(1, "nvim", vec!["file.txt"]);
        let mut meta = session_with_editor("/usr/bin/nvim", vec![pane]);
        meta.detect_editor_panes();
        assert_eq!(
            get_first_tiled_run(&meta),
            Some(&Run::EditFile(PathBuf::from("file.txt"), None, None))
        );
    }

    #[test]
    fn does_not_affect_existing_edit_file_run() {
        let pane = make_edit_file_pane(1, "file.txt", Some(10));
        let mut meta = session_with_editor("nvim", vec![pane]);
        meta.detect_editor_panes();
        assert_eq!(
            get_first_tiled_run(&meta),
            Some(&Run::EditFile(PathBuf::from("file.txt"), Some(10), None))
        );
    }

    #[test]
    fn detects_vi_when_editor_is_vim() {
        // configured as vim, pane running vi (common alias)
        let pane = make_command_pane(1, "vi", vec!["file.txt"]);
        let mut meta = session_with_editor("vim", vec![pane]);
        meta.detect_editor_panes();
        assert_eq!(
            get_first_tiled_run(&meta),
            Some(&Run::EditFile(PathBuf::from("file.txt"), None, None))
        );
    }

    #[test]
    fn detects_nvim_when_editor_is_vi() {
        // configured as vi, pane running nvim
        let pane = make_command_pane(1, "nvim", vec!["file.txt"]);
        let mut meta = session_with_editor("vi", vec![pane]);
        meta.detect_editor_panes();
        assert_eq!(
            get_first_tiled_run(&meta),
            Some(&Run::EditFile(PathBuf::from("file.txt"), None, None))
        );
    }

    #[test]
    fn does_not_cross_match_vim_with_emacs() {
        // configured as emacs, pane running vim — NOT cross-matched
        let pane = make_command_pane(1, "vim", vec!["file.txt"]);
        let mut meta = session_with_editor("emacs", vec![pane]);
        meta.detect_editor_panes();
        match get_first_tiled_run(&meta) {
            Some(Run::Command(rc)) => assert_eq!(rc.command, PathBuf::from("vim")),
            other => panic!("expected Command, got {:?}", other),
        }
    }

    #[test]
    fn detects_hx_when_editor_is_helix() {
        let pane = make_command_pane(1, "hx", vec!["file.txt"]);
        let mut meta = session_with_editor("helix", vec![pane]);
        meta.detect_editor_panes();
        assert_eq!(
            get_first_tiled_run(&meta),
            Some(&Run::EditFile(PathBuf::from("file.txt"), None, None))
        );
    }

    #[test]
    fn absolute_file_path_preserved() {
        let pane = make_command_pane(1, "nvim", vec!["/home/user/file.txt"]);
        let mut meta = session_with_editor("nvim", vec![pane]);
        meta.detect_editor_panes();
        assert_eq!(
            get_first_tiled_run(&meta),
            Some(&Run::EditFile(
                PathBuf::from("/home/user/file.txt"),
                None,
                None
            ))
        );
    }

    #[test]
    fn list_clients_render_keeps_client_pane_and_command_columns() {
        let mut pane = make_command_pane(7, "workload", vec!["--pid"]);
        pane.focused_clients = vec![2];
        let mut meta = SessionLayoutMetadata {
            default_editor: Some(PathBuf::from("nvim")),
            ..Default::default()
        };
        meta.add_tab(
            "tab1".to_string(),
            "11111111111111111111111111111111".to_string(),
            true,
            true,
            vec![pane],
            vec![],
        );
        let rendered = meta.list_clients_metadata();
        let mut lines = rendered.lines();
        assert_eq!(
            lines.next(),
            Some("CLIENT_ID ZELLIJ_PANE_ID RUNNING_COMMAND")
        );
        let row = lines.next().expect("one focused client");
        let columns: Vec<&str> = row.split_whitespace().collect();
        assert_eq!(columns[0], "2");
        assert_eq!(columns[1], "terminal_7");
        assert!(columns[2].contains("workload"));
        assert!(!row.contains("UNAVAILABLE"));
        assert!(lines.next().is_none());
    }

    #[test]
    fn focused_list_client_terminal_ids_ignore_unfocused_and_plugin_panes() {
        let mut silent = make_command_pane(1, "silent", vec!["sleep"]);
        silent.focused_clients = vec![];
        let mut focused = make_command_pane(7, "workload", vec!["--pid"]);
        focused.focused_clients = vec![2];
        let plugin = PaneLayoutMetadata {
            id: PaneId::Plugin(3),
            geom: PaneGeom::default(),
            run: Some(Run::Plugin(RunPluginOrAlias::RunPlugin(
                RunPlugin::from_url("vc-frame:compact-bar").unwrap(),
            ))),
            cwd: None,
            is_borderless: false,
            title: None,
            is_focused: true,
            pane_contents: None,
            focused_clients: vec![4],
            default_fg: None,
            default_bg: None,
        };
        let mut meta = SessionLayoutMetadata::default();
        meta.add_tab(
            "tab1".to_string(),
            "11111111111111111111111111111111".to_string(),
            true,
            true,
            vec![silent, focused, plugin],
            vec![],
        );
        assert_eq!(meta.focused_list_client_terminal_ids(), vec![7]);
    }

    #[test]
    fn list_clients_unavailable_terminal_does_not_present_stale_command_as_current() {
        let mut pane = make_command_pane(7, "stale-invoked", vec!["--old"]);
        pane.focused_clients = vec![2];
        let mut meta = SessionLayoutMetadata::default();
        meta.add_tab(
            "tab1".to_string(),
            "11111111111111111111111111111111".to_string(),
            true,
            true,
            vec![pane],
            vec![],
        );
        meta.mark_list_client_terminal_unconfirmed(7);
        let rendered = meta.list_clients_metadata();
        let row = rendered.lines().nth(1).expect("one focused client");
        let command = row
            .split_once("terminal_7")
            .map(|(_, rest)| rest.trim())
            .expect("pane id");
        assert!(
            command.starts_with("UNAVAILABLE"),
            "stale invoked_with must not be the confirmed command cell: {row}"
        );
        assert!(command.contains("last: stale-invoked --old"));
    }

    #[test]
    fn list_clients_plugin_row_uses_plugin_identity_not_pty_confirmation() {
        let pane = PaneLayoutMetadata {
            id: PaneId::Plugin(3),
            geom: PaneGeom::default(),
            run: Some(Run::Plugin(RunPluginOrAlias::RunPlugin(
                RunPlugin::from_url("vc-frame:compact-bar").unwrap(),
            ))),
            cwd: None,
            is_borderless: false,
            title: None,
            is_focused: true,
            pane_contents: None,
            focused_clients: vec![2],
            default_fg: None,
            default_bg: None,
        };
        let mut meta = SessionLayoutMetadata::default();
        meta.add_tab(
            "tab1".to_string(),
            "11111111111111111111111111111111".to_string(),
            true,
            true,
            vec![pane],
            vec![],
        );
        let rendered = meta.list_clients_metadata();
        let row = rendered.lines().nth(1).expect("one focused client");
        assert!(row.contains("plugin_3"));
        assert!(row.contains("vc-frame:compact-bar"));
        assert!(!row.contains("UNAVAILABLE"));
    }

    #[test]
    fn list_clients_editor_row_unavailable_when_pty_did_not_confirm() {
        let mut pane = make_edit_file_pane(9, "notes.md", Some(12));
        pane.focused_clients = vec![2];
        let mut meta = session_with_editor("nvim", vec![pane]);
        meta.tabs[0].hide_floating_panes = true;
        meta.mark_list_client_terminal_unconfirmed(9);
        let rendered = meta.list_clients_metadata();
        let row = rendered.lines().nth(1).expect("one focused client");
        let command = row
            .split_once("terminal_9")
            .map(|(_, rest)| rest.trim())
            .expect("pane id");
        assert!(
            command.starts_with("UNAVAILABLE"),
            "EditFile invoked_with must not be confirmed current: {row}"
        );
        assert!(command.contains("last: nvim notes.md"));
    }
}
