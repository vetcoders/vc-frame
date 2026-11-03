use super::*;
use crate::input::command::RunCommand;
use crate::input::layout::{CanvasLayoutPhase, LayoutConstraint, PercentOrFixed, Run};
use prost::Message;

fn semantic_request() -> TemplateAdoption {
    let chrome = ["rail", "tab-bar", "status-bar"]
        .into_iter()
        .map(|name| TiledPaneLayout {
            name: Some(name.into()),
            borderless: Some(true),
            run: Some(Run::Plugin(RunPluginOrAlias::Alias(PluginAlias::new(
                name,
                &Some(BTreeMap::from([("session_canvas".into(), "true".into())])),
                Some(PathBuf::from("/workspace/chrome")),
            )))),
            ..Default::default()
        })
        .collect();
    let content = TiledPaneLayout {
        name: Some("future-content".into()),
        run: Some(Run::Command(RunCommand {
            command: PathBuf::from("/bin/sh"),
            args: vec!["-l".into()],
            cwd: Some(PathBuf::from("/workspace/future")),
            ..Default::default()
        })),
        ..Default::default()
    };
    let floating = FloatingPaneLayout {
        name: Some("future-floating".into()),
        width: Some(PercentOrFixed::Percent(60)),
        run: content.run.clone(),
        ..Default::default()
    };
    let mut layout = Layout {
        session_layer: Some((
            TiledPaneLayout {
                children: chrome,
                external_children_index: Some(1),
                ..Default::default()
            },
            vec![],
        )),
        template: Some((content.clone(), vec![floating.clone()])),
        swap_layouts: vec![(content.clone(), vec![floating.clone()])],
        swap_tiled_layouts: vec![(
            BTreeMap::from([
                (LayoutConstraint::MaxPanes(2), content.clone()),
                (LayoutConstraint::MinPanes(3), content),
            ]),
            Some("semantic-swaps".into()),
        )],
        swap_floating_layouts: vec![(
            BTreeMap::from([(LayoutConstraint::ExactPanes(1), vec![floating])]),
            Some("floating-swaps".into()),
        )],
        focused_tab_index: Some(0),
        ..Default::default()
    };
    let (mut materialized, floating) = layout.new_tab();
    materialized.tab_instance_id = Some("existing-tab-instance".into());
    layout
        .tabs
        .push((Some("Existing".into()), materialized, floating));
    TemplateAdoption {
        request_id: "1".into(),
        expected_generation: "session-incarnation:0".into(),
        layout: Box::new(layout),
    }
}

fn adoption_action(request: &TemplateAdoption) -> Action {
    Action::OverrideLayout {
        tabs: request.tabs(),
        template_adoption: Some(request.encode().unwrap()),
        retain_existing_terminal_panes: true,
        retain_existing_plugin_panes: false,
        apply_only_to_active_tab: false,
    }
}

fn both_wire_roundtrips(action: Action) -> [Action; 2] {
    type IpcAction = crate::client_server_contract::client_server_contract::Action;
    type PluginAction = crate::plugin_api::action::ProtobufAction;
    let ipc: IpcAction = action.clone().into();
    let ipc = IpcAction::decode(ipc.encode_to_vec().as_slice()).unwrap();
    let plugin: PluginAction = action.try_into().unwrap();
    let plugin = PluginAction::decode(plugin.encode_to_vec().as_slice()).unwrap();
    [ipc.try_into().unwrap(), plugin.try_into().unwrap()]
}

#[test]
fn template_adoption_full_semantics_survive_both_public_wire_families() {
    let request = semantic_request();
    let encoded = request.encode().unwrap();
    assert_eq!(
        TemplateAdoption::decode(&encoded)
            .unwrap()
            .encode()
            .unwrap(),
        encoded
    );
    for action in both_wire_roundtrips(adoption_action(&request)) {
        let Action::OverrideLayout {
            tabs,
            template_adoption,
            retain_existing_terminal_panes,
            retain_existing_plugin_panes,
            apply_only_to_active_tab,
        } = action
        else {
            panic!("wrong action after wire roundtrip")
        };
        assert!(retain_existing_terminal_panes);
        assert!(!retain_existing_plugin_panes);
        assert!(!apply_only_to_active_tab);
        assert_eq!(tabs.len(), 1);
        assert_eq!(
            tabs[0].tiled_layout.canvas_phase,
            CanvasLayoutPhase::Materialized
        );
        let decoded = TemplateAdoption::decode(&template_adoption.unwrap()).unwrap();
        // Layout equality intentionally ignores some PluginAlias runtime fields.
        // Comparing canonical envelopes also protects cwd and resolved alias data.
        assert_eq!(decoded.encode().unwrap(), encoded);
        assert_eq!(
            decoded.layout.tabs[0].1.canvas_phase,
            CanvasLayoutPhase::Materialized
        );
        assert_eq!(
            decoded.layout.tabs[0].1.tab_instance_id.as_deref(),
            Some("existing-tab-instance")
        );
        assert_eq!(
            decoded.layout.template.as_ref().unwrap().0.canvas_phase,
            CanvasLayoutPhase::Content
        );
        let shell = &decoded.layout.session_layer.as_ref().unwrap().0;
        assert_eq!(shell.children.len(), 3);
        let Some(Run::Plugin(RunPluginOrAlias::Alias(alias))) = &shell.children[0].run else {
            panic!("semantic alias was lost")
        };
        assert_eq!(alias.name, "rail");
        assert_eq!(alias.initial_cwd, Some(PathBuf::from("/workspace/chrome")));
        let Some(Run::Command(command)) = &decoded.layout.template.as_ref().unwrap().0.run else {
            panic!("future command was lost")
        };
        assert_eq!(command.cwd, Some(PathBuf::from("/workspace/future")));
        assert_eq!(decoded.layout.swap_tiled_layouts[0].0.len(), 2);
        assert!(
            decoded.layout.swap_tiled_layouts[0]
                .0
                .contains_key(&LayoutConstraint::MaxPanes(2))
        );
        assert!(
            decoded.layout.swap_floating_layouts[0]
                .0
                .contains_key(&LayoutConstraint::ExactPanes(1))
        );
        let (future, floating) = decoded.layout.new_tab();
        assert_eq!(future.canvas_phase, CanvasLayoutPhase::Materialized);
        assert_eq!(
            future.children.len(),
            4,
            "exactly three chrome panes plus one content pane"
        );
        assert_eq!(future.children[1].name.as_deref(), Some("future-content"));
        assert_eq!(floating.len(), 1);
        assert_eq!(
            decoded.tabs()[0].tiled_layout.children.len(),
            4,
            "materialized tabs must not mount chrome twice"
        );
    }
}

#[test]
fn legacy_override_remains_non_adopting_through_both_wires() {
    let mut action = adoption_action(&semantic_request());
    if let Action::OverrideLayout {
        template_adoption, ..
    } = &mut action
    {
        *template_adoption = None;
    }
    for restored in both_wire_roundtrips(action) {
        let Action::OverrideLayout {
            template_adoption,
            tabs,
            ..
        } = restored
        else {
            panic!("wrong action after wire roundtrip")
        };
        assert!(template_adoption.is_none());
        assert_eq!(tabs.len(), 1);
        assert_eq!(
            tabs[0].tiled_layout.canvas_phase,
            CanvasLayoutPhase::Materialized
        );
    }
}

#[test]
fn template_adoption_rejects_invalid_semantic_child_index() {
    let mut request = semantic_request();
    request
        .layout
        .session_layer
        .as_mut()
        .unwrap()
        .0
        .external_children_index = Some(999);
    // Encode does not assert trust: hostile wire producers can supply this shape.
    // Decode must reject before tabs() reaches Vec::insert during shell mounting.
    assert!(TemplateAdoption::decode(&request.encode().unwrap()).is_err());
}

#[test]
fn template_adoption_cli_rejects_active_tab_only_before_layout_loading() {
    let result = Action::actions_from_cli(
        CliAction::OverrideLayout {
            template_adoption_id: Some("1".into()),
            expected_template_generation: Some("session-incarnation:0".into()),
            template_status: false,
            layout: None,
            layout_string: Some("layout { pane; }".into()),
            layout_dir: None,
            retain_existing_terminal_panes: true,
            retain_existing_plugin_panes: false,
            apply_only_to_active_tab: true,
        },
        Box::new(|| panic!("active-only rejection must precede cwd/layout resolution")),
        None,
    );
    assert!(result.is_err());
}
