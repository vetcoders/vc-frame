//! Context tools share one visible layer. PaneManifest remains the inventory;
//! hiding a tool suppresses its pane without closing its process or draft.
use zellij_tile::prelude::*;

use super::{COMPOSER_PANE_NAME, PANEL_DRAWER_TITLE, QUICK_CMD_PANE_NAME};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextLayer {
    QuickCmd,
    Composer,
    Panels,
    Help,
}

impl ContextLayer {
    pub fn of(pane: &PaneInfo) -> Option<Self> {
        if pane.is_plugin {
            if pane.plugin_url.as_deref().and_then(panels_chrome_plugin) != Some("compact-bar") {
                return None;
            }
            if pane.title == PANEL_DRAWER_TITLE {
                Some(Self::Panels)
            } else if pane.is_floating {
                // The other floating compact-bar instance is mode help.
                Some(Self::Help)
            } else {
                None
            }
        } else {
            match pane.title.as_str() {
                QUICK_CMD_PANE_NAME => Some(Self::QuickCmd),
                COMPOSER_PANE_NAME => Some(Self::Composer),
                _ => None,
            }
        }
    }
}

fn pane_id(pane: &PaneInfo) -> PaneId {
    if pane.is_plugin {
        PaneId::Plugin(pane.id)
    } else {
        PaneId::Terminal(pane.id)
    }
}

/// Prepare an explicit tool entry before requesting its open/show operation.
pub fn competing_layers(panes: &[PaneInfo], entering: ContextLayer) -> Vec<PaneId> {
    panes
        .iter()
        .filter(|pane| {
            pane.is_floating
                && !pane.is_suppressed
                && ContextLayer::of(pane).is_some_and(|layer| layer != entering)
        })
        .map(pane_id)
        .collect()
}

/// Reconcile server focus after asynchronous opens (including keyboard Composer)
/// and panel selection. Never infer focus from z-order or a stale local registry.
pub fn contextual_panes_to_hide(panes: &[PaneInfo]) -> Vec<PaneId> {
    let Some(focused) = panes.iter().find(|pane| {
        pane.is_focused
            && pane.is_floating
            && !pane.is_suppressed
            && ContextLayer::of(pane).is_some()
    }) else {
        return vec![];
    };
    let keep = pane_id(focused);
    panes
        .iter()
        .filter(|pane| {
            pane.is_floating
                && !pane.is_suppressed
                && pane_id(pane) != keep
                && ContextLayer::of(pane).is_some()
        })
        .map(pane_id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(id: u32, title: &str) -> PaneInfo {
        PaneInfo {
            id,
            title: title.into(),
            is_floating: true,
            ..Default::default()
        }
    }

    #[test]
    fn drawer_hides_quick_cmd_and_composer_but_preserves_the_work_surface() {
        let mut drawer = tool(1, PANEL_DRAWER_TITLE);
        drawer.is_plugin = true;
        drawer.plugin_url = Some("vc-frame:compact-bar".into());
        drawer.is_focused = true;
        let panes = vec![
            drawer,
            tool(1, QUICK_CMD_PANE_NAME),
            tool(2, COMPOSER_PANE_NAME),
            tool(3, "Voc ZEN"),
            tool(4, "agent"),
        ];
        assert_eq!(
            contextual_panes_to_hide(&panes),
            vec![PaneId::Terminal(1), PaneId::Terminal(2)]
        );
    }

    #[test]
    fn explicit_entry_hides_peers_before_open_and_keeps_existing_shell() {
        assert_eq!(
            competing_layers(
                &[tool(1, QUICK_CMD_PANE_NAME), tool(2, COMPOSER_PANE_NAME)],
                ContextLayer::QuickCmd
            ),
            vec![PaneId::Terminal(2)]
        );
    }

    #[test]
    fn suppressed_tools_do_not_cause_repeated_hide_events() {
        let mut quick = tool(1, QUICK_CMD_PANE_NAME);
        quick.is_focused = true;
        let mut composer = tool(2, COMPOSER_PANE_NAME);
        composer.is_suppressed = true;
        assert!(contextual_panes_to_hide(&[quick, composer]).is_empty());
    }

    #[test]
    fn normal_focus_and_unrelated_plugins_do_not_manage_layers() {
        let mut custom = tool(1, PANEL_DRAWER_TITLE);
        custom.is_plugin = true;
        custom.plugin_url = Some("file:/tmp/other.wasm".into());
        custom.is_focused = true;
        assert!(contextual_panes_to_hide(&[custom, tool(2, QUICK_CMD_PANE_NAME)]).is_empty());
    }

    #[test]
    fn floating_help_is_dismissed_by_a_command_but_tiled_chrome_is_preserved() {
        let mut help = tool(1, "Tab");
        help.is_plugin = true;
        help.plugin_url = Some("zellij:compact-bar".into());
        let mut bar = help.clone();
        bar.id = 2;
        bar.is_floating = false;
        assert_eq!(
            competing_layers(&[help, bar], ContextLayer::QuickCmd),
            vec![PaneId::Plugin(1)]
        );
    }
}
