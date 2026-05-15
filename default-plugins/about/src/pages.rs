use zellij_tile::prelude::*;

use std::cell::RefCell;
use std::rc::Rc;

#[derive(Debug)]
pub struct Page {
    title: Option<Text>,
    components_to_render: Vec<RenderedComponent>,
    has_hover: bool,
    hovering_over_link: bool,
    menu_item_is_selected: bool,
    pub is_main_screen: bool,
}

#[derive(Debug)]
pub struct ActiveComponent {
    text_no_hover: TextOrCustomRender,
    text_hover: Option<TextOrCustomRender>,
    left_click_action: Option<ClickAction>,
    last_rendered_coordinates: Option<ComponentCoordinates>,
    pub is_active: bool,
}

impl ActiveComponent {
    pub fn new(text_no_hover: TextOrCustomRender) -> Self {
        ActiveComponent {
            text_no_hover,
            text_hover: None,
            left_click_action: None,
            is_active: false,
            last_rendered_coordinates: None,
        }
    }
    pub fn with_hover(mut self, text_hover: TextOrCustomRender) -> Self {
        self.text_hover = Some(text_hover);
        self
    }
    pub fn with_left_click_action(mut self, left_click_action: ClickAction) -> Self {
        self.left_click_action = Some(left_click_action);
        self
    }
    pub fn render(&mut self, x: usize, y: usize, rows: usize, columns: usize) -> usize {
        let component_width = match self.text_hover.as_mut() {
            Some(text) if self.is_active => text.render(x, y, rows, columns),
            _ => self.text_no_hover.render(x, y, rows, columns),
        };
        self.last_rendered_coordinates = Some(ComponentCoordinates::new(x, y, 1, columns));
        component_width
    }
    pub fn left_click_action(&mut self) -> Option<Page> {
        match self.left_click_action.take() {
            Some(ClickAction::ChangePage(go_to_page)) => Some(go_to_page()),
            Some(ClickAction::OpenLink(link, executable)) => {
                self.left_click_action =
                    Some(ClickAction::OpenLink(link.clone(), executable.clone()));
                run_command(&[&executable.borrow(), &link], Default::default());
                None
            },
            Some(ClickAction::LaunchPlugin(plugin_url)) => {
                open_plugin_pane_floating(
                    &plugin_url,
                    Default::default(),
                    None,
                    Default::default(),
                );
                self.left_click_action = Some(ClickAction::LaunchPlugin(plugin_url));
                None
            },
            None => None,
        }
    }
    pub fn handle_left_click_at_position(&mut self, x: usize, y: usize) -> Option<Page> {
        let Some(last_rendered_coordinates) = &self.last_rendered_coordinates else {
            return None;
        };
        if last_rendered_coordinates.contains(x, y) {
            self.left_click_action()
        } else {
            None
        }
    }
    pub fn handle_hover_at_position(&mut self, x: usize, y: usize) -> bool {
        let Some(last_rendered_coordinates) = &self.last_rendered_coordinates else {
            return false;
        };
        if last_rendered_coordinates.contains(x, y) && self.text_hover.is_some() {
            self.is_active = true;
            true
        } else {
            false
        }
    }
    pub fn handle_selection(&mut self) -> Option<Page> {
        if self.is_active {
            self.left_click_action()
        } else {
            None
        }
    }
    pub fn column_count(&self) -> usize {
        match self.text_hover.as_ref() {
            Some(text) if self.is_active => text.len(),
            _ => self.text_no_hover.len(),
        }
    }
    pub fn clear_hover(&mut self) {
        self.is_active = false;
    }
}

#[derive(Debug)]
struct ComponentCoordinates {
    x: usize,
    y: usize,
    rows: usize,
    columns: usize,
}

impl ComponentCoordinates {
    fn new(x: usize, y: usize, rows: usize, columns: usize) -> Self {
        ComponentCoordinates {
            x,
            y,
            rows,
            columns,
        }
    }

    fn contains(&self, x: usize, y: usize) -> bool {
        x >= self.x && x < self.x + self.columns && y >= self.y && y < self.y + self.rows
    }
}

pub enum ClickAction {
    ChangePage(Box<dyn FnOnce() -> Page>),
    OpenLink(String, Rc<RefCell<String>>),
    LaunchPlugin(String),
}

impl std::fmt::Debug for ClickAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClickAction::ChangePage(_) => write!(f, "ChangePage"),
            ClickAction::OpenLink(destination, executable) => {
                write!(f, "OpenLink: {}, {:?}", destination, executable)
            },
            ClickAction::LaunchPlugin(url) => {
                write!(f, "LaunchPlugin: {}", url)
            },
        }
    }
}

impl ClickAction {
    pub fn new_change_page<F>(go_to_page: F) -> Self
    where
        F: FnOnce() -> Page + 'static,
    {
        ClickAction::ChangePage(Box::new(go_to_page))
    }
    pub fn new_open_link(destination: String, executable: Rc<RefCell<String>>) -> Self {
        ClickAction::OpenLink(destination, executable)
    }
    pub fn new_launch_plugin(plugin_url: String) -> Self {
        ClickAction::LaunchPlugin(plugin_url)
    }
}

impl Page {
    pub fn new_main_screen(
        link_executable: Rc<RefCell<String>>,
        zellij_version: String,
        _base_mode: Rc<RefCell<InputMode>>,
        is_release_notes: bool,
    ) -> Self {
        Page::new()
            .main_screen()
            .with_title(main_screen_title(zellij_version.clone(), is_release_notes))
            .with_bulletin_list(BulletinList::new(whats_new_title()).with_items(vec![
                    ActiveComponent::new(TextOrCustomRender::Text(main_menu_item(
                        "Windows Support",
                    )))
                    .with_hover(TextOrCustomRender::Text(
                        main_menu_item("Windows Support").selected(),
                    ))
                    .with_left_click_action(ClickAction::new_change_page({
                        move || Page::new_windows_support()
                    })),
                    ActiveComponent::new(TextOrCustomRender::Text(main_menu_item(
                        "Remote Sessions",
                    )))
                    .with_hover(TextOrCustomRender::Text(
                        main_menu_item("Remote Sessions").selected(),
                    ))
                    .with_left_click_action(ClickAction::new_change_page({
                        let link_executable = link_executable.clone();
                        move || Page::new_remote_sessions(link_executable.clone())
                    })),
                    ActiveComponent::new(TextOrCustomRender::Text(main_menu_item(
                        "Read-Only Session Sharing",
                    )))
                    .with_hover(TextOrCustomRender::Text(
                        main_menu_item("Read-Only Session Sharing").selected(),
                    ))
                    .with_left_click_action(ClickAction::new_change_page({
                        let link_executable = link_executable.clone();
                        move || Page::new_read_only_sharing(link_executable.clone())
                    })),
                    ActiveComponent::new(TextOrCustomRender::Text(main_menu_item(
                        "CLI Automation",
                    )))
                    .with_hover(TextOrCustomRender::Text(
                        main_menu_item("CLI Automation").selected(),
                    ))
                    .with_left_click_action(ClickAction::new_change_page({
                        let link_executable = link_executable.clone();
                        move || Page::new_cli_automation(link_executable.clone())
                    })),
                    ActiveComponent::new(TextOrCustomRender::Text(main_menu_item(
                        "Mouse Resize",
                    )))
                    .with_hover(TextOrCustomRender::Text(
                        main_menu_item("Mouse Resize").selected(),
                    ))
                    .with_left_click_action(ClickAction::new_change_page(move || {
                        Page::new_mouse_resize()
                    })),
                    ActiveComponent::new(TextOrCustomRender::Text(main_menu_item(
                        "Click-to-Open File Paths",
                    )))
                    .with_hover(TextOrCustomRender::Text(
                        main_menu_item("Click-to-Open File Paths").selected(),
                    ))
                    .with_left_click_action(ClickAction::new_change_page({
                        move || Page::new_click_to_open()
                    })),
                    ActiveComponent::new(TextOrCustomRender::Text(main_menu_item(
                        "Layout Manager",
                    )))
                    .with_hover(TextOrCustomRender::Text(
                        main_menu_item("Layout Manager").selected(),
                    ))
                    .with_left_click_action(ClickAction::new_change_page({
                        move || Page::new_layout_manager()
                    })),
                ]))
            .with_paragraph(vec![ComponentLine::new(vec![
                ActiveComponent::new(TextOrCustomRender::Text(Text::new("Full Changelog: "))),
                ActiveComponent::new(TextOrCustomRender::Text(changelog_link_unselected(
                    zellij_version.clone(),
                )))
                .with_hover(TextOrCustomRender::CustomRender(
                    Box::new(changelog_link_selected(zellij_version.clone())),
                    Box::new(changelog_link_selected_len(zellij_version.clone())),
                ))
                .with_left_click_action(ClickAction::new_open_link(
                    format!(
                        "https://github.com/zellij-org/zellij/releases/tag/v{}",
                        zellij_version.clone()
                    ),
                    link_executable.clone(),
                )),
            ])])
            .with_paragraph(vec![ComponentLine::new(vec![
                ActiveComponent::new(TextOrCustomRender::Text(support_the_developer_text())),
                ActiveComponent::new(TextOrCustomRender::Text(sponsors_link_text_unselected()))
                    .with_hover(TextOrCustomRender::CustomRender(
                        Box::new(sponsors_link_text_selected),
                        Box::new(sponsors_link_text_selected_len),
                    ))
                    .with_left_click_action(ClickAction::new_open_link(
                        "https://github.com/sponsors/imsnif".to_owned(),
                        link_executable.clone(),
                    )),
            ])])
            .with_help(if is_release_notes {
                Box::new(|hovering_over_link, menu_item_is_selected| {
                    release_notes_main_help(hovering_over_link, menu_item_is_selected)
                })
            } else {
                Box::new(|hovering_over_link, menu_item_is_selected| {
                    main_screen_help_text(hovering_over_link, menu_item_is_selected)
                })
            })
    }
    pub fn new_vibecrafted_mission_control(
        link_executable: Rc<RefCell<String>>,
        zellij_version: String,
        _base_mode: Rc<RefCell<InputMode>>,
    ) -> Self {
        Page::new()
            .main_screen()
            .with_title(Text::new("VibeCrafted Mission Control").color_range(0, ..))
            .with_paragraph(vec![
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("A branded shell-provider surface built on top of native Zellij control decks."),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new(format!(
                        "This guide is wired into Zellij {} so operators can jump from telemetry into action without leaving the dashboard.",
                        zellij_version
                    ))
                    .color_substring(2, "operators")
                    .color_substring(3, "dashboard"),
                ))]),
            ])
            .with_bulletin_list(
                BulletinList::new(Text::new("Open a deck:").color_range(2, ..)).with_items(vec![
                    ActiveComponent::new(TextOrCustomRender::Text(main_menu_item(
                        "Session Atlas",
                    )))
                    .with_hover(TextOrCustomRender::Text(
                        main_menu_item("Session Atlas").selected(),
                    ))
                    .with_left_click_action(ClickAction::new_launch_plugin(
                        "zellij:session-manager".to_owned(),
                    )),
                    ActiveComponent::new(TextOrCustomRender::Text(main_menu_item(
                        "Layout Forge",
                    )))
                    .with_hover(TextOrCustomRender::Text(
                        main_menu_item("Layout Forge").selected(),
                    ))
                    .with_left_click_action(ClickAction::new_launch_plugin(
                        "zellij:layout-manager".to_owned(),
                    )),
                    ActiveComponent::new(TextOrCustomRender::Text(main_menu_item(
                        "Control Deck",
                    )))
                    .with_hover(TextOrCustomRender::Text(
                        main_menu_item("Control Deck").selected(),
                    ))
                    .with_left_click_action(ClickAction::new_launch_plugin(
                        "zellij:configuration".to_owned(),
                    )),
                    ActiveComponent::new(TextOrCustomRender::Text(main_menu_item(
                        "Plugin Forge",
                    )))
                    .with_hover(TextOrCustomRender::Text(
                        main_menu_item("Plugin Forge").selected(),
                    ))
                    .with_left_click_action(ClickAction::new_launch_plugin(
                        "zellij:plugin-manager".to_owned(),
                    )),
                    ActiveComponent::new(TextOrCustomRender::Text(main_menu_item(
                        "Workspace Atlas",
                    )))
                    .with_hover(TextOrCustomRender::Text(
                        main_menu_item("Workspace Atlas").selected(),
                    ))
                    .with_left_click_action(ClickAction::new_launch_plugin(
                        "zellij:strider".to_owned(),
                    )),
                    ActiveComponent::new(TextOrCustomRender::Text(main_menu_item(
                        "Share Relay",
                    )))
                    .with_hover(TextOrCustomRender::Text(
                        main_menu_item("Share Relay").selected(),
                    ))
                    .with_left_click_action(ClickAction::new_launch_plugin(
                        "zellij:share".to_owned(),
                    )),
                ]),
            )
            .with_paragraph(vec![
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Operator shell: vibecrafted start").color_substring(
                        3,
                        "vibecrafted start",
                    ),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Implementation surface: vibecrafted dashboard vc-workflow")
                        .color_substring(3, "vibecrafted dashboard vc-workflow"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Convergence surface: vibecrafted dashboard vc-marbles")
                        .color_substring(3, "vibecrafted dashboard vc-marbles"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Research surface: vibecrafted dashboard vc-research")
                        .color_substring(3, "vibecrafted dashboard vc-research"),
                ))]),
            ])
            .with_paragraph(vec![ComponentLine::new(vec![
                ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Learn more about the native Zellij surfaces behind this shell: ")
                        .color_range(2, ..),
                )),
                ActiveComponent::new(TextOrCustomRender::Text(Text::new(
                    "https://zellij.dev/documentation/",
                )))
                .with_hover(TextOrCustomRender::Text(
                    Text::new("https://zellij.dev/documentation/").selected(),
                ))
                .with_left_click_action(ClickAction::new_open_link(
                    "https://zellij.dev/documentation/".to_owned(),
                    link_executable.clone(),
                )),
            ])])
            .with_help(Box::new(|hovering_over_link, menu_item_is_selected| {
                main_screen_help_text(hovering_over_link, menu_item_is_selected)
            }))
    }
    fn new_windows_support() -> Page {
        Page::new()
            .with_title(Text::new("Windows Support").color_range(0, ..))
            .with_paragraph(vec![
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Zellij now runs natively on Windows."),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Windows users can now enjoy the same workspace management, plugin ecosystem"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("and multiplayer capabilities that have been available on Linux and macOS."),
                ))]),
            ])
            .with_help(Box::new(|_hovering_over_link, _menu_item_is_selected| {
                esc_to_go_back_help()
            }))
    }
    pub fn new_remote_sessions(link_executable: Rc<RefCell<String>>) -> Page {
        Page::new()
            .with_title(Text::new("Remote Sessions").color_range(0, ..))
            .with_paragraph(vec![
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Attach to remote Zellij sessions over HTTPS, directly from the terminal."),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("The remote session needs to be running the Zellij web client."),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Zellij will attach to it exactly as a browser would, through the same interface."),
                ))]),
            ])
            .with_bulletin_list(
                BulletinList::new(Text::new("Try it:").color_range(2, ..))
                    .with_items(vec![
                        ActiveComponent::new(TextOrCustomRender::Text(
                            Text::new("Run the Zellij web server on one machine")
                                .color_substring(3, "Zellij web server"),
                        ))
                        .with_hover(TextOrCustomRender::Text(
                            Text::new("Run the Zellij web server on one machine")
                                .color_substring(3, "Zellij web server")
                                .selected(),
                        ))
                        .with_left_click_action(ClickAction::new_launch_plugin(
                            "zellij:share".to_owned(),
                        )),
                        ActiveComponent::new(TextOrCustomRender::Text(
                            Text::new("From another: zellij attach https://<ip>/<session-name>")
                                .color_substring(3, "zellij attach")
                                .color_substring(2, "https://<ip>/<session-name>"),
                        )),
                    ]),
            )
            .with_paragraph(vec![ComponentLine::new(vec![
                ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Learn more about the web client: ").color_range(2, ..),
                )),
                ActiveComponent::new(TextOrCustomRender::Text(Text::new(
                    "https://zellij.dev/tutorials/web-client/",
                )))
                .with_hover(TextOrCustomRender::CustomRender(
                    Box::new(web_client_link_selected),
                    Box::new(web_client_link_selected_len),
                ))
                .with_left_click_action(ClickAction::new_open_link(
                    "https://zellij.dev/tutorials/web-client/".to_owned(),
                    link_executable.clone(),
                )),
            ])])
            .with_help(Box::new(|hovering_over_link, menu_item_is_selected| {
                esc_go_back_plus_link_hover(hovering_over_link, menu_item_is_selected)
            }))
    }
    fn new_read_only_sharing(link_executable: Rc<RefCell<String>>) -> Page {
        Page::new()
            .with_title(Text::new("Read-Only Session Sharing").color_range(0, ..))
            .with_paragraph(vec![
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Sessions can now be shared in read-only mode."),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new(
                        "Useful for demonstrations, teaching, monitoring and pair programming",
                    )
                    .color_substring(2, "demonstrations")
                    .color_substring(2, "teaching")
                    .color_substring(2, "monitoring")
                    .color_substring(2, "pair programming"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("where one participant should observe without interfering."),
                ))]),
            ])
            .with_paragraph(vec![
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Create a read-only web token with:")
                        .color_substring(2, "read-only web token"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("zellij web --create-read-only-token").color_range(3, ..),
                ))]),
            ])
            .with_paragraph(vec![ComponentLine::new(vec![ActiveComponent::new(
                TextOrCustomRender::Text(Text::new(
                    "Share the token for view-only access without risk of unintended input.",
                )),
            )])])
            .with_paragraph(vec![ComponentLine::new(vec![
                ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Learn more: ").color_range(2, ..),
                )),
                ActiveComponent::new(TextOrCustomRender::Text(Text::new(
                    "https://zellij.dev/tutorials/web-client/",
                )))
                .with_hover(TextOrCustomRender::CustomRender(
                    Box::new(web_client_link_selected),
                    Box::new(web_client_link_selected_len),
                ))
                .with_left_click_action(ClickAction::new_open_link(
                    "https://zellij.dev/tutorials/web-client/".to_owned(),
                    link_executable.clone(),
                )),
            ])])
            .with_help(Box::new(|hovering_over_link, menu_item_is_selected| {
                esc_go_back_plus_link_hover(hovering_over_link, menu_item_is_selected)
            }))
    }
    fn new_cli_automation(link_executable: Rc<RefCell<String>>) -> Page {
        Page::new()
            .with_title(Text::new("CLI Automation").color_range(0, ..))
            .with_paragraph(vec![
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("This release significantly expands the CLI's control surface,"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("enabling the building of powerful workspace automations."),
                ))]),
            ])
            .with_bulletin_list(
                BulletinList::new(Text::new("New and expanded capabilities:").color_range(2, ..))
                    .with_items(vec![
                        ActiveComponent::new(TextOrCustomRender::Text(
                            Text::new("list-panes, list-tabs, dump-screen, dump-layout with --json output")
                                .color_substring(3, "list-panes")
                                .color_substring(3, "list-tabs")
                                .color_substring(3, "dump-screen")
                                .color_substring(3, "dump-layout")
                                .color_substring(3, "--json"),
                        )),
                        ActiveComponent::new(TextOrCustomRender::Text(
                            Text::new("zellij run optionally blocks until success/failure")
                                .color_substring(3, "zellij run"),
                        )),
                        ActiveComponent::new(TextOrCustomRender::Text(
                            Text::new("zellij subscribe can stream pane scrollback in real time")
                                .color_substring(3, "zellij subscribe"),
                        )),
                        ActiveComponent::new(TextOrCustomRender::Text(
                            Text::new("zellij send-keys/paste can send human readable keys to other panes or sessions")
                                .color_substring(3, "zellij send-keys/paste"),
                        )),
                    ]),
            )
            .with_paragraph(vec![ComponentLine::new(vec![
                ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Learn more: ").color_range(2, ..),
                )),
                ActiveComponent::new(TextOrCustomRender::Text(Text::new(
                    "https://zellij.dev/documentation/controlling-zellij-through-cli.html",
                )))
                .with_hover(TextOrCustomRender::CustomRender(
                    Box::new(cli_automation_link_selected),
                    Box::new(cli_automation_link_selected_len),
                ))
                .with_left_click_action(ClickAction::new_open_link(
                    "https://zellij.dev/documentation/controlling-zellij-through-cli.html".to_owned(),
                    link_executable.clone(),
                )),
            ])])
            .with_help(Box::new(|hovering_over_link, menu_item_is_selected| {
                esc_go_back_plus_link_hover(hovering_over_link, menu_item_is_selected)
            }))
    }
    fn new_mouse_resize() -> Page {
        Page::new()
            .with_title(Text::new("Mouse Resize").color_range(0, ..))
            .with_paragraph(vec![
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Panes can now be resized by dragging their borders with the mouse."),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Tiled panes can be resized with or without Ctrl held down.")
                        .color_substring(3, "Ctrl"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Floating panes require Ctrl+drag to resize.")
                        .color_substring(3, "Ctrl+drag"),
                ))]),
            ])
            .with_paragraph(vec![ComponentLine::new(vec![ActiveComponent::new(
                TextOrCustomRender::Text(
                    Text::new("Try it: Ctrl+drag on the borders of this pane.")
                        .color_substring(2, "Try it:")
                        .color_substring(3, "Ctrl+drag"),
                ),
            )])])
            .with_help(Box::new(|_hovering_over_link, _menu_item_is_selected| {
                esc_to_go_back_help()
            }))
    }
    fn new_click_to_open() -> Page {
        Page::new()
            .with_title(Text::new("Click-to-Open File Paths").color_range(0, ..))
            .with_paragraph(vec![
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Zellij now detects file paths in the terminal viewport."),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Alt-Click on a file path to open it.")
                        .color_substring(3, "Alt-Click"),
                ))]),
            ])
            .with_paragraph(vec![
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Useful for navigating compiler errors, grep results,")
                        .color_substring(2, "compiler errors")
                        .color_substring(2, "grep results"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("log files, or any output containing file paths.")
                        .color_substring(2, "log files"),
                ))]),
            ])
            .with_paragraph(vec![
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Plugins can also highlight arbitrary text in the viewport,"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("opening possibilities for custom link handlers")
                        .color_substring(3, "custom link handlers"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("and interactive overlays.")
                        .color_substring(3, "interactive overlays"),
                ))]),
            ])
            .with_help(Box::new(|_hovering_over_link, _menu_item_is_selected| {
                esc_to_go_back_help()
            }))
    }
    fn new_layout_manager() -> Page {
        Page::new()
            .with_title(Text::new("Layout Manager").color_range(0, ..))
            .with_paragraph(vec![
                ComponentLine::new(vec![
                    ActiveComponent::new(TextOrCustomRender::Text(Text::new("A new "))),
                    ActiveComponent::new(TextOrCustomRender::Text(
                        Text::new("layout-manager interface").color_range(3, ..),
                    ))
                    .with_hover(TextOrCustomRender::Text(
                        Text::new("layout-manager interface")
                            .color_range(3, ..)
                            .selected(),
                    ))
                    .with_left_click_action(ClickAction::new_launch_plugin(
                        "zellij:layout-manager".to_owned(),
                    )),
                    ActiveComponent::new(TextOrCustomRender::Text(Text::new(
                        " allows overriding layouts at runtime.",
                    ))),
                ]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new(
                        "Workspaces can be reconfigured dynamically without restarting sessions.",
                    ),
                ))]),
            ])
            .with_paragraph(vec![
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Access it through the session menu, or run:")
                        .color_substring(2, "session menu"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("zellij plugin -- zellij:layout-manager").color_range(3, ..),
                ))]),
            ])
            .with_help(Box::new(|_hovering_over_link, _menu_item_is_selected| {
                esc_to_go_back_help()
            }))
    }
}

impl Page {
    pub fn new() -> Self {
        Page {
            title: None,
            components_to_render: vec![],
            has_hover: false,
            hovering_over_link: false,
            menu_item_is_selected: false,
            is_main_screen: false,
        }
    }
    pub fn main_screen(mut self) -> Self {
        self.is_main_screen = true;
        self
    }
    pub fn with_title(mut self, title: Text) -> Self {
        self.title = Some(title);
        self
    }
    pub fn with_bulletin_list(mut self, bulletin_list: BulletinList) -> Self {
        self.components_to_render
            .push(RenderedComponent::BulletinList(bulletin_list));
        self
    }
    pub fn with_paragraph(mut self, paragraph: Vec<ComponentLine>) -> Self {
        self.components_to_render
            .push(RenderedComponent::Paragraph(paragraph));
        self
    }
    pub fn with_help(mut self, help_text_fn: Box<dyn Fn(bool, bool) -> Text>) -> Self {
        self.components_to_render
            .push(RenderedComponent::HelpText(help_text_fn));
        self
    }
    pub fn handle_key(&mut self, key: KeyWithModifier) -> bool {
        let mut should_render = false;
        if key.bare_key == BareKey::Down && key.has_no_modifiers() {
            self.move_selection_down();
            should_render = true;
        } else if key.bare_key == BareKey::Up && key.has_no_modifiers() {
            self.move_selection_up();
            should_render = true;
        }
        should_render
    }
    pub fn handle_mouse_left_click(&mut self, x: usize, y: usize) -> Option<Page> {
        for rendered_component in &mut self.components_to_render {
            match rendered_component {
                RenderedComponent::BulletinList(bulletin_list) => {
                    let page_to_render = bulletin_list.handle_left_click_at_position(x, y);
                    if page_to_render.is_some() {
                        return page_to_render;
                    }
                },
                RenderedComponent::Paragraph(paragraph) => {
                    for component_line in paragraph {
                        let page_to_render = component_line.handle_left_click_at_position(x, y);
                        if page_to_render.is_some() {
                            return page_to_render;
                        }
                    }
                },
                _ => {},
            }
        }
        None
    }
    pub fn handle_selection(&mut self) -> Option<Page> {
        for rendered_component in &mut self.components_to_render {
            if let RenderedComponent::BulletinList(bulletin_list) = rendered_component {
                let page_to_render = bulletin_list.handle_selection();
                if page_to_render.is_some() {
                    return page_to_render;
                }
            }
        }
        None
    }
    pub fn handle_mouse_hover(&mut self, x: usize, y: usize) -> bool {
        let hover_cleared = self.clear_hover(); // TODO: do the right thing if the same component was hovered from
                                                // previous motion
        for rendered_component in &mut self.components_to_render {
            match rendered_component {
                RenderedComponent::BulletinList(bulletin_list) => {
                    let should_render = bulletin_list.handle_hover_at_position(x, y);
                    if should_render {
                        self.has_hover = true;
                        self.menu_item_is_selected = true;
                        return should_render;
                    }
                },
                RenderedComponent::Paragraph(paragraph) => {
                    for component_line in paragraph {
                        let should_render = component_line.handle_hover_at_position(x, y);
                        if should_render {
                            self.has_hover = true;
                            self.hovering_over_link = true;
                            return should_render;
                        }
                    }
                },
                _ => {},
            }
        }
        hover_cleared
    }
    fn move_selection_up(&mut self) {
        match self.position_of_active_bulletin() {
            Some(position_of_active_bulletin) if position_of_active_bulletin > 0 => {
                self.clear_active_bulletins();
                self.set_active_bulletin(position_of_active_bulletin.saturating_sub(1));
            },
            Some(0) => {
                self.clear_active_bulletins();
            },
            _ => {
                self.clear_active_bulletins();
                self.set_last_active_bulletin();
            },
        }
    }
    fn move_selection_down(&mut self) {
        match self.position_of_active_bulletin() {
            Some(position_of_active_bulletin) => {
                self.clear_active_bulletins();
                self.set_active_bulletin(position_of_active_bulletin + 1);
            },
            None => {
                self.set_active_bulletin(0);
            },
        }
    }
    fn position_of_active_bulletin(&self) -> Option<usize> {
        self.components_to_render.iter().find_map(|c| match c {
            RenderedComponent::BulletinList(bulletin_list) => {
                bulletin_list.active_component_position()
            },
            _ => None,
        })
    }
    fn clear_active_bulletins(&mut self) {
        self.components_to_render.iter_mut().for_each(|c| {
            match c {
                RenderedComponent::BulletinList(bulletin_list) => {
                    let _: () = bulletin_list.clear_active_bulletins();
                    Some(())
                },
                _ => None,
            };
        });
    }
    fn set_active_bulletin(&mut self, active_bulletin_position: usize) {
        self.components_to_render.iter_mut().for_each(|c| {
            if let RenderedComponent::BulletinList(bulletin_list) = c {
                bulletin_list.set_active_bulletin(active_bulletin_position)
            };
        });
    }
    fn set_last_active_bulletin(&mut self) {
        self.components_to_render.iter_mut().for_each(|c| {
            if let RenderedComponent::BulletinList(bulletin_list) = c {
                bulletin_list.set_last_active_bulletin()
            };
        });
    }
    fn clear_hover(&mut self) -> bool {
        let had_hover = self.has_hover;
        self.menu_item_is_selected = false;
        self.hovering_over_link = false;
        for rendered_component in &mut self.components_to_render {
            match rendered_component {
                RenderedComponent::BulletinList(bulletin_list) => {
                    bulletin_list.clear_hover();
                },
                RenderedComponent::Paragraph(paragraph) => {
                    for active_component in paragraph {
                        active_component.clear_hover();
                    }
                },
                _ => {},
            }
        }
        self.has_hover = false;
        had_hover
    }
    pub fn ui_column_count(&mut self) -> usize {
        let mut column_count = 0;
        for rendered_component in &self.components_to_render {
            match rendered_component {
                RenderedComponent::BulletinList(bulletin_list) => {
                    column_count = std::cmp::max(column_count, bulletin_list.column_count());
                },
                RenderedComponent::Paragraph(paragraph) => {
                    for active_component in paragraph {
                        column_count = std::cmp::max(column_count, active_component.column_count());
                    }
                },
                RenderedComponent::HelpText(_text) => {}, // we ignore help text in column
                                                          // calculation because it's always left
                                                          // justified
            }
        }
        column_count
    }
    pub fn ui_row_count(&mut self) -> usize {
        let mut row_count = 0;
        if self.title.is_some() {
            row_count += 1;
        }
        for rendered_component in &self.components_to_render {
            match rendered_component {
                RenderedComponent::BulletinList(bulletin_list) => {
                    row_count += bulletin_list.len();
                },
                RenderedComponent::Paragraph(paragraph) => {
                    row_count += paragraph.len();
                },
                RenderedComponent::HelpText(_text) => {}, // we ignore help text as it is outside
                                                          // the UI container
            }
        }
        row_count += self.components_to_render.len();
        row_count
    }
    pub fn render(&mut self, rows: usize, columns: usize, error: &Option<String>) {
        let base_x = columns.saturating_sub(self.ui_column_count()) / 2;
        let base_y = rows.saturating_sub(self.ui_row_count()) / 2;
        let mut current_y = base_y;
        if let Some(title) = &self.title {
            print_text_with_coordinates(
                title.clone(),
                base_x,
                current_y,
                Some(columns),
                Some(rows),
            );
            current_y += 2;
        }
        for rendered_component in &mut self.components_to_render {
            let is_help = match rendered_component {
                RenderedComponent::HelpText(_) => true,
                _ => false,
            };
            if is_help {
                if let Some(error) = error {
                    render_error(error, rows);
                    continue;
                }
            }
            let y = if is_help { rows } else { current_y };
            let columns = if is_help {
                columns
            } else {
                columns.saturating_sub(base_x * 2)
            };
            let rendered_rows = rendered_component.render(
                base_x,
                y,
                rows,
                columns,
                self.hovering_over_link,
                self.menu_item_is_selected,
            );
            current_y += rendered_rows + 1; // 1 for the line space between components
        }
    }
}

fn render_error(error: &str, y: usize) {
    print_text_with_coordinates(
        Text::new(format!("ERROR: {}", error)).color_range(3, ..),
        0,
        y,
        None,
        None,
    );
}

fn changelog_link_unselected(version: String) -> Text {
    let full_changelog_text = format!(
        "https://github.com/zellij-org/zellij/releases/tag/v{}",
        version
    );
    Text::new(full_changelog_text)
}

fn changelog_link_selected(version: String) -> Box<dyn Fn(usize, usize) -> usize> {
    Box::new(move |x, y| {
        print!(
            "\u{1b}[{};{}H\u{1b}[m\u{1b}[1;4mhttps://github.com/zellij-org/zellij/releases/tag/v{}",
            y + 1,
            x + 1,
            version
        );
        51 + version.chars().count()
    })
}

fn changelog_link_selected_len(version: String) -> Box<dyn Fn() -> usize> {
    Box::new(move || 51 + version.chars().count())
}

fn sponsors_link_text_unselected() -> Text {
    Text::new("https://github.com/sponsors/imsnif")
}

fn sponsors_link_text_selected(x: usize, y: usize) -> usize {
    print!(
        "\u{1b}[{};{}H\u{1b}[m\u{1b}[1;4mhttps://github.com/sponsors/imsnif",
        y + 1,
        x + 1
    );
    34
}

fn sponsors_link_text_selected_len() -> usize {
    34
}

fn cli_automation_link_selected(x: usize, y: usize) -> usize {
    print!(
        "\u{1b}[{};{}H\u{1b}[m\u{1b}[1;4mhttps://zellij.dev/documentation/controlling-zellij-through-cli.html",
        y + 1,
        x + 1
    );
    68
}

fn cli_automation_link_selected_len() -> usize {
    68
}

fn web_client_link_selected(x: usize, y: usize) -> usize {
    print!(
        "\u{1b}[{};{}H\u{1b}[m\u{1b}[1;4mhttps://zellij.dev/tutorials/web-client/",
        y + 1,
        x + 1
    );
    40
}

fn web_client_link_selected_len() -> usize {
    40
}

// Text components
fn whats_new_title() -> Text {
    Text::new("What's new?")
}

fn main_screen_title(version: String, is_release_notes: bool) -> Text {
    if is_release_notes {
        let title_text = format!("Hi there, welcome to VibeCrafted Shell {}!", &version);
        Text::new(title_text).color_range(2, 21..=38 + version.chars().count())
    } else {
        let title_text = format!("VibeCrafted Shell {}", &version);
        Text::new(title_text).color_range(2, ..)
    }
}

fn main_screen_help_text(hovering_over_link: bool, menu_item_is_selected: bool) -> Text {
    if hovering_over_link {
        let help_text = "Help: Click or Shift-Click to open in browser".to_string();
        Text::new(help_text)
            .color_range(3, 6..=10)
            .color_range(3, 15..=25)
    } else if menu_item_is_selected {
        let help_text = "Help: <↓↑> - Navigate, <ENTER> - Learn More, <ESC> - Dismiss".to_string();
        Text::new(help_text)
            .color_range(1, 6..=9)
            .color_range(1, 23..=29)
            .color_range(1, 45..=49)
    } else {
        let help_text = "Help: <↓↑> - Navigate, <ESC> - Dismiss, <?> - Usage Tips".to_string();
        Text::new(help_text)
            .color_range(1, 6..=9)
            .color_range(1, 23..=27)
            .color_range(1, 40..=42)
    }
}

fn release_notes_main_help(hovering_over_link: bool, menu_item_is_selected: bool) -> Text {
    if hovering_over_link {
        let help_text = "Help: Click or Shift-Click to open in browser".to_string();
        Text::new(help_text)
            .color_range(3, 6..=10)
            .color_range(3, 15..=25)
    } else if menu_item_is_selected {
        let help_text = "Help: <↓↑> - Navigate, <ENTER> - Learn More, <ESC> - Dismiss".to_string();
        Text::new(help_text)
            .color_range(1, 6..=9)
            .color_range(1, 23..=29)
            .color_range(1, 45..=49)
    } else {
        let help_text = "Help: <↓↑> - Navigate, <ESC> - Dismiss".to_string();
        Text::new(help_text)
            .color_range(1, 6..=9)
            .color_range(1, 23..=27)
    }
}

fn esc_go_back_plus_link_hover(hovering_over_link: bool, _menu_item_is_selected: bool) -> Text {
    if hovering_over_link {
        let help_text = "Help: Click or Shift-Click to open in browser".to_string();
        Text::new(help_text)
            .color_range(3, 6..=10)
            .color_range(3, 15..=25)
    } else {
        let help_text = "Help: <ESC> - Go back".to_string();
        Text::new(help_text).color_range(1, 6..=10)
    }
}

fn esc_to_go_back_help() -> Text {
    let help_text = "Help: <ESC> - Go back".to_string();
    Text::new(help_text).color_range(1, 6..=10)
}

fn main_menu_item(item_name: &str) -> Text {
    Text::new(item_name).color_range(0, ..)
}

fn support_the_developer_text() -> Text {
    let support_text = "Please support the VibeCrafted / Zellij craft <3: ".to_string();
    Text::new(support_text).color_range(3, ..)
}

pub enum TextOrCustomRender {
    Text(Text),
    CustomRender(
        Box<dyn Fn(usize, usize) -> usize>, // (rows, columns) -> text_len (render function)
        Box<dyn Fn() -> usize>,             // length of rendered component
    ),
}

impl TextOrCustomRender {
    pub fn len(&self) -> usize {
        match self {
            TextOrCustomRender::Text(text) => text.len(),
            TextOrCustomRender::CustomRender(_render_fn, len_fn) => len_fn(),
        }
    }
    pub fn render(&mut self, x: usize, y: usize, rows: usize, columns: usize) -> usize {
        match self {
            TextOrCustomRender::Text(text) => {
                print_text_with_coordinates(text.clone(), x, y, Some(columns), Some(rows));
                text.len()
            },
            TextOrCustomRender::CustomRender(render_fn, _len_fn) => render_fn(x, y),
        }
    }
}

impl std::fmt::Debug for TextOrCustomRender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TextOrCustomRender::Text(text) => write!(f, "Text {{ {:?} }}", text),
            TextOrCustomRender::CustomRender(..) => write!(f, "CustomRender"),
        }
    }
}

enum RenderedComponent {
    HelpText(Box<dyn Fn(bool, bool) -> Text>),
    BulletinList(BulletinList),
    Paragraph(Vec<ComponentLine>),
}

impl std::fmt::Debug for RenderedComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RenderedComponent::HelpText(_) => write!(f, "HelpText"),
            RenderedComponent::BulletinList(bulletinlist) => write!(f, "{:?}", bulletinlist),
            RenderedComponent::Paragraph(component_list) => write!(f, "{:?}", component_list),
        }
    }
}

impl RenderedComponent {
    pub fn render(
        &mut self,
        x: usize,
        y: usize,
        rows: usize,
        columns: usize,
        hovering_over_link: bool,
        menu_item_is_selected: bool,
    ) -> usize {
        let mut rendered_rows = 0;
        match self {
            RenderedComponent::HelpText(text) => {
                rendered_rows += 1;
                print_text_with_coordinates(
                    text(hovering_over_link, menu_item_is_selected),
                    0,
                    y,
                    Some(columns),
                    Some(rows),
                );
            },
            RenderedComponent::BulletinList(bulletin_list) => {
                rendered_rows += bulletin_list.len();
                bulletin_list.render(x, y, rows, columns);
            },
            RenderedComponent::Paragraph(paragraph) => {
                let mut paragraph_rendered_rows = 0;
                for component_line in paragraph {
                    component_line.render(
                        x,
                        y + paragraph_rendered_rows,
                        rows.saturating_sub(paragraph_rendered_rows),
                        columns,
                    );
                    rendered_rows += 1;
                    paragraph_rendered_rows += 1;
                }
            },
        }
        rendered_rows
    }
}

#[derive(Debug)]
pub struct BulletinList {
    title: Text,
    items: Vec<ActiveComponent>,
}

impl BulletinList {
    pub fn new(title: Text) -> Self {
        BulletinList {
            title,
            items: vec![],
        }
    }
    pub fn with_items(mut self, items: Vec<ActiveComponent>) -> Self {
        self.items = items;
        self
    }
    pub fn len(&self) -> usize {
        self.items.len() + 1 // 1 for the title
    }
    pub fn column_count(&self) -> usize {
        let mut column_count = 0;
        for item in &self.items {
            column_count = std::cmp::max(column_count, item.column_count());
        }
        column_count
    }
    pub fn handle_left_click_at_position(&mut self, x: usize, y: usize) -> Option<Page> {
        for component in &mut self.items {
            let page_to_render = component.handle_left_click_at_position(x, y);
            if page_to_render.is_some() {
                return page_to_render;
            }
        }
        None
    }
    pub fn handle_selection(&mut self) -> Option<Page> {
        for component in &mut self.items {
            let page_to_render = component.handle_selection();
            if page_to_render.is_some() {
                return page_to_render;
            }
        }
        None
    }
    pub fn handle_hover_at_position(&mut self, x: usize, y: usize) -> bool {
        for component in &mut self.items {
            let should_render = component.handle_hover_at_position(x, y);
            if should_render {
                return should_render;
            }
        }
        false
    }
    pub fn clear_hover(&mut self) {
        for component in &mut self.items {
            component.clear_hover();
        }
    }
    pub fn active_component_position(&self) -> Option<usize> {
        self.items.iter().position(|i| i.is_active)
    }
    pub fn clear_active_bulletins(&mut self) {
        self.items.iter_mut().for_each(|i| {
            i.is_active = false;
        });
    }
    pub fn set_active_bulletin(&mut self, new_index: usize) {
        if let Some(i) = self.items.get_mut(new_index) { i.is_active = true; }
    }
    pub fn set_last_active_bulletin(&mut self) {
        if let Some(i) = self.items.last_mut() { i.is_active = true; }
    }
    pub fn render(&mut self, x: usize, y: usize, rows: usize, columns: usize) {
        print_text_with_coordinates(self.title.clone(), x, y, Some(columns), Some(rows));
        let mut item_bulletin = 1;
        let mut running_y = y + 1;
        for item in &mut self.items {
            let mut item_bulletin_text = Text::new(format!("{}. ", item_bulletin));
            if item.is_active {
                item_bulletin_text = item_bulletin_text.selected();
            }
            let item_bulletin_text_len = item_bulletin_text.len();
            print_text_with_coordinates(
                item_bulletin_text,
                x,
                running_y,
                Some(item_bulletin_text_len),
                Some(rows),
            );
            item.render(
                x + item_bulletin_text_len,
                running_y,
                rows,
                columns.saturating_sub(item_bulletin_text_len),
            );
            running_y += 1;
            item_bulletin += 1;
        }
    }
}

#[derive(Debug)]
pub struct ComponentLine {
    components: Vec<ActiveComponent>,
}

impl ComponentLine {
    pub fn handle_left_click_at_position(&mut self, x: usize, y: usize) -> Option<Page> {
        for active_component in &mut self.components {
            let page_to_render = active_component.handle_left_click_at_position(x, y);
            if page_to_render.is_some() {
                return page_to_render;
            }
        }
        None
    }
    pub fn handle_hover_at_position(&mut self, x: usize, y: usize) -> bool {
        for active_component in &mut self.components {
            let should_render = active_component.handle_hover_at_position(x, y);
            if should_render {
                return should_render;
            }
        }
        false
    }
    pub fn clear_hover(&mut self) {
        for active_component in &mut self.components {
            active_component.clear_hover();
        }
    }
    pub fn column_count(&self) -> usize {
        let mut column_count = 0;
        for active_component in &self.components {
            column_count += active_component.column_count()
        }
        column_count
    }
    pub fn render(&mut self, x: usize, y: usize, rows: usize, columns: usize) {
        let mut current_x = x;
        let mut columns_left = columns;
        for component in &mut self.components {
            let component_len = component.render(current_x, y, rows, columns_left);
            current_x += component_len;
            columns_left = columns_left.saturating_sub(component_len);
        }
    }
}

impl ComponentLine {
    pub fn new(components: Vec<ActiveComponent>) -> Self {
        ComponentLine { components }
    }
}
