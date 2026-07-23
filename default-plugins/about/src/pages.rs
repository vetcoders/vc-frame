use zellij_tile::prelude::*;

pub const VC_FRAME_REPOSITORY_URL: &str = "https://github.com/vetcoders/vc-frame";

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
        base_mode: Rc<RefCell<InputMode>>,
        is_release_notes: bool,
    ) -> Self {
        Page::new()
            .main_screen()
            .with_title(main_screen_title(zellij_version.clone(), is_release_notes))
            .with_bulletin_list(BulletinList::new(whats_new_title()).with_items(vec![
                    ActiveComponent::new(TextOrCustomRender::Text(main_menu_item(
                        "Live Agent Session Rail",
                    )))
                    .with_hover(TextOrCustomRender::Text(
                        main_menu_item("Live Agent Session Rail").selected(),
                    ))
                    .with_left_click_action(ClickAction::new_change_page({
                        let link_executable = link_executable.clone();
                        let zellij_version = zellij_version.clone();
                        let base_mode = base_mode.clone();
                        move || {
                            Page::new_vibecrafted_mission_control(
                                link_executable.clone(),
                                zellij_version.clone(),
                                base_mode.clone(),
                            )
                        }
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
                    ActiveComponent::new(TextOrCustomRender::Text(main_menu_item(
                        "Windows Support",
                    )))
                    .with_hover(TextOrCustomRender::Text(
                        main_menu_item("Windows Support").selected(),
                    ))
                    .with_left_click_action(ClickAction::new_change_page({
                        move || Page::new_windows_support()
                    })),
                ]))
            .with_paragraph(vec![ComponentLine::new(vec![
                ActiveComponent::new(TextOrCustomRender::Text(Text::new("VC Frame release: "))),
                ActiveComponent::new(TextOrCustomRender::Text(changelog_link_unselected(
                    zellij_version.clone(),
                )))
                .with_hover(TextOrCustomRender::CustomRender(
                    Box::new(changelog_link_selected(zellij_version.clone())),
                    Box::new(changelog_link_selected_len(zellij_version.clone())),
                ))
                .with_left_click_action(ClickAction::new_open_link(
                    vc_frame_release_url(&zellij_version),
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
                        VC_FRAME_REPOSITORY_URL.to_owned(),
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
    /// First-run map for the vibecrafted operator layout (Guide / Start here tab).
    /// Written for people who have never used a multiplexor — plain labels, no jargon.
    pub fn new_vibecrafted_mission_control(
        link_executable: Rc<RefCell<String>>,
        zellij_version: String,
        base_mode: Rc<RefCell<InputMode>>,
    ) -> Self {
        Page::new()
            .main_screen()
            .with_title(Text::new("Start here — map of this workspace").color_range(0, ..))
            .with_paragraph(vec![
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new(format!(
                        "vc-frame {} · Vibecrafted operator layout",
                        zellij_version
                    )),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new(
                        "You are looking at ONE session (this window). It has a fixed chrome:",
                    ),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new(
                        "  TOP    = tabs of THIS session (Start here · Shell)",
                    )
                    .color_substring(3, "TOP")
                    .color_substring(2, "tabs of THIS session"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new(
                        "  LEFT   = SESSIONS rail — other sessions / agent rooms (click to jump)",
                    )
                    .color_substring(3, "LEFT")
                    .color_substring(2, "SESSIONS rail"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new(
                        "  CENTER = this Guide (help). Work happens on the Shell tab.",
                    )
                    .color_substring(3, "CENTER")
                    .color_substring(2, "Shell tab"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("  BOTTOM = status bar (modes: Ctrl+t TAB, Ctrl+p PANE, Ctrl+o SESSION)")
                        .color_substring(3, "BOTTOM")
                        .color_substring(2, "Ctrl+t")
                        .color_substring(2, "Ctrl+p")
                        .color_substring(2, "Ctrl+o"),
                ))]),
            ])
            .with_paragraph(vec![
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Do this first (60 seconds):").color_range(2, ..),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new(
                        "  1. Open the Shell tab — click \"Shell\" on the top bar, or: Ctrl+t then 2",
                    )
                    .color_substring(3, "Shell")
                    .color_substring(2, "Ctrl+t then 2"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new(
                        "  2. Read the banner in the shell, then run:  vibecrafted start",
                    )
                    .color_substring(3, "vibecrafted start"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new(
                        "  3. Come back here anytime (Ctrl+t then 1) if you get lost.",
                    )
                    .color_substring(2, "Ctrl+t then 1"),
                ))]),
            ])
            .with_bulletin_list(
                BulletinList::new(Text::new("Learn the chrome (click a topic):").color_range(2, ..))
                    .with_items(vec![
                        ActiveComponent::new(TextOrCustomRender::Text(main_menu_item(
                            "Left rail = sessions (not tabs)",
                        )))
                        .with_hover(TextOrCustomRender::Text(
                            main_menu_item("Left rail = sessions (not tabs)").selected(),
                        ))
                        .with_left_click_action(ClickAction::new_change_page({
                            let link_executable = link_executable.clone();
                            let zellij_version = zellij_version.clone();
                            let base_mode = base_mode.clone();
                            move || {
                                Page::new_onboarding_sessions_rail(
                                    link_executable.clone(),
                                    zellij_version.clone(),
                                    base_mode.clone(),
                                )
                            }
                        })),
                        ActiveComponent::new(TextOrCustomRender::Text(main_menu_item(
                            "Top bar = tabs of this session",
                        )))
                        .with_hover(TextOrCustomRender::Text(
                            main_menu_item("Top bar = tabs of this session").selected(),
                        ))
                        .with_left_click_action(ClickAction::new_change_page({
                            let link_executable = link_executable.clone();
                            let zellij_version = zellij_version.clone();
                            let base_mode = base_mode.clone();
                            move || {
                                Page::new_onboarding_tabs(
                                    link_executable.clone(),
                                    zellij_version.clone(),
                                    base_mode.clone(),
                                )
                            }
                        })),
                        ActiveComponent::new(TextOrCustomRender::Text(main_menu_item(
                            "Keyboard + mouse cheat sheet",
                        )))
                        .with_hover(TextOrCustomRender::Text(
                            main_menu_item("Keyboard + mouse cheat sheet").selected(),
                        ))
                        .with_left_click_action(ClickAction::new_change_page({
                            let link_executable = link_executable.clone();
                            let zellij_version = zellij_version.clone();
                            let base_mode = base_mode.clone();
                            move || {
                                Page::new_onboarding_keyboard(
                                    link_executable.clone(),
                                    zellij_version.clone(),
                                    base_mode.clone(),
                                )
                            }
                        })),
                        ActiveComponent::new(TextOrCustomRender::Text(main_menu_item(
                            "What to type on the Shell tab",
                        )))
                        .with_hover(TextOrCustomRender::Text(
                            main_menu_item("What to type on the Shell tab").selected(),
                        ))
                        .with_left_click_action(ClickAction::new_change_page({
                            let link_executable = link_executable.clone();
                            let zellij_version = zellij_version.clone();
                            let base_mode = base_mode.clone();
                            move || {
                                Page::new_onboarding_shell_commands(
                                    link_executable.clone(),
                                    zellij_version.clone(),
                                    base_mode.clone(),
                                )
                            }
                        })),
                        ActiveComponent::new(TextOrCustomRender::Text(main_menu_item(
                            "Advanced tools (optional)",
                        )))
                        .with_hover(TextOrCustomRender::Text(
                            main_menu_item("Advanced tools (optional)").selected(),
                        ))
                        .with_left_click_action(ClickAction::new_change_page({
                            let link_executable = link_executable.clone();
                            let zellij_version = zellij_version.clone();
                            let base_mode = base_mode.clone();
                            move || {
                                Page::new_onboarding_advanced_tools(
                                    link_executable.clone(),
                                    zellij_version.clone(),
                                    base_mode.clone(),
                                )
                            }
                        })),
                    ]),
            )
            .with_help(Box::new(|hovering_over_link, menu_item_is_selected| {
                main_screen_help_text(hovering_over_link, menu_item_is_selected)
            }))
    }

    fn new_onboarding_sessions_rail(
        link_executable: Rc<RefCell<String>>,
        zellij_version: String,
        base_mode: Rc<RefCell<InputMode>>,
    ) -> Self {
        Page::new()
            .with_title(Text::new("Left rail = SESSIONS (other workspaces)").color_range(0, ..))
            .with_paragraph(vec![
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new(
                        "Each row on the left is a full session: its own tabs, panes, and often",
                    ),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new(
                        "its own agents. It is NOT a tab of this window — it is another room.",
                    )
                    .color_substring(3, "NOT a tab")
                    .color_substring(2, "another room"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("  · Click a session name  →  switch into that room"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("  · Click a live process under a session  →  jump to that tab"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new(
                        "  · Keys f / x / n  →  open drawers Finalized / Failed / Needs attention",
                    )
                    .color_substring(3, "f / x / n"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new(
                        "  · Empty drawer still opens (creates the session) — like opening a folder",
                    ),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new(
                        "  · Hover lights a row when the pointer is over the rail (no focus steal)",
                    ),
                ))]),
            ])
            .with_paragraph(vec![ComponentLine::new(vec![ActiveComponent::new(
                TextOrCustomRender::Text(
                    Text::new("Back: press Esc  ·  or open topic list from Start here").color_range(
                        2,
                        ..,
                    ),
                ),
            )])])
            .with_help(Box::new(esc_go_back_plus_link_hover))
            .with_bulletin_list(onboarding_back_bulletin(
                link_executable,
                zellij_version,
                base_mode,
            ))
    }

    fn new_onboarding_tabs(
        link_executable: Rc<RefCell<String>>,
        zellij_version: String,
        base_mode: Rc<RefCell<InputMode>>,
    ) -> Self {
        Page::new()
            .with_title(Text::new("Top bar = tabs of THIS session").color_range(0, ..))
            .with_paragraph(vec![
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Tabs live only inside the session you are in right now."),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("In this layout you start with two:"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("  · Start here  —  this help map (you are here)")
                        .color_substring(3, "Start here"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("  · Shell       —  your terminal to run vibecrafted / agents")
                        .color_substring(3, "Shell"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("How to switch:"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("  · Mouse: click the tab name on the top compact bar"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("  · Keyboard: Ctrl+t  (TAB mode), then 1 / 2 or Left/Right, Enter")
                        .color_substring(2, "Ctrl+t"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("  · Ctrl+t then n  —  open a new tab (still this session)")
                        .color_substring(2, "Ctrl+t then n"),
                ))]),
            ])
            .with_bulletin_list(onboarding_back_bulletin(
                link_executable,
                zellij_version,
                base_mode,
            ))
            .with_help(Box::new(esc_go_back_plus_link_hover))
    }

    fn new_onboarding_keyboard(
        link_executable: Rc<RefCell<String>>,
        zellij_version: String,
        base_mode: Rc<RefCell<InputMode>>,
    ) -> Self {
        Page::new()
            .with_title(Text::new("Keyboard + mouse (plain Ctrl scheme)").color_range(0, ..))
            .with_paragraph(vec![
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Modes (status bar shows labels):").color_range(2, ..),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("  Ctrl+t  TAB     switch / create tabs")
                        .color_substring(3, "Ctrl+t"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("  Ctrl+p  PANE    split / close / full-screen panes")
                        .color_substring(3, "Ctrl+p"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("  Ctrl+o  SESSION attach / rename / kill session")
                        .color_substring(3, "Ctrl+o"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("  Ctrl+n  new pane immediately").color_substring(3, "Ctrl+n"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("  Ctrl+s  SCROLL  scrollback / copy").color_substring(3, "Ctrl+s"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("  Ctrl+g  LOCK    lock input (then Ctrl+arrows to move focus)")
                        .color_substring(3, "Ctrl+g"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("  Ctrl+q  close focused pane (does NOT quit the whole session)")
                        .color_substring(3, "Ctrl+q"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("Mouse:").color_range(2, ..),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("  · Click session / drawer on the left rail"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("  · Click tab names on the top bar"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("  · Scroll wheel over the rail moves selection"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("  · Esc dismisses floating UIs (layout manager, etc.)"),
                ))]),
            ])
            .with_bulletin_list(onboarding_back_bulletin(
                link_executable,
                zellij_version,
                base_mode,
            ))
            .with_help(Box::new(esc_go_back_plus_link_hover))
    }

    fn new_onboarding_shell_commands(
        link_executable: Rc<RefCell<String>>,
        zellij_version: String,
        base_mode: Rc<RefCell<InputMode>>,
    ) -> Self {
        Page::new()
            .with_title(Text::new("Shell tab — what to type").color_range(0, ..))
            .with_paragraph(vec![
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new(
                        "Open the Shell tab (top bar or Ctrl+t then 2). You land in a real shell",
                    )
                    .color_substring(2, "Ctrl+t then 2"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("with a short banner. Useful first commands:"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("  vibecrafted start      attach / open your operator flow")
                        .color_substring(3, "vibecrafted start"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("  vibecrafted help       list CLI surfaces")
                        .color_substring(3, "vibecrafted help"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("  vibecrafted dashboard vc-workflow   implementation workspace")
                        .color_substring(3, "vc-workflow"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("  vibecrafted dashboard vc-marbles    convergence surface")
                        .color_substring(3, "vc-marbles"),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new(
                        "The left SESSIONS rail still works from the Shell tab — other rooms stay one click away.",
                    )
                    .color_substring(2, "SESSIONS"),
                ))]),
            ])
            .with_bulletin_list(onboarding_back_bulletin(
                link_executable,
                zellij_version,
                base_mode,
            ))
            .with_help(Box::new(esc_go_back_plus_link_hover))
    }

    fn new_onboarding_advanced_tools(
        link_executable: Rc<RefCell<String>>,
        zellij_version: String,
        base_mode: Rc<RefCell<InputMode>>,
    ) -> Self {
        Page::new()
            .with_title(Text::new("Advanced tools (optional)").color_range(0, ..))
            .with_paragraph(vec![ComponentLine::new(vec![ActiveComponent::new(
                TextOrCustomRender::Text(Text::new(
                    "You do not need these on day one. They open floating control decks.",
                )),
            )])])
            .with_bulletin_list(
                BulletinList::new(Text::new("Open:").color_range(2, ..)).with_items(vec![
                    ActiveComponent::new(TextOrCustomRender::Text(main_menu_item("Session Atlas")))
                        .with_hover(TextOrCustomRender::Text(
                            main_menu_item("Session Atlas").selected(),
                        ))
                        .with_left_click_action(ClickAction::new_launch_plugin(
                            "vc-frame:session-manager".to_owned(),
                        )),
                    ActiveComponent::new(TextOrCustomRender::Text(main_menu_item("Layout Forge")))
                        .with_hover(TextOrCustomRender::Text(
                            main_menu_item("Layout Forge").selected(),
                        ))
                        .with_left_click_action(ClickAction::new_launch_plugin(
                            "vc-frame:layout-manager".to_owned(),
                        )),
                    ActiveComponent::new(TextOrCustomRender::Text(main_menu_item(
                        "Back to Start here",
                    )))
                    .with_hover(TextOrCustomRender::Text(
                        main_menu_item("Back to Start here").selected(),
                    ))
                    .with_left_click_action(ClickAction::new_change_page({
                        let link_executable = link_executable.clone();
                        let zellij_version = zellij_version.clone();
                        let base_mode = base_mode.clone();
                        move || {
                            Page::new_vibecrafted_mission_control(
                                link_executable.clone(),
                                zellij_version.clone(),
                                base_mode.clone(),
                            )
                        }
                    })),
                ]),
            )
            .with_help(Box::new(esc_go_back_plus_link_hover))
    }
    fn new_windows_support() -> Page {
        Page::new()
            .with_title(Text::new("Windows Support").color_range(0, ..))
            .with_paragraph(vec![
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("vc-frame now runs natively on Windows."),
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
                    Text::new("Attach to remote vc-frame sessions over HTTPS, directly from the terminal."),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("The remote session needs to be running the vc-frame web client."),
                ))]),
                ComponentLine::new(vec![ActiveComponent::new(TextOrCustomRender::Text(
                    Text::new("vc-frame will attach to it exactly as a browser would, through the same interface."),
                ))]),
            ])
            .with_bulletin_list(
                BulletinList::new(Text::new("Try it:").color_range(2, ..))
                    .with_items(vec![
                        ActiveComponent::new(TextOrCustomRender::Text(
                            Text::new("Run the vc-frame web server on one machine")
                                .color_substring(3, "vc-frame web server"),
                        ))
                        .with_hover(TextOrCustomRender::Text(
                            Text::new("Run the vc-frame web server on one machine")
                                .color_substring(3, "vc-frame web server")
                                .selected(),
                        ))
                        .with_left_click_action(ClickAction::new_launch_plugin(
                            "vc-frame:share".to_owned(),
                        )),
                        ActiveComponent::new(TextOrCustomRender::Text(
                            Text::new("From another: vc-frame attach https://<ip>/<session-name>")
                                .color_substring(3, "vc-frame attach")
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
                    Text::new("vc-frame web --create-read-only-token").color_range(3, ..),
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
                            Text::new("vc-frame run optionally blocks until success/failure")
                                .color_substring(3, "vc-frame run"),
                        )),
                        ActiveComponent::new(TextOrCustomRender::Text(
                            Text::new("vc-frame subscribe can stream pane scrollback in real time")
                                .color_substring(3, "vc-frame subscribe"),
                        )),
                        ActiveComponent::new(TextOrCustomRender::Text(
                            Text::new("vc-frame send-keys/paste can send human readable keys to other panes or sessions")
                                .color_substring(3, "vc-frame send-keys/paste"),
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
                    Text::new("vc-frame now detects file paths in the terminal viewport."),
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
                        "vc-frame:layout-manager".to_owned(),
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
                    Text::new("vc-frame plugin -- zellij:layout-manager").color_range(3, ..),
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
    /// Drop all component hover highlights. Used on cursor-leave (line < 0).
    pub fn clear_hover(&mut self) -> bool {
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
            let is_help = matches!(rendered_component, RenderedComponent::HelpText(_));
            if is_help && let Some(error) = error {
                render_error(error, rows);
                continue;
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
    Text::new(vc_frame_release_url(&version))
}

fn changelog_link_selected(version: String) -> Box<dyn Fn(usize, usize) -> usize> {
    Box::new(move |x, y| {
        let release_url = vc_frame_release_url(&version);
        print!(
            "\u{1b}[{};{}H\u{1b}[m\u{1b}[1;4m{}",
            y + 1,
            x + 1,
            release_url
        );
        release_url.chars().count()
    })
}

fn changelog_link_selected_len(version: String) -> Box<dyn Fn() -> usize> {
    Box::new(move || vc_frame_release_url(&version).chars().count())
}

fn sponsors_link_text_unselected() -> Text {
    Text::new(VC_FRAME_REPOSITORY_URL)
}

fn sponsors_link_text_selected(x: usize, y: usize) -> usize {
    print!(
        "\u{1b}[{};{}H\u{1b}[m\u{1b}[1;4m{}",
        y + 1,
        x + 1,
        VC_FRAME_REPOSITORY_URL
    );
    VC_FRAME_REPOSITORY_URL.chars().count()
}

fn sponsors_link_text_selected_len() -> usize {
    VC_FRAME_REPOSITORY_URL.chars().count()
}

fn vc_frame_release_url(version: &str) -> String {
    format!("{VC_FRAME_REPOSITORY_URL}/releases/tag/v{version}")
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
    Text::new("Operator surfaces")
}

fn main_screen_title(version: String, is_release_notes: bool) -> Text {
    if is_release_notes {
        let title_text = format!(
            "Hi there, welcome to vc-frame ⚒ (vibecrafted runtime) {}!",
            &version
        );
        Text::new(title_text).color_range(2, 21..=56 + version.chars().count())
    } else {
        let title_text = format!("vc-frame ⚒ (vibecrafted runtime) {}", &version);
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
    let support_text = "Source, issues, and the vc-frame craft: ".to_string();
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
                for (paragraph_rendered_rows, component_line) in paragraph.iter_mut().enumerate() {
                    component_line.render(
                        x,
                        y + paragraph_rendered_rows,
                        rows.saturating_sub(paragraph_rendered_rows),
                        columns,
                    );
                    rendered_rows += 1;
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
        if let Some(i) = self.items.get_mut(new_index) {
            i.is_active = true;
        }
    }
    pub fn set_last_active_bulletin(&mut self) {
        if let Some(i) = self.items.last_mut() {
            i.is_active = true;
        }
    }
    pub fn render(&mut self, x: usize, y: usize, rows: usize, columns: usize) {
        print_text_with_coordinates(self.title.clone(), x, y, Some(columns), Some(rows));
        for (idx, item) in self.items.iter_mut().enumerate() {
            let item_bulletin = idx + 1;
            let running_y = y + 1 + idx;
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

fn onboarding_back_bulletin(
    link_executable: Rc<RefCell<String>>,
    zellij_version: String,
    base_mode: Rc<RefCell<InputMode>>,
) -> BulletinList {
    BulletinList::new(Text::new("Navigate:").color_range(2, ..)).with_items(vec![
        ActiveComponent::new(TextOrCustomRender::Text(main_menu_item(
            "Back to Start here map",
        )))
        .with_hover(TextOrCustomRender::Text(
            main_menu_item("Back to Start here map").selected(),
        ))
        .with_left_click_action(ClickAction::new_change_page(move || {
            Page::new_vibecrafted_mission_control(
                link_executable.clone(),
                zellij_version.clone(),
                base_mode.clone(),
            )
        })),
    ])
}

#[cfg(test)]
mod product_identity_tests {
    use super::*;

    #[test]
    fn about_links_to_vc_frame_release_and_repository() {
        assert_eq!(
            vc_frame_release_url("0.45.4"),
            "https://github.com/vetcoders/vc-frame/releases/tag/v0.45.4"
        );
        assert_eq!(
            VC_FRAME_REPOSITORY_URL,
            "https://github.com/vetcoders/vc-frame"
        );
    }

    #[test]
    fn about_surface_contains_no_upstream_owner_or_sponsor_links() {
        let source = concat!(include_str!("pages.rs"), include_str!("tips.rs"));
        let upstream_release_owner = ["zellij-org", "zellij", "releases"].join("/");
        let upstream_sponsor = ["sponsors", "imsnif"].join("/");

        assert!(!source.contains(&upstream_release_owner));
        assert!(!source.contains(&upstream_sponsor));
    }

    #[test]
    fn first_run_guide_teaches_chrome_not_jargon() {
        // Source contract: the mission-control / Start here page must orient
        // a newcomer to SESSIONS rail, tabs, and how to reach the Shell tab.
        let source = include_str!("pages.rs");
        for needle in [
            "Start here — map of this workspace",
            "SESSIONS rail",
            "Ctrl+t then 2",
            "vibecrafted start",
            "Left rail = sessions (not tabs)",
            "Top bar = tabs of this session",
            "Keyboard + mouse cheat sheet",
            "What to type on the Shell tab",
        ] {
            assert!(
                source.contains(needle),
                "first-run guide missing orientation copy: {needle}"
            );
        }
        // Old useless marketing line must stay gone (check production page body).
        let mission_fn = source
            .split("pub fn new_vibecrafted_mission_control")
            .nth(1)
            .and_then(|s| s.split("fn new_onboarding_sessions_rail").next())
            .unwrap_or("");
        assert!(
            !mission_fn.contains("branded shell-provider surface"),
            "mission-control page regressed to marketing jargon"
        );
    }
}
