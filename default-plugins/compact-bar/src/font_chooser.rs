//! Aa is a route to the Agents-owned chooser, never another font implementation.
use std::collections::BTreeMap;
use zellij_tile::prelude::*;

const TITLE: &str = "Aa · Terminal Typography";
const RUNNER: &str = include_str!("../assets/font-chooser.sh");

pub(super) trait FontChooserHost {
    fn open_font_chooser(&mut self) -> Option<u32>;
    fn focus_font_chooser(&mut self, pane_id: u32);
}

#[derive(Default)]
pub(super) struct FontChooser {
    pending: BTreeMap<u32, usize>,
}

impl FontChooser {
    pub(super) fn reconcile(&mut self, manifest: &PaneManifest) {
        // Snapshots can precede publication or skip the renamed pane entirely.
        // Absence is not closure: PaneClosed is the host's semantic barrier.
        // Until the title is published, follow the ID across tab reordering.
        self.pending.retain(|id, tab| {
            if let Some((position, pane)) = manifest.panes.iter().find_map(|(position, panes)| {
                panes
                    .iter()
                    .find(|pane| pane.id == *id && !pane.is_plugin)
                    .map(|pane| (*position, pane))
            }) {
                *tab = position;
                !pane.exited && pane.title != TITLE
            } else {
                true
            }
        });
    }

    pub(super) fn pane_closed(&mut self, pane_id: PaneId) {
        if let PaneId::Terminal(id) = pane_id {
            self.pending.remove(&id);
        }
    }

    pub(super) fn open_or_focus(
        &mut self,
        manifest: Option<&PaneManifest>,
        tab_position: usize,
        host: &mut impl FontChooserHost,
    ) {
        if let Some(manifest) = manifest {
            self.reconcile(manifest);
        }
        let live = manifest
            .and_then(|manifest| manifest.panes.get(&tab_position))
            .and_then(|panes| {
                panes
                    .iter()
                    .find(|pane| !pane.is_plugin && !pane.exited && pane.title == TITLE)
            })
            .map(|pane| pane.id);
        let pending = self
            .pending
            .iter()
            .find_map(|(id, tab)| (*tab == tab_position).then_some(*id));
        if let Some(id) = live.or(pending) {
            host.focus_font_chooser(id);
        } else if let Some(id) = host.open_font_chooser() {
            self.pending.insert(id, tab_position);
        }
    }
}

fn coordinates() -> Option<FloatingPaneCoordinates> {
    FloatingPaneCoordinates::new(
        Some("10%".to_owned()),
        Some("7%".to_owned()),
        Some("80%".to_owned()),
        Some("85%".to_owned()),
        Some(false),
        None,
    )
}

impl FontChooserHost for super::ZellijVocPaneHost {
    fn open_font_chooser(&mut self) -> Option<u32> {
        let command = CommandToRun::new_with_args("sh", vec!["-c", RUNNER]);
        let Some(PaneId::Terminal(id)) =
            open_command_pane_floating(command, coordinates(), BTreeMap::new())
        else {
            return None;
        };
        rename_terminal_pane(id, TITLE);
        switch_to_input_mode(&InputMode::Normal);
        Some(id)
    }

    fn focus_font_chooser(&mut self, pane_id: u32) {
        show_pane_with_id(PaneId::Terminal(pane_id), true, true);
        if let Some(coordinates) = coordinates() {
            change_floating_panes_coordinates(vec![(PaneId::Terminal(pane_id), coordinates)]);
        }
        switch_to_input_mode(&InputMode::Normal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Host {
        opens: usize,
        focused: Vec<u32>,
        fail: bool,
    }
    impl FontChooserHost for Host {
        fn open_font_chooser(&mut self) -> Option<u32> {
            self.opens += 1;
            (!self.fail).then_some(7)
        }
        fn focus_font_chooser(&mut self, id: u32) {
            self.focused.push(id);
        }
    }

    fn manifest(tab: usize, exited: bool, plugin: bool) -> PaneManifest {
        let mut manifest = PaneManifest::default();
        manifest.panes.insert(
            tab,
            vec![PaneInfo {
                id: 7,
                title: TITLE.to_owned(),
                exited,
                is_plugin: plugin,
                ..Default::default()
            }],
        );
        manifest
    }

    #[test]
    fn repeat_click_and_reload_focus_live_chooser_and_closed_chooser_reopens() {
        let mut chooser = FontChooser::default();
        let mut host = Host::default();
        chooser.open_or_focus(None, 0, &mut host);
        chooser.open_or_focus(None, 0, &mut host);
        assert_eq!(host.opens, 1);
        assert_eq!(host.focused, vec![7]);
        let mut not_renamed_yet = manifest(0, false, false);
        not_renamed_yet.panes.get_mut(&0).unwrap()[0].title = "sh".to_owned();
        chooser.reconcile(&not_renamed_yet);
        chooser.open_or_focus(Some(&not_renamed_yet), 0, &mut host);
        assert_eq!(host.opens, 1, "pane open must survive a delayed rename");
        assert_eq!(host.focused, vec![7, 7]);
        let live = manifest(0, false, false);
        chooser.reconcile(&live);
        assert_eq!(host.focused, vec![7, 7], "updates never steal focus");
        chooser.open_or_focus(Some(&live), 0, &mut host);
        let mut reloaded = FontChooser::default();
        reloaded.open_or_focus(Some(&live), 0, &mut host);
        assert_eq!(host.opens, 1);
        chooser.reconcile(&PaneManifest::default());
        chooser.open_or_focus(Some(&PaneManifest::default()), 0, &mut host);
        assert_eq!(host.opens, 2);
    }

    #[test]
    fn wrong_tab_exited_plugin_and_failed_open_never_become_focus_targets() {
        for manifest in [
            manifest(1, false, false),
            manifest(0, true, false),
            manifest(0, false, true),
        ] {
            let mut chooser = FontChooser::default();
            let mut host = Host {
                fail: true,
                ..Default::default()
            };
            chooser.reconcile(&manifest);
            chooser.open_or_focus(Some(&manifest), 0, &mut host);
            chooser.open_or_focus(Some(&manifest), 0, &mut host);
            assert_eq!(host.opens, 2);
            assert!(host.focused.is_empty());
        }
    }
}
