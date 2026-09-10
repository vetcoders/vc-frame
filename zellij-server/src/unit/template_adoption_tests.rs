use super::*;
use crate::screen::{
    CommittedOverrideLayout, IndeterminatePreparedLayout, LayoutCoordination,
    LayoutReconciliationIntent, LayoutReconciliationPlan,
    ReconcileIndeterminateLayoutTransactionParams, ScreenLayoutDecision, StagedTemplateAdoption,
};
use crate::tab::{OverrideLayoutOptions, TabLayoutTransaction};
use zellij_utils::input::actions::TemplateAdoption;
use zellij_utils::input::layout::CanvasLayoutPhase;

fn candidate() -> Layout {
    Layout::from_str(
        r#"
        layout {
            session_layer {
                pane size=1 borderless=true {
                    plugin location="compact-bar" {
                        session_canvas true
                        session_canvas_kind "compact-bar"
                    }
                }
                pane { children; }
            }
            default_tab_template { pane name="future-content"; }
            tab name="migrated" { pane; }
        }
    "#,
        "adoption-test".into(),
        None,
        None,
    )
    .unwrap()
}

fn stage(screen: &mut Screen, ids: &[usize], layout: Layout) -> (u64, StagedTemplateAdoption) {
    let request = StagedTemplateAdoption {
        request: TemplateAdoption {
            request_id: (screen.last_adoption_request_id + 1).to_string(),
            expected_generation: screen.template_generation_token(),
            layout: Box::new(layout),
        },
        retain_terminals: true,
        retain_plugins: false,
    };
    let id = screen.reserve_layout_transaction_id();
    let owner = ActiveLayoutTransaction {
        template_adoption: Some(request.clone()),
        published_template_generation: None,
        kind: ScreenLayoutTransactionKind::Override,
        targets: ids
            .iter()
            .map(|id| LayoutTabOwner::capture(screen, *id))
            .collect(),
        created_pending_tabs: vec![],
        render_fenced_tabs: vec![],
        tabs_to_close_after_commit: vec![],
        moved_original_panes: vec![],
        generation: None,
    };
    screen.register_layout_transaction(id, owner).unwrap();
    (id, request)
}

fn prepare(screen: &mut Screen, tab_id: usize, layout: TiledPaneLayout) -> TabLayoutTransaction {
    let plugins = layout
        .extract_run_instructions()
        .into_iter()
        .filter_map(|run| match run {
            Some(Run::Plugin(plugin)) => Some((plugin, vec![100 + tab_id as u32])),
            _ => None,
        })
        .collect();
    screen
        .tabs
        .get_mut(&tab_id)
        .unwrap()
        .begin_override_layout(OverrideLayoutOptions {
            layout,
            floating_panes_layout: vec![],
            new_swap_tiled_layouts: Some(vec![]),
            new_swap_floating_layouts: Some(vec![]),
            new_terminal_ids: vec![],
            new_floating_terminal_ids: vec![],
            new_plugin_ids: plugins,
            retain_existing_terminal_panes: true,
            retain_existing_plugin_panes: false,
            client_id: 1,
            blocking_terminal: None,
        })
        .unwrap()
}

fn plan() -> LayoutReconciliationPlan {
    LayoutReconciliationPlan {
        intent: LayoutReconciliationIntent::Activate,
        expected_plugin_ids: vec![],
        resource_ids: vec![],
        preserve_pending_tab_on_rejection: false,
        close_fenced_tab_on_rejection: false,
        layout_generation: None,
    }
}

fn reconcile(screen: &mut Screen, id: u64, coordination: LayoutCoordination) {
    screen
        .reconcile_indeterminate_layout_transaction(ReconcileIndeterminateLayoutTransactionParams {
            transaction_id: id,
            coordination,
            pending_tab_ids: &mut HashSet::new(),
            durable_tab_layout_generations: &HashMap::new(),
            pending_tab_switches: &mut HashSet::new(),
            pending_events_waiting_for_client: &mut vec![],
            pending_events_waiting_for_tab: &mut vec![],
            plugin_loading_message_cache: &mut HashMap::new(),
        })
        .unwrap();
}

#[test]
fn template_adoption_publishes_only_after_complete_commit() {
    let mut screen = create_new_screen(
        Size {
            cols: 100,
            rows: 30,
        },
        true,
        true,
    );
    new_tab(&mut screen, 1, 0);
    new_tab(&mut screen, 2, 1);
    let old = screen.default_layout.clone();
    let generation = screen.template_generation_token();
    let (id, request) = stage(&mut screen, &[0, 1], candidate());
    let target = request.request.tabs()[0].tiled_layout.clone();
    let first = prepare(&mut screen, 0, target.clone());
    let second = prepare(&mut screen, 1, target);
    assert_eq!(screen.default_layout, old, "preparation cannot publish");
    let missing = screen.tabs.remove(&1).unwrap();
    let result = screen.commit_override_layout_state(id, vec![(0, first), (1, second)]);
    let CommittedOverrideLayout::Indeterminate {
        committed_effects,
        remaining_prepared,
        ..
    } = result
    else {
        panic!("missing later target must quarantine partial local commit")
    };
    assert_eq!(committed_effects.len(), 1);
    assert_eq!(remaining_prepared.len(), 1);
    assert_eq!(screen.default_layout, old);
    assert_eq!(screen.template_generation_token(), generation);
    assert!(screen.template_adoption_pending());
    assert!(
        screen
            .replay_template_adoption(&request)
            .unwrap()
            .1
            .contains("unresolved")
    );
    screen.tabs.insert(1, missing);
    screen.indeterminate_layout_transactions.insert(
        id,
        IndeterminatePreparedLayout::Override {
            prepared_layouts: remaining_prepared,
            created_tab_ids: vec![],
            plan: plan(),
        },
    );
    reconcile(&mut screen, id, LayoutCoordination::Commit);
    assert_eq!(screen.default_layout, request.request.layout);
    assert_eq!(screen.template_generation, 1);
    assert!(!screen.template_adoption_pending());
    assert!(screen.tabs[&0].has_pane_with_pid(&PaneId::Terminal(1)));
    assert!(screen.tabs[&1].has_pane_with_pid(&PaneId::Terminal(2)));
    reconcile(&mut screen, id, LayoutCoordination::Commit);
    assert_eq!(
        screen.template_generation, 1,
        "background replay cannot republish"
    );
    assert!(
        screen
            .replay_template_adoption(&request)
            .unwrap()
            .1
            .contains("Committed")
    );
}

#[test]
fn template_adoption_preserves_future_tab_shell() {
    let mut screen = create_new_screen(
        Size {
            cols: 100,
            rows: 30,
        },
        true,
        true,
    );
    new_tab(&mut screen, 1, 0);
    let (id, request) = stage(&mut screen, &[0], candidate());
    let prepared = prepare(
        &mut screen,
        0,
        request.request.tabs()[0].tiled_layout.clone(),
    );
    assert!(matches!(
        screen.commit_override_layout_state(id, vec![(0, prepared)]),
        CommittedOverrideLayout::Complete(_)
    ));
    let published = screen.template_generation_token();
    screen.finalize_template_adoption(id);
    assert_eq!(screen.template_generation_token(), published);
    let owner = screen.active_layout_transactions[&id].clone();
    screen.record_resolved_layout_transaction(id, &owner, vec![], ScreenLayoutDecision::Committed);
    screen.active_layout_transactions.remove(&id);
    let (future, floating) = screen.resolve_new_tab_layout(None, vec![]);
    assert_eq!(future, request.request.layout.new_tab().0);
    assert!(floating.is_empty());
    assert_eq!(future.canvas_phase, CanvasLayoutPhase::Materialized);
    let chrome = future
        .extract_run_instructions()
        .into_iter()
        .filter(|run| matches!(run, Some(Run::Plugin(_))))
        .count();
    assert_eq!(chrome, 1, "one shared shell role in each future tab view");
    assert!(screen.tabs[&0].has_pane_with_pid(&PaneId::Terminal(1)));
    // Use the existing persistence conversion/serializer; no blob writer change.
    let metadata = screen.get_layout_metadata(None, None);
    let (saved, _) =
        zellij_utils::session_serialization::serialize_session_layout(metadata.into()).unwrap();
    let restored = Layout::from_str(&saved, "restored".into(), None, None).unwrap();
    assert_eq!(restored.session_layer, request.request.layout.session_layer);
    assert_eq!(restored.new_tab(), request.request.layout.new_tab());
    assert_eq!(
        restored.tabs()[0].1.canvas_phase,
        CanvasLayoutPhase::Materialized
    );
}

#[test]
fn template_adoption_unknown_and_compensated_rejection_keep_originals() {
    let mut screen = create_new_screen(
        Size {
            cols: 100,
            rows: 30,
        },
        true,
        true,
    );
    new_tab(&mut screen, 1, 0);
    let old = screen.default_layout.clone();
    let (id, request) = stage(&mut screen, &[0], candidate());
    let prepared = prepare(
        &mut screen,
        0,
        request.request.tabs()[0].tiled_layout.clone(),
    );
    screen.indeterminate_layout_transactions.insert(
        id,
        IndeterminatePreparedLayout::Override {
            prepared_layouts: vec![(0, prepared)],
            created_tab_ids: vec![],
            plan: plan(),
        },
    );
    let result = screen.reconcile_indeterminate_layout_transaction(
        ReconcileIndeterminateLayoutTransactionParams {
            transaction_id: id,
            coordination: LayoutCoordination::Unknown("lost ACK".into()),
            pending_tab_ids: &mut HashSet::new(),
            durable_tab_layout_generations: &HashMap::new(),
            pending_tab_switches: &mut HashSet::new(),
            pending_events_waiting_for_client: &mut vec![],
            pending_events_waiting_for_tab: &mut vec![],
            plugin_loading_message_cache: &mut HashMap::new(),
        },
    );
    assert!(result.is_err());
    assert_eq!(screen.default_layout, old);
    assert!(screen.template_adoption_pending());
    reconcile(
        &mut screen,
        id,
        LayoutCoordination::Rollback("certified compensation".into()),
    );
    assert_eq!(screen.default_layout, old);
    assert_eq!(screen.template_generation, 0);
    assert!(!screen.template_adoption_pending());
    assert!(screen.tabs[&0].has_pane_with_pid(&PaneId::Terminal(1)));
    assert!(
        screen
            .replay_template_adoption(&request)
            .unwrap()
            .1
            .contains("Rejected")
    );
    // Evict the receipt: high-water identity must prevent automatic second override.
    screen.resolved_layout_transactions.clear();
    let mut owner = ActiveLayoutTransaction {
        template_adoption: Some(request),
        published_template_generation: None,
        kind: ScreenLayoutTransactionKind::Override,
        targets: vec![LayoutTabOwner::capture(&screen, 0)],
        created_pending_tabs: vec![],
        render_fenced_tabs: vec![],
        tabs_to_close_after_commit: vec![],
        moved_original_panes: vec![],
        generation: None,
    };
    assert!(
        screen
            .register_layout_transaction(id + 1, owner.clone())
            .is_err()
    );
    owner.template_adoption.as_mut().unwrap().request.request_id = "2".into();
    owner
        .template_adoption
        .as_mut()
        .unwrap()
        .request
        .expected_generation = "other-session:0".into();
    assert!(screen.register_layout_transaction(id + 1, owner).is_err());
    assert_eq!(screen.default_layout, old);
}

#[test]
fn template_adoption_excludes_topology_but_not_pty_output() {
    let mut screen = create_new_screen(
        Size {
            cols: 100,
            rows: 30,
        },
        true,
        true,
    );
    new_tab(&mut screen, 1, 0);
    let (id, _) = stage(&mut screen, &[0], candidate());
    let mut competitor = screen.active_layout_transactions[&id].clone();
    competitor.template_adoption = None;
    assert!(
        screen
            .register_layout_transaction(id + 1, competitor)
            .is_err()
    );
    for explicit in [None, Some(TiledPaneLayout::default())] {
        assert!(
            ScreenInstruction::NewTab(
                None,
                None,
                explicit,
                vec![],
                None,
                (None, None),
                None,
                false,
                true,
                TabPlacement::Append,
                (1, false),
                None
            )
            .conflicts_with_template_adoption()
        );
    }
    assert!(
        ScreenInstruction::GoToTabName("named".into(), None, true, Some(1), None)
            .conflicts_with_template_adoption()
    );
    assert!(ScreenInstruction::BreakPane(None, 1, None).conflicts_with_template_adoption());
    assert!(ScreenInstruction::CloseTab(1, None).conflicts_with_template_adoption());
    assert!(
        ScreenInstruction::ClosePane(PaneId::Terminal(1), None, None, None)
            .conflicts_with_template_adoption()
    );
    assert!(ScreenInstruction::MoveTabLeft(1, None).conflicts_with_template_adoption());
    assert!(ScreenInstruction::NextSwapLayout(1, None).conflicts_with_template_adoption());
    assert!(
        !ScreenInstruction::PtyBytes(1, b"still alive".to_vec()).conflicts_with_template_adoption()
    );
    assert!(!ScreenInstruction::Render.conflicts_with_template_adoption());
}

#[test]
fn template_adoption_postcommit_failure_cannot_restore_old_default() {
    for debt in [false, true] {
        let mut screen = create_new_screen(
            Size {
                cols: 100,
                rows: 30,
            },
            true,
            true,
        );
        new_tab(&mut screen, 1, 0);
        new_tab(&mut screen, 2, 1);
        let (id, request) = stage(&mut screen, &[0], candidate());
        let prepared = prepare(
            &mut screen,
            0,
            request.request.tabs()[0].tiled_layout.clone(),
        );
        assert!(matches!(
            screen.commit_override_layout_state(id, vec![(0, prepared)]),
            CommittedOverrideLayout::Complete(_)
        ));
        let mut owner = screen.active_layout_transactions[&id].clone();
        let decision = if debt {
            ScreenLayoutDecision::CommittedWithCleanupDebt("lost cleanup ACK".into())
        } else {
            let mut stale = LayoutTabOwner::capture(&screen, 1);
            stale.expected = crate::screen::ExpectedLayoutTab::Present {
                instance_id: "foreign-incarnation".into(),
            };
            owner.tabs_to_close_after_commit.push(stale);
            let error = screen
                .close_owned_tabs_after_layout_commit(id, &owner)
                .unwrap_err();
            ScreenLayoutDecision::CommittedWithPostCommitError(error.to_string())
        };
        screen.record_resolved_layout_transaction(id, &owner, vec![], decision);
        screen.active_layout_transactions.remove(&id);
        assert_eq!(screen.default_layout, request.request.layout);
        assert_eq!(screen.template_generation, 1);
        let (_, disposition) = screen.replay_template_adoption(&request).unwrap();
        assert!(disposition.contains("CommittedWith"));
        assert!(disposition.contains("published=Some"));
    }
}

#[test]
fn template_adoption_later_prepare_failure_rolls_back_original_terminals() {
    let mut screen = create_new_screen(
        Size {
            cols: 100,
            rows: 30,
        },
        true,
        true,
    );
    new_tab(&mut screen, 1, 0);
    new_tab(&mut screen, 2, 1);
    let old = screen.default_layout.clone();
    let original_instances: Vec<_> = screen
        .tabs
        .values()
        .map(|tab| tab.instance_id.clone())
        .collect();
    let (id, request) = stage(&mut screen, &[0, 1], candidate());
    let first = prepare(
        &mut screen,
        0,
        request.request.tabs()[0].tiled_layout.clone(),
    );
    let missing =
        RunPluginOrAlias::from_url("file:/missing-adoption-plugin.wasm", &None, None, None)
            .unwrap();
    let result = screen
        .tabs
        .get_mut(&1)
        .unwrap()
        .begin_override_layout(OverrideLayoutOptions {
            layout: TiledPaneLayout {
                run: Some(Run::Plugin(missing)),
                ..Default::default()
            },
            floating_panes_layout: vec![],
            new_swap_tiled_layouts: Some(vec![]),
            new_swap_floating_layouts: Some(vec![]),
            new_terminal_ids: vec![],
            new_floating_terminal_ids: vec![],
            new_plugin_ids: HashMap::new(),
            retain_existing_terminal_panes: true,
            retain_existing_plugin_panes: false,
            client_id: 1,
            blocking_terminal: None,
        });
    assert!(
        result.is_err(),
        "later preparation has no plugin resource for its layout"
    );
    first.rollback(screen.tabs.get_mut(&0).unwrap(), "later preparation failed");
    let owner = screen.active_layout_transactions[&id].clone();
    screen.record_resolved_layout_transaction(
        id,
        &owner,
        vec![],
        ScreenLayoutDecision::Rejected("later preparation failed".into()),
    );
    screen.active_layout_transactions.remove(&id);
    assert_eq!(screen.default_layout, old);
    assert_eq!(screen.template_generation, 0);
    assert_eq!(
        screen
            .tabs
            .values()
            .map(|tab| tab.instance_id.clone())
            .collect::<Vec<_>>(),
        original_instances
    );
    assert!(screen.tabs[&0].has_pane_with_pid(&PaneId::Terminal(1)));
    assert!(screen.tabs[&1].has_pane_with_pid(&PaneId::Terminal(2)));
    assert!(
        screen
            .replay_template_adoption(&request)
            .unwrap()
            .1
            .contains("Rejected")
    );
}

#[test]
fn template_adoption_stale_generation_cannot_activate_or_publish() {
    let mut screen = create_new_screen(
        Size {
            cols: 100,
            rows: 30,
        },
        true,
        true,
    );
    new_tab(&mut screen, 1, 0);
    let old = screen.default_layout.clone();
    let (id, request) = stage(&mut screen, &[0], candidate());
    let prepared = prepare(
        &mut screen,
        0,
        request.request.tabs()[0].tiled_layout.clone(),
    );
    screen.template_generation += 1; // adversarial drift after preparation
    assert!(
        screen
            .validate_layout_transaction(id, &[ScreenLayoutTransactionKind::Override], &[0], None)
            .is_err()
    );
    let result = screen.commit_override_layout_state(id, vec![(0, prepared)]);
    let CommittedOverrideLayout::Indeterminate {
        committed_effects,
        remaining_prepared,
        ..
    } = result
    else {
        panic!("stale generation must remain quarantined")
    };
    assert!(committed_effects.is_empty());
    assert_eq!(remaining_prepared.len(), 1);
    assert_eq!(screen.default_layout, old);
    assert!(screen.template_adoption_pending());
    for (tab, transaction) in remaining_prepared {
        transaction.rollback(screen.tabs.get_mut(&tab).unwrap(), "stale");
    }
}
