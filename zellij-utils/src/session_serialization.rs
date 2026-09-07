use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use crate::{
    input::layout::PluginUserConfiguration,
    input::layout::{
        CanvasLayoutPhase, FloatingPaneLayout, Layout, LayoutConstraint, PercentOrFixed, Run,
        RunPluginOrAlias, SplitDirection, SplitSize, SwapFloatingLayout, SwapTiledLayout,
        TiledPaneLayout,
    },
    pane_size::{Constraint, PaneGeom},
};

#[derive(Default, Debug, Clone)]
pub struct GlobalLayoutManifest {
    pub global_cwd: Option<PathBuf>,
    pub default_shell: Option<PathBuf>,
    pub default_layout: Box<Layout>,
    pub tabs: Vec<(String, TabLayoutManifest)>,
}

#[derive(Default, Debug, Clone)]
pub struct TabLayoutManifest {
    pub tab_instance_id: String,
    pub tiled_panes: Vec<PaneLayoutManifest>,
    pub floating_panes: Vec<PaneLayoutManifest>,
    pub is_focused: bool,
    pub hide_floating_panes: bool,
}

#[derive(Default, Debug, Clone)]
pub struct PaneLayoutManifest {
    pub geom: PaneGeom,
    pub run: Option<Run>,
    pub cwd: Option<PathBuf>,
    pub is_borderless: bool,
    pub title: Option<String>,
    pub is_focused: bool,
    pub pane_contents: Option<String>,
    pub default_fg: Option<String>,
    pub default_bg: Option<String>,
}

struct PaneNodeAttributes<'a> {
    command: &'a Option<String>,
    edit: &'a Option<String>,
    name: &'a Option<String>,
    cwd: Option<PathBuf>,
    focus: Option<bool>,
    initial_pane_contents: &'a Option<String>,
    has_children: bool,
}

pub fn serialize_session_layout(
    global_layout_manifest: GlobalLayoutManifest,
) -> Result<(String, BTreeMap<String, String>), &'static str> {
    // BTreeMap is the pane contents and their file names
    let mut document = KdlDocument::new();
    let mut pane_contents = BTreeMap::new();
    let mut layout_node = KdlNode::new("layout");
    let mut layout_node_children = KdlDocument::new();
    if let Some(global_cwd) = serialize_global_cwd(&global_layout_manifest.global_cwd) {
        layout_node_children.nodes_mut().push(global_cwd);
    }
    match serialize_multiple_tabs(global_layout_manifest.tabs, &mut pane_contents) {
        Ok(mut serialized_tabs) => {
            layout_node_children
                .nodes_mut()
                .append(&mut serialized_tabs);
        },
        Err(e) => {
            return Err(e);
        },
    }
    serialize_session_layer(
        global_layout_manifest.default_layout.session_layer,
        &mut pane_contents,
        &mut layout_node_children,
    );
    serialize_new_tab_template(
        global_layout_manifest.default_layout.template,
        &mut pane_contents,
        &mut layout_node_children,
    );
    serialize_swap_tiled_layouts(
        global_layout_manifest.default_layout.swap_tiled_layouts,
        &mut pane_contents,
        &mut layout_node_children,
    );
    serialize_swap_floating_layouts(
        global_layout_manifest.default_layout.swap_floating_layouts,
        &mut pane_contents,
        &mut layout_node_children,
    );

    layout_node.set_children(layout_node_children);
    document.nodes_mut().push(layout_node);
    Ok((document.to_string(), pane_contents))
}

fn serialize_tab(
    tab_name: String,
    tab_instance_id: String,
    is_focused: bool,
    hide_floating_panes: bool,
    tiled_panes: &[PaneLayoutManifest],
    floating_panes: &[PaneLayoutManifest],
    pane_contents: &mut BTreeMap<String, String>,
) -> Option<KdlNode> {
    let mut serialized_tab = KdlNode::new("tab");
    // Manifests capture the full current canvas, including explicit no-layer tabs.
    serialized_tab.push(KdlEntry::new_prop("canvas_state", "materialized"));
    let mut serialized_tab_children = KdlDocument::new();
    match get_tiled_panes_layout_from_panegeoms(tiled_panes, None) {
        Some(tiled_panes_layout) => {
            let floating_panes_layout = get_floating_panes_layout_from_panegeoms(floating_panes);
            let tiled_panes = if tiled_panes_layout.children_split_direction
                != SplitDirection::default()
                || tiled_panes_layout.children_are_stacked
                // A real single pane is the root itself, not a grouping node.
                || (!tiled_panes.is_empty() && tiled_panes_layout.children.is_empty())
            {
                vec![tiled_panes_layout]
            } else {
                tiled_panes_layout.children
            };
            serialized_tab
                .entries_mut()
                .push(KdlEntry::new_prop("name", tab_name));
            if !tab_instance_id.is_empty() {
                serialized_tab
                    .entries_mut()
                    .push(KdlEntry::new_prop("vc_tab_instance_id", tab_instance_id));
            }
            if is_focused {
                serialized_tab
                    .entries_mut()
                    .push(KdlEntry::new_prop("focus", KdlValue::Bool(true)));
            }
            if hide_floating_panes {
                serialized_tab.entries_mut().push(KdlEntry::new_prop(
                    "hide_floating_panes",
                    KdlValue::Bool(true),
                ));
            }

            serialize_tiled_and_floating_panes(
                &tiled_panes,
                floating_panes_layout,
                pane_contents,
                &mut serialized_tab_children,
            );

            serialized_tab.set_children(serialized_tab_children);
            Some(serialized_tab)
        },
        None => None,
    }
}

fn serialize_tiled_and_floating_panes(
    tiled_panes: &[TiledPaneLayout],
    floating_panes_layout: Vec<FloatingPaneLayout>,
    pane_contents: &mut BTreeMap<String, String>,
    serialized_tab_children: &mut KdlDocument,
) {
    for tiled_pane_layout in tiled_panes {
        let ignore_size = false;
        let tiled_pane_node = serialize_tiled_pane(tiled_pane_layout, ignore_size, pane_contents);
        serialized_tab_children.nodes_mut().push(tiled_pane_node);
    }
    if !floating_panes_layout.is_empty() {
        let mut floating_panes_node = KdlNode::new("floating_panes");
        let mut floating_panes_node_children = KdlDocument::new();
        for floating_pane in floating_panes_layout {
            let pane_node = serialize_floating_pane(&floating_pane, pane_contents);
            floating_panes_node_children.nodes_mut().push(pane_node);
        }
        floating_panes_node.set_children(floating_panes_node_children);
        serialized_tab_children
            .nodes_mut()
            .push(floating_panes_node);
    }
}

fn serialize_tiled_pane(
    layout: &TiledPaneLayout,
    ignore_size: bool,
    pane_contents: &mut BTreeMap<String, String>,
) -> KdlNode {
    let (command, args) = extract_command_and_args(&layout.run);
    let (plugin, plugin_config) = extract_plugin_and_config(&layout.run);
    let (edit, _line_number) = extract_edit_and_line_number(&layout.run);
    let cwd = layout.run.as_ref().and_then(|r| r.get_cwd());
    let has_children = layout.external_children_index.is_some() || !layout.children.is_empty();

    let mut tiled_pane_node = KdlNode::new("pane");
    serialize_pane_title_and_attributes(
        PaneNodeAttributes {
            command: &command,
            edit: &edit,
            name: &layout.name,
            cwd,
            focus: layout.focus,
            initial_pane_contents: &layout.pane_initial_contents,
            has_children,
        },
        pane_contents,
        &mut tiled_pane_node,
    );

    serialize_tiled_layout_attributes(layout, ignore_size, &mut tiled_pane_node);
    if let Some(ref fg) = layout.default_fg {
        tiled_pane_node
            .entries_mut()
            .push(KdlEntry::new_prop("default_fg", fg.to_owned()));
    }
    if let Some(ref bg) = layout.default_bg {
        tiled_pane_node
            .entries_mut()
            .push(KdlEntry::new_prop("default_bg", bg.to_owned()));
    }
    let has_child_attributes = !layout.children.is_empty()
        || layout.external_children_index.is_some()
        || !args.is_empty()
        || plugin.is_some()
        || command.is_some();
    if has_child_attributes {
        let mut tiled_pane_node_children = KdlDocument::new();
        serialize_args(args, &mut tiled_pane_node_children);
        serialize_start_suspended(&command, &mut tiled_pane_node_children);
        serialize_plugin(
            plugin,
            plugin_config,
            &layout.run,
            &mut tiled_pane_node_children,
        );
        serialize_tiled_children(layout, pane_contents, &mut tiled_pane_node_children);
        tiled_pane_node.set_children(tiled_pane_node_children);
    }
    tiled_pane_node
}

// A raw insertion point is a gap, not a placeholder occupying a child.
fn serialize_tiled_children(
    layout: &TiledPaneLayout,
    pane_contents: &mut BTreeMap<String, String>,
    children: &mut KdlDocument,
) {
    for index in 0..=layout.children.len() {
        if layout.external_children_index == Some(index) {
            children.nodes_mut().push(KdlNode::new("children"));
        }
        if let Some(pane) = layout.children.get(index) {
            children.nodes_mut().push(serialize_tiled_pane(
                pane,
                layout.children_are_stacked,
                pane_contents,
            ));
        }
    }
}

fn serialize_session_layer(
    session_layer: Option<(TiledPaneLayout, Vec<FloatingPaneLayout>)>,
    pane_contents: &mut BTreeMap<String, String>,
    layout_children: &mut KdlDocument,
) {
    if let Some((tiled, floating)) = session_layer {
        let mut node = KdlNode::new("session_layer");
        serialize_tiled_layout_attributes(&tiled, false, &mut node);
        let mut children = KdlDocument::new();
        serialize_tiled_children(&tiled, pane_contents, &mut children);
        // Parsed layers reject floating panes. Do not silently discard them if a
        // programmatic producer supplies one: reparsing must still reject it.
        serialize_tiled_and_floating_panes(&[], floating, pane_contents, &mut children);
        node.set_children(children);
        layout_children.nodes_mut().push(node);
    }
}

/// Unwrap only synthetic grouping roots. A cwd-only empty root belongs on the
/// tab title, so it survives without manufacturing an additional pane.
fn serialize_template_root(mut tiled: TiledPaneLayout, node: &mut KdlNode) -> Vec<TiledPaneLayout> {
    tiled.canvas_phase = CanvasLayoutPhase::Content;
    if tiled.children.is_empty() {
        if let Some(Run::Cwd(cwd)) = &tiled.run {
            node.push(KdlEntry::new_prop("cwd", cwd.display().to_string()));
            tiled.run = None;
        }
    } else if matches!(&tiled.run, Some(Run::Cwd(_))) {
        // add_cwd_to_layout already propagates this grouping cwd to descendants.
        tiled.run = None;
    }
    if tiled.hide_floating_panes {
        node.push(KdlEntry::new_prop("hide_floating_panes", true));
        tiled.hide_floating_panes = false;
    }
    let children = std::mem::take(&mut tiled.children);
    if tiled == TiledPaneLayout::default() {
        children
    } else {
        tiled.children = children;
        vec![tiled]
    }
}

pub fn extract_command_and_args(layout_run: &Option<Run>) -> (Option<String>, Vec<String>) {
    match layout_run {
        Some(Run::Command(run_command)) => (
            Some(run_command.command.display().to_string()),
            run_command.args.clone(),
        ),
        _ => (None, vec![]),
    }
}
pub fn extract_plugin_and_config(
    layout_run: &Option<Run>,
) -> (Option<String>, Option<PluginUserConfiguration>) {
    match &layout_run {
        Some(Run::Plugin(run_plugin_or_alias)) => match run_plugin_or_alias {
            RunPluginOrAlias::RunPlugin(run_plugin) => (
                Some(run_plugin.location.display()),
                Some(run_plugin.configuration.clone()),
            ),
            RunPluginOrAlias::Alias(plugin_alias) => {
                // Semantic layer/template aliases may not have been resolved yet.
                // Retain their authored configuration as well as resolved run configuration.
                let name = plugin_alias
                    .run_plugin
                    .as_ref()
                    .map(|run_plugin| run_plugin.location.display().to_string())
                    .unwrap_or_else(|| plugin_alias.name.clone());
                let configuration = plugin_alias
                    .run_plugin
                    .as_ref()
                    .map(|run_plugin| run_plugin.configuration.clone())
                    .or_else(|| plugin_alias.configuration.clone());
                (Some(name), configuration)
            },
        },
        _ => (None, None),
    }
}
pub fn extract_edit_and_line_number(layout_run: &Option<Run>) -> (Option<String>, Option<usize>) {
    match &layout_run {
        // TODO: line number in layouts?
        Some(Run::EditFile(path, line_number, _cwd)) => {
            (Some(path.display().to_string()), *line_number)
        },
        _ => (None, None),
    }
}

fn serialize_pane_title_and_attributes(
    attributes: PaneNodeAttributes<'_>,
    pane_contents: &mut BTreeMap<String, String>,
    kdl_node: &mut KdlNode,
) {
    let PaneNodeAttributes {
        command,
        edit,
        name,
        cwd,
        focus,
        initial_pane_contents,
        has_children,
    } = attributes;
    match (command, edit) {
        (Some(command), _) => kdl_node
            .entries_mut()
            .push(KdlEntry::new_prop("command", command.to_owned())),
        (None, Some(edit)) => kdl_node
            .entries_mut()
            .push(KdlEntry::new_prop("edit", edit.to_owned())),
        _ => {},
    };
    if let Some(name) = name {
        kdl_node
            .entries_mut()
            .push(KdlEntry::new_prop("name", name.to_owned()));
    }
    if let Some(cwd) = cwd {
        let path = cwd.display().to_string();
        if !path.is_empty() && !has_children {
            kdl_node
                .entries_mut()
                .push(KdlEntry::new_prop("cwd", path.to_owned()));
        }
    }
    if focus.unwrap_or(false) {
        kdl_node
            .entries_mut()
            .push(KdlEntry::new_prop("focus", KdlValue::Bool(true)));
    }
    if let Some(initial_pane_contents) = initial_pane_contents.as_ref()
        && command.is_none()
        && edit.is_none()
    {
        let file_name = format!("initial_contents_{}", pane_contents.keys().len() + 1);
        kdl_node
            .entries_mut()
            .push(KdlEntry::new_prop("contents_file", file_name.clone()));

        pane_contents.insert(file_name, initial_pane_contents.clone());
    }
}

fn serialize_args(args: Vec<String>, pane_node_children: &mut KdlDocument) {
    if !args.is_empty() {
        let mut args_node = KdlNode::new("args");
        for arg in &args {
            args_node.entries_mut().push(KdlEntry::new(arg.to_owned()));
        }
        pane_node_children.nodes_mut().push(args_node);
    }
}

fn serialize_plugin(
    plugin: Option<String>,
    plugin_config: Option<PluginUserConfiguration>,
    run: &Option<Run>,
    pane_node_children: &mut KdlDocument,
) {
    if let Some(plugin) = plugin {
        let mut plugin_node = KdlNode::new("plugin");
        plugin_node
            .entries_mut()
            .push(KdlEntry::new_prop("location", plugin.to_owned()));
        let initial_cwd = match run {
            Some(Run::Plugin(RunPluginOrAlias::RunPlugin(plugin))) => plugin.initial_cwd.clone(),
            Some(Run::Plugin(RunPluginOrAlias::Alias(alias))) => {
                alias.initial_cwd.clone().or_else(|| {
                    alias
                        .run_plugin
                        .as_ref()
                        .and_then(|plugin| plugin.initial_cwd.clone())
                })
            },
            _ => None,
        };
        if let Some(cwd) = initial_cwd {
            plugin_node.push(KdlEntry::new_prop("cwd", cwd.display().to_string()));
        }
        if let Some(plugin_config) =
            plugin_config.and_then(|p| if p.inner().is_empty() { None } else { Some(p) })
        {
            let mut plugin_node_children = KdlDocument::new();
            for (config_key, config_value) in plugin_config.inner() {
                let mut config_node = KdlNode::new(config_key.to_owned());
                config_node
                    .entries_mut()
                    .push(KdlEntry::new(config_value.to_owned()));
                plugin_node_children.nodes_mut().push(config_node);
            }
            plugin_node.set_children(plugin_node_children);
        }
        pane_node_children.nodes_mut().push(plugin_node);
    }
}

fn serialize_tiled_layout_attributes(
    layout: &TiledPaneLayout,
    ignore_size: bool,
    kdl_node: &mut KdlNode,
) {
    if !ignore_size {
        match layout.split_size {
            Some(SplitSize::Fixed(size)) => kdl_node
                .entries_mut()
                .push(KdlEntry::new_prop("size", KdlValue::Base10(size as i64))),
            Some(SplitSize::Percent(size)) => kdl_node
                .entries_mut()
                .push(KdlEntry::new_prop("size", format!("{size}%"))),
            None => (),
        };
    }
    if layout.borderless.unwrap_or(false) {
        kdl_node
            .entries_mut()
            .push(KdlEntry::new_prop("borderless", KdlValue::Bool(true)));
    }
    if layout.children_are_stacked {
        kdl_node
            .entries_mut()
            .push(KdlEntry::new_prop("stacked", KdlValue::Bool(true)));
    }
    if layout.is_expanded_in_stack {
        kdl_node
            .entries_mut()
            .push(KdlEntry::new_prop("expanded", KdlValue::Bool(true)));
    }
    if layout.children_split_direction != SplitDirection::default() {
        let direction = match layout.children_split_direction {
            SplitDirection::Horizontal => "horizontal",
            SplitDirection::Vertical => "vertical",
        };
        kdl_node
            .entries_mut()
            .push(KdlEntry::new_prop("split_direction", direction));
    }
}

fn serialize_floating_layout_attributes(
    layout: &FloatingPaneLayout,
    pane_node_children: &mut KdlDocument,
) {
    match layout.height {
        Some(PercentOrFixed::Fixed(fixed_height)) => {
            let mut node = KdlNode::new("height");
            node.entries_mut()
                .push(KdlEntry::new(KdlValue::Base10(fixed_height as i64)));
            pane_node_children.nodes_mut().push(node);
        },
        Some(PercentOrFixed::Percent(percent)) => {
            let mut node = KdlNode::new("height");
            node.entries_mut()
                .push(KdlEntry::new(format!("{}%", percent)));
            pane_node_children.nodes_mut().push(node);
        },
        None => {},
    }
    match layout.width {
        Some(PercentOrFixed::Fixed(fixed_width)) => {
            let mut node = KdlNode::new("width");
            node.entries_mut()
                .push(KdlEntry::new(KdlValue::Base10(fixed_width as i64)));
            pane_node_children.nodes_mut().push(node);
        },
        Some(PercentOrFixed::Percent(percent)) => {
            let mut node = KdlNode::new("width");
            node.entries_mut()
                .push(KdlEntry::new(format!("{}%", percent)));
            pane_node_children.nodes_mut().push(node);
        },
        None => {},
    }
    match layout.x {
        Some(PercentOrFixed::Fixed(fixed_x)) => {
            let mut node = KdlNode::new("x");
            node.entries_mut()
                .push(KdlEntry::new(KdlValue::Base10(fixed_x as i64)));
            pane_node_children.nodes_mut().push(node);
        },
        Some(PercentOrFixed::Percent(percent)) => {
            let mut node = KdlNode::new("x");
            node.entries_mut()
                .push(KdlEntry::new(format!("{}%", percent)));
            pane_node_children.nodes_mut().push(node);
        },
        None => {},
    }
    match layout.y {
        Some(PercentOrFixed::Fixed(fixed_y)) => {
            let mut node = KdlNode::new("y");
            node.entries_mut()
                .push(KdlEntry::new(KdlValue::Base10(fixed_y as i64)));
            pane_node_children.nodes_mut().push(node);
        },
        Some(PercentOrFixed::Percent(percent)) => {
            let mut node = KdlNode::new("y");
            node.entries_mut()
                .push(KdlEntry::new(format!("{}%", percent)));
            pane_node_children.nodes_mut().push(node);
        },
        None => {},
    }
    if let Some(true) = layout.pinned {
        let mut node = KdlNode::new("pinned");
        node.entries_mut().push(KdlEntry::new(KdlValue::Bool(true)));
        pane_node_children.nodes_mut().push(node);
    }
}

fn serialize_start_suspended(command: &Option<String>, pane_node_children: &mut KdlDocument) {
    if command.is_some() {
        let mut start_suspended_node = KdlNode::new("start_suspended");
        start_suspended_node
            .entries_mut()
            .push(KdlEntry::new(KdlValue::Bool(true)));
        pane_node_children.nodes_mut().push(start_suspended_node);
    }
}

fn serialize_global_cwd(global_cwd: &Option<PathBuf>) -> Option<KdlNode> {
    global_cwd.as_ref().map(|cwd| {
        let mut node = KdlNode::new("cwd");
        node.push(cwd.display().to_string());
        node
    })
}

fn serialize_new_tab_template(
    new_tab_template: Option<(TiledPaneLayout, Vec<FloatingPaneLayout>)>,
    pane_contents: &mut BTreeMap<String, String>,
    layout_children_node: &mut KdlDocument,
) {
    if let Some((tiled_panes, floating_panes)) = new_tab_template {
        let mut new_tab_template_node = KdlNode::new("new_tab_template");
        let tiled_panes = serialize_template_root(tiled_panes, &mut new_tab_template_node);
        let mut new_tab_template_children = KdlDocument::new();

        serialize_tiled_and_floating_panes(
            &tiled_panes,
            floating_panes,
            pane_contents,
            &mut new_tab_template_children,
        );
        if !new_tab_template_children.is_empty() {
            new_tab_template_node.set_children(new_tab_template_children);
        }
        layout_children_node.nodes_mut().push(new_tab_template_node);
    }
}

fn serialize_swap_tiled_layouts(
    swap_tiled_layouts: Vec<SwapTiledLayout>,
    pane_contents: &mut BTreeMap<String, String>,
    layout_node_children: &mut KdlDocument,
) {
    for swap_tiled_layout in swap_tiled_layouts {
        let mut swap_tiled_layout_node = KdlNode::new("swap_tiled_layout");
        let mut swap_tiled_layout_node_children = KdlDocument::new();
        let swap_tiled_layout_name = swap_tiled_layout.1;
        if let Some(name) = swap_tiled_layout_name {
            swap_tiled_layout_node
                .entries_mut()
                .push(KdlEntry::new_prop("name", name.to_owned()));
        }

        for (layout_constraint, tiled_panes_layout) in swap_tiled_layout.0 {
            let mut layout_step_node = KdlNode::new("tab");
            if tiled_panes_layout.canvas_phase == CanvasLayoutPhase::Materialized {
                layout_step_node.push(KdlEntry::new_prop("canvas_state", "materialized"));
            }
            let tiled_panes_layout =
                serialize_template_root(tiled_panes_layout, &mut layout_step_node);
            let mut layout_step_node_children = KdlDocument::new();
            if let Some(layout_constraint_entry) = serialize_layout_constraint(layout_constraint) {
                layout_step_node.entries_mut().push(layout_constraint_entry);
            }

            serialize_tiled_and_floating_panes(
                &tiled_panes_layout,
                vec![],
                pane_contents,
                &mut layout_step_node_children,
            );
            if !layout_step_node_children.is_empty() {
                layout_step_node.set_children(layout_step_node_children);
            }
            swap_tiled_layout_node_children
                .nodes_mut()
                .push(layout_step_node);
        }
        swap_tiled_layout_node.set_children(swap_tiled_layout_node_children);
        layout_node_children
            .nodes_mut()
            .push(swap_tiled_layout_node);
    }
}

fn serialize_layout_constraint(layout_constraint: LayoutConstraint) -> Option<KdlEntry> {
    match layout_constraint {
        LayoutConstraint::MaxPanes(max_panes) => Some(KdlEntry::new_prop(
            "max_panes",
            KdlValue::Base10(max_panes as i64),
        )),
        LayoutConstraint::MinPanes(min_panes) => Some(KdlEntry::new_prop(
            "min_panes",
            KdlValue::Base10(min_panes as i64),
        )),
        LayoutConstraint::ExactPanes(exact_panes) => Some(KdlEntry::new_prop(
            "exact_panes",
            KdlValue::Base10(exact_panes as i64),
        )),
        LayoutConstraint::NoConstraint => None,
    }
}

fn serialize_swap_floating_layouts(
    swap_floating_layouts: Vec<SwapFloatingLayout>,
    pane_contents: &mut BTreeMap<String, String>,
    layout_children_node: &mut KdlDocument,
) {
    for swap_floating_layout in swap_floating_layouts {
        let mut swap_floating_layout_node = KdlNode::new("swap_floating_layout");
        let mut swap_floating_layout_node_children = KdlDocument::new();
        let swap_floating_layout_name = swap_floating_layout.1;
        if let Some(name) = swap_floating_layout_name {
            swap_floating_layout_node
                .entries_mut()
                .push(KdlEntry::new_prop("name", name.to_owned()));
        }

        for (layout_constraint, floating_panes_layout) in swap_floating_layout.0 {
            let mut layout_step_node = KdlNode::new("floating_panes");
            let mut layout_step_node_children = KdlDocument::new();
            if let Some(layout_constraint_entry) = serialize_layout_constraint(layout_constraint) {
                layout_step_node.entries_mut().push(layout_constraint_entry);
            }

            for floating_pane_layout in floating_panes_layout {
                let floating_pane_node =
                    serialize_floating_pane(&floating_pane_layout, pane_contents);
                layout_step_node_children
                    .nodes_mut()
                    .push(floating_pane_node);
            }
            layout_step_node.set_children(layout_step_node_children);
            swap_floating_layout_node_children
                .nodes_mut()
                .push(layout_step_node);
        }
        swap_floating_layout_node.set_children(swap_floating_layout_node_children);
        layout_children_node
            .nodes_mut()
            .push(swap_floating_layout_node);
    }
}

fn serialize_multiple_tabs(
    tabs: Vec<(String, TabLayoutManifest)>,
    pane_contents: &mut BTreeMap<String, String>,
) -> Result<Vec<KdlNode>, &'static str> {
    // A durable resurrection checkpoint must represent every captured tab.
    // Reject transiently indecomposable geometry so the previous complete
    // checkpoint survives until a later complete capture can replace it.
    let mut serialized_tabs: Vec<KdlNode> = vec![];
    for (tab_name, tab_layout_manifest) in tabs {
        let tiled_panes = tab_layout_manifest.tiled_panes;
        let floating_panes = tab_layout_manifest.floating_panes;
        let hide_floating_panes = tab_layout_manifest.hide_floating_panes;
        let serialized = serialize_tab(
            tab_name.clone(),
            tab_layout_manifest.tab_instance_id,
            tab_layout_manifest.is_focused,
            hide_floating_panes,
            &tiled_panes,
            &floating_panes,
            pane_contents,
        );
        if let Some(serialized) = serialized {
            serialized_tabs.push(serialized);
        } else {
            log::warn!(
                "Failed to serialize tab '{}' (pane geometry did not decompose into splits); \
                 rejecting incomplete session snapshot",
                tab_name
            );
            return Err("Incomplete session snapshot: failed to serialize a captured tab");
        }
    }
    Ok(serialized_tabs)
}

fn serialize_floating_pane(
    layout: &FloatingPaneLayout,
    pane_contents: &mut BTreeMap<String, String>,
) -> KdlNode {
    let mut floating_pane_node = KdlNode::new("pane");
    let mut floating_pane_node_children = KdlDocument::new();
    let (command, args) = extract_command_and_args(&layout.run);
    let (plugin, plugin_config) = extract_plugin_and_config(&layout.run);
    let (edit, _line_number) = extract_edit_and_line_number(&layout.run);
    let cwd = layout.run.as_ref().and_then(|r| r.get_cwd());
    let has_children = false;
    serialize_pane_title_and_attributes(
        PaneNodeAttributes {
            command: &command,
            edit: &edit,
            name: &layout.name,
            cwd,
            focus: layout.focus,
            initial_pane_contents: &layout.pane_initial_contents,
            has_children,
        },
        pane_contents,
        &mut floating_pane_node,
    );
    if let Some(ref fg) = layout.default_fg {
        floating_pane_node
            .entries_mut()
            .push(KdlEntry::new_prop("default_fg", fg.to_owned()));
    }
    if let Some(ref bg) = layout.default_bg {
        floating_pane_node
            .entries_mut()
            .push(KdlEntry::new_prop("default_bg", bg.to_owned()));
    }
    serialize_start_suspended(&command, &mut floating_pane_node_children);
    serialize_floating_layout_attributes(layout, &mut floating_pane_node_children);
    serialize_args(args, &mut floating_pane_node_children);
    serialize_plugin(
        plugin,
        plugin_config,
        &layout.run,
        &mut floating_pane_node_children,
    );
    floating_pane_node.set_children(floating_pane_node_children);
    floating_pane_node
}

fn stack_layout_from_manifest(
    geoms: &[PaneLayoutManifest],
    split_size: Option<SplitSize>,
) -> Option<TiledPaneLayout> {
    let mut children_stacks: HashMap<usize, Vec<PaneLayoutManifest>> = HashMap::new();
    for p in geoms {
        if let Some(stack_id) = p.geom.stacked {
            children_stacks.entry(stack_id).or_default().push(p.clone());
        }
    }
    let mut stack_nodes = vec![];
    for (_stack_id, stacked_panes) in children_stacks.into_iter() {
        stack_nodes.push(TiledPaneLayout {
            split_size,
            children: stacked_panes
                .iter()
                .map(|p| tiled_pane_layout_from_manifest(Some(p), None))
                .collect(),
            children_are_stacked: true,
            ..Default::default()
        })
    }
    if stack_nodes.len() == 1 {
        // if there's only one stack, we return it without a wrapper
        stack_nodes.first().cloned()
    } else {
        // here there is more than one stack, so we wrap it in a logical container node
        Some(TiledPaneLayout {
            split_size,
            children: stack_nodes,
            ..Default::default()
        })
    }
}

fn tiled_pane_layout_from_manifest(
    manifest: Option<&PaneLayoutManifest>,
    split_size: Option<SplitSize>,
) -> TiledPaneLayout {
    let (
        run,
        borderless,
        is_expanded_in_stack,
        name,
        focus,
        pane_initial_contents,
        default_fg,
        default_bg,
    ) = manifest
        .map(|g| {
            let mut run = g.run.clone();
            if let Some(cwd) = &g.cwd {
                if let Some(run) = run.as_mut() {
                    run.add_cwd(cwd);
                } else {
                    run = Some(Run::Cwd(cwd.clone()));
                }
            }
            (
                run,
                Some(g.is_borderless),
                g.geom.is_stacked() && g.geom.rows.inner > 1,
                g.title.clone(),
                Some(g.is_focused),
                g.pane_contents.clone(),
                g.default_fg.clone(),
                g.default_bg.clone(),
            )
        })
        .unwrap_or((None, None, false, None, None, None, None, None));
    TiledPaneLayout {
        split_size,
        run,
        borderless,
        is_expanded_in_stack,
        name,
        focus,
        pane_initial_contents,
        default_fg,
        default_bg,
        ..Default::default()
    }
}

/// Tab-level parsing
fn get_tiled_panes_layout_from_panegeoms(
    geoms: &[PaneLayoutManifest],
    split_size: Option<SplitSize>,
) -> Option<TiledPaneLayout> {
    let (children_split_direction, splits) = match get_splits(geoms) {
        Some(x) => x,
        None => {
            if geoms.len() > 1 {
                // this can only happen if all geoms belong to one or more stacks
                // since stack splits are discounted in the get_splits method
                return stack_layout_from_manifest(geoms, split_size);
            } else {
                return Some(tiled_pane_layout_from_manifest(
                    geoms.iter().next(),
                    split_size,
                ));
            }
        },
    };
    let mut children = Vec::new();
    let mut remaining_geoms = geoms.to_vec();
    let mut new_geoms = Vec::new();
    let mut new_constraints = Vec::new();
    for i in 1..splits.len() {
        let (v_min, v_max) = (splits[i - 1], splits[i]);
        let subgeoms: Vec<PaneLayoutManifest>;
        (subgeoms, remaining_geoms) = match children_split_direction {
            SplitDirection::Horizontal => remaining_geoms
                .clone()
                .into_iter()
                .partition(|g| g.geom.y + g.geom.rows.as_usize() <= v_max),
            SplitDirection::Vertical => remaining_geoms
                .clone()
                .into_iter()
                .partition(|g| g.geom.x + g.geom.cols.as_usize() <= v_max),
        };
        match get_domain_constraint(&subgeoms, &children_split_direction, (v_min, v_max)) {
            Some(constraint) => {
                new_geoms.push(subgeoms);
                new_constraints.push(constraint);
            },
            None => {
                return None;
            },
        }
    }

    let new_split_sizes = get_split_sizes(&new_constraints);

    for (subgeoms, subsplit_size) in new_geoms.iter().zip(new_split_sizes) {
        match get_tiled_panes_layout_from_panegeoms(subgeoms, subsplit_size) {
            Some(child) => {
                children.push(child);
            },
            None => {
                return None;
            },
        }
    }
    let children_are_stacked = children_split_direction == SplitDirection::Horizontal
        && all_geoms_are_from_the_same_stack(&new_geoms);
    Some(TiledPaneLayout {
        children_split_direction,
        split_size,
        children,
        children_are_stacked,
        ..Default::default()
    })
}

fn all_geoms_are_from_the_same_stack(manifests: &[Vec<PaneLayoutManifest>]) -> bool {
    let mut stack_ids = HashSet::new();
    for manifest_group in manifests {
        for pane_layout_manifest in manifest_group {
            stack_ids.insert(pane_layout_manifest.geom.stacked);
        }
    }
    stack_ids.len() == 1 && !stack_ids.contains(&None)
}

fn get_floating_panes_layout_from_panegeoms(
    manifests: &[PaneLayoutManifest],
) -> Vec<FloatingPaneLayout> {
    manifests
        .iter()
        .map(|m| {
            let mut run = m.run.clone();
            if let Some(cwd) = &m.cwd
                && let Some(r) = run.as_mut()
            {
                r.add_cwd(cwd)
            }
            FloatingPaneLayout {
                name: m.title.clone(),
                height: Some(m.geom.rows.into()),
                width: Some(m.geom.cols.into()),
                x: Some(PercentOrFixed::Fixed(m.geom.x)),
                y: Some(PercentOrFixed::Fixed(m.geom.y)),
                pinned: Some(m.geom.is_pinned),
                run,
                focus: Some(m.is_focused),
                already_running: false,
                pane_initial_contents: m.pane_contents.clone(),
                logical_position: None,
                borderless: Some(m.is_borderless),
                default_fg: m.default_fg.clone(),
                default_bg: m.default_bg.clone(),
            }
        })
        .collect()
}

fn get_x_lims(geoms: &[PaneLayoutManifest]) -> Option<(usize, usize)> {
    match (
        geoms.iter().map(|g| g.geom.x).min(),
        geoms
            .iter()
            .map(|g| g.geom.x + g.geom.cols.as_usize())
            .max(),
    ) {
        (Some(x_min), Some(x_max)) => Some((x_min, x_max)),
        _ => None,
    }
}

fn get_y_lims(geoms: &[PaneLayoutManifest]) -> Option<(usize, usize)> {
    match (
        geoms.iter().map(|g| g.geom.y).min(),
        geoms
            .iter()
            .map(|g| g.geom.y + g.geom.rows.as_usize())
            .max(),
    ) {
        (Some(y_min), Some(y_max)) => Some((y_min, y_max)),
        _ => None,
    }
}

/// Returns the `SplitDirection` as well as the values, on the axis
/// perpendicular the `SplitDirection`, for which there is a split spanning
/// the max_cols or max_rows of the domain. The values are ordered
/// increasingly and contains the boundaries of the domain.
fn get_splits(geoms: &[PaneLayoutManifest]) -> Option<(SplitDirection, Vec<usize>)> {
    if geoms.len() == 1 {
        return None;
    }
    let (x_lims, y_lims) = match (get_x_lims(geoms), get_y_lims(geoms)) {
        (Some(x_lims), Some(y_lims)) => (x_lims, y_lims),
        _ => return None,
    };
    let mut direction = SplitDirection::default();
    let mut splits = match direction {
        SplitDirection::Vertical => get_col_splits(geoms, &x_lims, &y_lims),
        SplitDirection::Horizontal => get_row_splits(geoms, &x_lims, &y_lims),
    };
    if splits.len() <= 2 {
        // ie only the boundaries are present and no real split has been found
        direction = !direction;
        splits = match direction {
            SplitDirection::Vertical => get_col_splits(geoms, &x_lims, &y_lims),
            SplitDirection::Horizontal => get_row_splits(geoms, &x_lims, &y_lims),
        };
    }
    if splits.len() <= 2 {
        // ie no real split has been found in both directions
        None
    } else {
        Some((direction, splits))
    }
}

/// Returns a vector containing the abscisse (x) of the cols that split the
/// domain including the boundaries, ie the min and max abscisse values.
fn get_col_splits(
    geoms: &[PaneLayoutManifest],
    (_, x_max): &(usize, usize),
    (y_min, y_max): &(usize, usize),
) -> Vec<usize> {
    let max_rows = y_max - y_min;
    let mut splits = Vec::new();
    let mut sorted_geoms = geoms.to_vec();
    sorted_geoms.sort_by_key(|g| g.geom.x);
    for x in sorted_geoms.iter().map(|g| g.geom.x) {
        if splits.contains(&x) {
            continue;
        }
        if sorted_geoms
            .iter()
            .filter(|g| g.geom.x == x)
            .map(|g| g.geom.rows.as_usize())
            .sum::<usize>()
            == max_rows
        {
            splits.push(x);
        };
    }
    splits.push(*x_max); // Necessary as `g.x` is from the upper-left corner
    splits
}

/// Returns a vector containing the coordinate (y) of the rows that split the
/// domain including the boundaries, ie the min and max coordinate values.
fn get_row_splits(
    geoms: &[PaneLayoutManifest],
    (x_min, x_max): &(usize, usize),
    (_, y_max): &(usize, usize),
) -> Vec<usize> {
    let max_cols = x_max - x_min;
    let mut splits = Vec::new();
    let mut sorted_geoms = geoms.to_vec();
    sorted_geoms.sort_by_key(|g| g.geom.y);

    //  here we make sure the various panes in all the stacks aren't counted as splits, since
    //  stacked panes must always stay togethyer - we group them into one "geom" for the purposes
    //  of figuring out their splits
    let mut stack_geoms: HashMap<usize, Vec<PaneLayoutManifest>> = HashMap::new();
    let mut all_geoms = vec![];
    for pane_layout_manifest in sorted_geoms.drain(..) {
        if let Some(stack_id) = pane_layout_manifest.geom.stacked {
            stack_geoms
                .entry(stack_id)
                .or_default()
                .push(pane_layout_manifest)
        } else {
            all_geoms.push(pane_layout_manifest);
        }
    }
    for (_stack_id, mut geoms_in_stack) in stack_geoms.into_iter() {
        let mut geom_of_whole_stack = geoms_in_stack.remove(0);
        if let Some(last_geom) = geoms_in_stack.last() {
            geom_of_whole_stack
                .geom
                .rows
                .set_inner(last_geom.geom.y + last_geom.geom.rows.as_usize())
        }
        all_geoms.push(geom_of_whole_stack);
    }

    all_geoms.sort_by_key(|g| g.geom.y);

    for y in all_geoms.iter().map(|g| g.geom.y) {
        if splits.contains(&y) {
            continue;
        }
        if all_geoms
            .iter()
            .filter(|g| g.geom.y == y)
            .map(|g| g.geom.cols.as_usize())
            .sum::<usize>()
            == max_cols
        {
            splits.push(y);
        };
    }
    splits.push(*y_max); // Necessary as `g.y` is from the upper-left corner
    splits
}

/// Get the constraint of the domain considered, base on the rows or columns,
/// depending on the split direction provided.
fn get_domain_constraint(
    geoms: &[PaneLayoutManifest],
    split_direction: &SplitDirection,
    (v_min, v_max): (usize, usize),
) -> Option<Constraint> {
    match split_direction {
        SplitDirection::Horizontal => get_domain_row_constraint(geoms, (v_min, v_max)),
        SplitDirection::Vertical => get_domain_col_constraint(geoms, (v_min, v_max)),
    }
}

fn get_domain_col_constraint(
    geoms: &[PaneLayoutManifest],
    (x_min, x_max): (usize, usize),
) -> Option<Constraint> {
    let mut percent = 0.0;
    let mut x = x_min;
    while x != x_max {
        // we only look at one (ie the last) geom that has value `x` for `g.x`
        let geom = geoms.iter().rfind(|g| g.geom.x == x);
        match geom {
            Some(geom) => {
                if let Some(size) = geom.geom.cols.as_percent() {
                    percent += size;
                }
                x += geom.geom.cols.as_usize();
            },
            None => {
                return None;
            },
        }
    }
    if percent == 0.0 {
        Some(Constraint::Fixed(x_max - x_min))
    } else {
        Some(Constraint::Percent(percent))
    }
}

fn get_domain_row_constraint(
    geoms: &[PaneLayoutManifest],
    (y_min, y_max): (usize, usize),
) -> Option<Constraint> {
    let mut percent = 0.0;
    let mut y = y_min;
    while y != y_max {
        // we only look at one (ie the last) geom that has value `y` for `g.y`
        let geom = geoms.iter().rfind(|g| g.geom.y == y);
        match geom {
            Some(geom) => {
                if let Some(size) = geom.geom.rows.as_percent() {
                    percent += size;
                }
                y += geom.geom.rows.as_usize();
            },
            None => {
                return None;
            },
        }
    }
    if percent == 0.0 {
        Some(Constraint::Fixed(y_max - y_min))
    } else {
        Some(Constraint::Percent(percent))
    }
}

/// Returns split sizes for all the children of a `TiledPaneLayout` based on
/// their constraints.
fn get_split_sizes(constraints: &[Constraint]) -> Vec<Option<SplitSize>> {
    let mut split_sizes = Vec::new();
    let max_percent = constraints
        .iter()
        .filter_map(|c| match c {
            Constraint::Percent(size) => Some(size),
            _ => None,
        })
        .sum::<f64>();
    for constraint in constraints {
        let split_size = match constraint {
            Constraint::Fixed(size) => Some(SplitSize::Fixed(*size)),
            Constraint::Percent(size) => {
                if size == &max_percent {
                    None
                } else {
                    Some(SplitSize::Percent((100.0 * size / max_percent) as usize))
                }
            },
        };
        split_sizes.push(split_size);
    }
    split_sizes
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::pane_size::Dimension;
    use expect_test::expect;
    use insta::assert_snapshot;
    use serde_json::Value;
    use std::collections::HashMap;
    const PANEGEOMS_JSON: &[&[&str]] = &[
        &[
            r#"{ "x": 0, "y": 1, "rows": { "constraint": "Percent(100.0)", "inner": 43 }, "cols": { "constraint": "Percent(100.0)", "inner": 211 }, "is_stacked": false }"#,
            r#"{ "x": 0, "y": 0, "rows": { "constraint": "Fixed(1)", "inner": 1 }, "cols": { "constraint": "Percent(100.0)", "inner": 211 }, "is_stacked": false }"#,
            r#"{ "x": 0, "y": 44, "rows": { "constraint": "Fixed(2)", "inner": 2 }, "cols": { "constraint": "Percent(100.0)", "inner": 211 }, "is_stacked": false }"#,
        ],
        &[
            r#"{ "x": 0, "y": 0, "rows": { "constraint": "Percent(100.0)", "inner": 26 }, "cols": { "constraint": "Percent(100.0)", "inner": 211 }, "is_stacked": false }"#,
            r#"{ "x": 0, "y": 26, "rows": { "constraint": "Fixed(20)", "inner": 20 }, "cols": { "constraint": "Fixed(50)", "inner": 50 }, "is_stacked": false }"#,
            r#"{ "x": 50, "y": 26, "rows": { "constraint": "Fixed(20)", "inner": 20 }, "cols": { "constraint": "Percent(100.0)", "inner": 161 }, "is_stacked": false }"#,
        ],
        &[
            r#"{ "x": 0, "y": 0, "rows": { "constraint": "Fixed(10)", "inner": 10 }, "cols": { "constraint": "Percent(50.0)", "inner": 106 }, "is_stacked": false }"#,
            r#"{ "x": 106, "y": 0, "rows": { "constraint": "Fixed(10)", "inner": 10 }, "cols": { "constraint": "Percent(50.0)", "inner": 105 }, "is_stacked": false }"#,
            r#"{ "x": 0, "y": 10, "rows": { "constraint": "Percent(100.0)", "inner": 26 }, "cols": { "constraint": "Fixed(40)", "inner": 40 }, "is_stacked": false }"#,
            r#"{ "x": 40, "y": 10, "rows": { "constraint": "Percent(100.0)", "inner": 26 }, "cols": { "constraint": "Percent(100.0)", "inner": 131 }, "is_stacked": false }"#,
            r#"{ "x": 171, "y": 10, "rows": { "constraint": "Percent(100.0)", "inner": 26 }, "cols": { "constraint": "Fixed(40)", "inner": 40 }, "is_stacked": false }"#,
            r#"{ "x": 0, "y": 36, "rows": { "constraint": "Fixed(10)", "inner": 10 }, "cols": { "constraint": "Percent(50.0)", "inner": 106 }, "is_stacked": false }"#,
            r#"{ "x": 106, "y": 36, "rows": { "constraint": "Fixed(10)", "inner": 10 }, "cols": { "constraint": "Percent(50.0)", "inner": 105 }, "is_stacked": false }"#,
        ],
        &[
            r#"{ "x": 0, "y": 0, "rows": { "constraint": "Percent(30.0)", "inner": 11 }, "cols": { "constraint": "Percent(35.0)", "inner": 74 }, "is_stacked": false }"#,
            r#"{ "x": 0, "y": 11, "rows": { "constraint": "Percent(30.0)", "inner": 11 }, "cols": { "constraint": "Percent(35.0)", "inner": 74 }, "is_stacked": false }"#,
            r#"{ "x": 0, "y": 22, "rows": { "constraint": "Percent(40.0)", "inner": 14 }, "cols": { "constraint": "Percent(35.0)", "inner": 74 }, "is_stacked": false }"#,
            r#"{ "x": 74, "y": 0, "rows": { "constraint": "Percent(100.0)", "inner": 36 }, "cols": { "constraint": "Percent(35.0)", "inner": 74 }, "is_stacked": false }"#,
            r#"{ "x": 0, "y": 36, "rows": { "constraint": "Fixed(10)", "inner": 10 }, "cols": { "constraint": "Percent(70.0)", "inner": 148 }, "is_stacked": false }"#,
            r#"{ "x": 148, "y": 0, "rows": { "constraint": "Percent(100.0)", "inner": 46 }, "cols": { "constraint": "Percent(30.0)", "inner": 63 }, "is_stacked": false }"#,
        ],
        &[
            r#"{ "x": 0, "y": 0, "rows": { "constraint": "Fixed(5)", "inner": 5 }, "cols": { "constraint": "Percent(100.0)", "inner": 211 }, "is_stacked": false }"#,
            r#"{ "x": 0, "y": 5, "rows": { "constraint": "Percent(100.0)", "inner": 36 }, "cols": { "constraint": "Fixed(20)", "inner": 20 }, "is_stacked": false }"#,
            r#"{ "x": 20, "y": 5, "rows": { "constraint": "Percent(100.0)", "inner": 36 }, "cols": { "constraint": "Percent(50.0)", "inner": 86 }, "is_stacked": false }"#,
            r#"{ "x": 106, "y": 5, "rows": { "constraint": "Percent(100.0)", "inner": 36 }, "cols": { "constraint": "Percent(50.0)", "inner": 85 }, "is_stacked": false }"#,
            r#"{ "x": 191, "y": 5, "rows": { "constraint": "Percent(100.0)", "inner": 36 }, "cols": { "constraint": "Fixed(20)", "inner": 20 }, "is_stacked": false }"#,
            r#"{ "x": 0, "y": 41, "rows": { "constraint": "Fixed(5)", "inner": 5 }, "cols": { "constraint": "Percent(100.0)", "inner": 211 }, "is_stacked": false }"#,
        ],
    ];

    #[test]
    fn can_serialize_single_terminal_snapshot() {
        use crate::input::command::RunCommand;

        // Command panes intentionally omit scrollback; ordinary shells retain it.
        for command in [false, true] {
            let cwd = PathBuf::from("/tmp/snapshot work");
            let title = "single terminal";
            let contents = "saved shell output\n";
            let manifest = GlobalLayoutManifest {
                tabs: vec![(
                    "proof-sleep".to_owned(),
                    TabLayoutManifest {
                        tiled_panes: vec![PaneLayoutManifest {
                            geom: PaneGeom {
                                rows: Dimension::fixed(24),
                                cols: Dimension::fixed(80),
                                ..Default::default()
                            },
                            run: command.then(|| {
                                Run::Command(RunCommand {
                                    command: PathBuf::from("sleep"),
                                    args: vec!["600".to_owned()],
                                    ..Default::default()
                                })
                            }),
                            cwd: Some(cwd.clone()),
                            title: Some(title.to_owned()),
                            is_focused: true,
                            pane_contents: Some(contents.to_owned()),
                            ..Default::default()
                        }],
                        ..Default::default()
                    },
                )],
                ..Default::default()
            };
            let (serialized, contents_files) = serialize_session_layout(manifest).unwrap();
            let document: KdlDocument = serialized.parse().unwrap();
            let tab = document
                .get("layout")
                .unwrap()
                .children()
                .unwrap()
                .get("tab")
                .unwrap()
                .children()
                .unwrap();
            assert_eq!(tab.nodes().len(), 1, "must serialize the actual leaf");
            let pane_node = tab.get("pane").unwrap();
            assert_eq!(
                pane_node.get("name").unwrap().value().as_string(),
                Some(title)
            );
            assert_eq!(contents_files.len(), usize::from(!command));

            let directory = tempfile::tempdir().unwrap();
            for (filename, value) in &contents_files {
                std::fs::write(directory.path().join(filename), value).unwrap();
            }
            if !command {
                let filename = pane_node
                    .get("contents_file")
                    .unwrap()
                    .value()
                    .as_string()
                    .unwrap();
                assert_eq!(
                    contents_files.get(filename).map(String::as_str),
                    Some(contents)
                );
            }
            let parsed = Layout::from_kdl(
                &serialized,
                Some(
                    directory
                        .path()
                        .join("session-layout.kdl")
                        .display()
                        .to_string(),
                ),
                None,
                None,
            )
            .unwrap();
            assert_eq!(parsed.tabs.len(), 1);
            let (tab_name, tiled, floating) = &parsed.tabs[0];
            assert_eq!(tab_name.as_deref(), Some("proof-sleep"));
            assert!(floating.is_empty());
            assert_eq!(tiled.children.len(), 1);
            let pane = &tiled.children[0];
            assert!(pane.children.is_empty());
            assert_eq!(pane.name.as_deref(), Some(title));
            assert_eq!(pane.focus, Some(true));
            assert_eq!(pane.run.as_ref().and_then(Run::get_cwd), Some(cwd));
            if command {
                let Some(Run::Command(run)) = &pane.run else {
                    panic!("single terminal command was lost");
                };
                assert_eq!(run.command, PathBuf::from("sleep"));
                assert_eq!(run.args, vec!["600".to_owned()]);
                assert!(run.hold_on_start);
                assert!(pane.pane_initial_contents.is_none());
            } else {
                assert_eq!(pane.pane_initial_contents.as_deref(), Some(contents));
            }
        }
    }

    #[test]
    fn tab_snapshot_preserves_tiled_and_floating_structure() {
        for tiled_count in [0, 1, 2] {
            for floating_count in [0, 1] {
                let manifest = GlobalLayoutManifest {
                    tabs: vec![(
                        "topology".to_owned(),
                        TabLayoutManifest {
                            tiled_panes: (0..tiled_count)
                                .map(|index| PaneLayoutManifest {
                                    geom: PaneGeom {
                                        y: index * 10,
                                        rows: Dimension::fixed(10),
                                        cols: Dimension::fixed(80),
                                        ..Default::default()
                                    },
                                    title: Some(format!("tiled-{index}")),
                                    ..Default::default()
                                })
                                .collect(),
                            floating_panes: (0..floating_count)
                                .map(|_| PaneLayoutManifest {
                                    geom: PaneGeom {
                                        rows: Dimension::fixed(5),
                                        cols: Dimension::fixed(20),
                                        ..Default::default()
                                    },
                                    title: Some("floating".to_owned()),
                                    ..Default::default()
                                })
                                .collect(),
                            ..Default::default()
                        },
                    )],
                    ..Default::default()
                };
                let (serialized, _) = serialize_session_layout(manifest).unwrap();
                let document: KdlDocument = serialized.parse().unwrap();
                let tab = document
                    .get("layout")
                    .unwrap()
                    .children()
                    .unwrap()
                    .get("tab")
                    .unwrap()
                    .children()
                    .unwrap();
                let tiled: Vec<_> = tab
                    .nodes()
                    .iter()
                    .filter(|node| node.name().value() == "pane")
                    .collect();
                assert_eq!(tiled.len(), tiled_count);
                for (index, pane) in tiled.iter().enumerate() {
                    assert_eq!(
                        pane.get("name").unwrap().value().as_string(),
                        Some(format!("tiled-{index}").as_str())
                    );
                    assert!(
                        pane.children().is_none(),
                        "must not add grouping around default splits"
                    );
                }
                let floating = tab.get("floating_panes");
                assert_eq!(usize::from(floating.is_some()), floating_count);
                if let Some(floating) = floating {
                    let panes = floating.children().unwrap();
                    assert_eq!(panes.nodes().len(), 1);
                    assert_eq!(
                        panes
                            .get("pane")
                            .unwrap()
                            .get("name")
                            .unwrap()
                            .value()
                            .as_string(),
                        Some("floating")
                    );
                }
                Layout::from_kdl(&serialized, None, None, None).unwrap();
            }
        }
    }

    #[test]
    fn geoms() {
        let geoms = PANEGEOMS_JSON[0]
            .iter()
            .map(|pg| parse_panegeom_from_json(pg))
            .map(|geom| PaneLayoutManifest {
                geom,
                ..Default::default()
            })
            .collect();
        let tab_layout_manifest = TabLayoutManifest {
            tiled_panes: geoms,
            ..Default::default()
        };
        let global_layout_manifest = GlobalLayoutManifest {
            tabs: vec![("Tab #1".to_owned(), tab_layout_manifest)],
            ..Default::default()
        };
        let kdl = serialize_session_layout(global_layout_manifest).unwrap();
        expect![[r#"
            layout {
                tab canvas_state="materialized" name="Tab #1" {
                    pane size=1
                    pane
                    pane size=2
                }
            }
        "#]]
        .assert_eq(&kdl.0);

        let geoms = PANEGEOMS_JSON[1]
            .iter()
            .map(|pg| parse_panegeom_from_json(pg))
            .map(|geom| PaneLayoutManifest {
                geom,
                ..Default::default()
            })
            .collect();
        let tab_layout_manifest = TabLayoutManifest {
            tiled_panes: geoms,
            ..Default::default()
        };
        let global_layout_manifest = GlobalLayoutManifest {
            tabs: vec![("Tab #1".to_owned(), tab_layout_manifest)],
            ..Default::default()
        };
        let kdl = serialize_session_layout(global_layout_manifest).unwrap();
        expect![[r#"
            layout {
                tab canvas_state="materialized" name="Tab #1" {
                    pane
                    pane size=20 split_direction="vertical" {
                        pane size=50
                        pane
                    }
                }
            }
        "#]]
        .assert_eq(&kdl.0);

        let geoms = PANEGEOMS_JSON[2]
            .iter()
            .map(|pg| parse_panegeom_from_json(pg))
            .map(|geom| PaneLayoutManifest {
                geom,
                ..Default::default()
            })
            .collect();
        let tab_layout_manifest = TabLayoutManifest {
            tiled_panes: geoms,
            ..Default::default()
        };
        let global_layout_manifest = GlobalLayoutManifest {
            tabs: vec![("Tab #1".to_owned(), tab_layout_manifest)],
            ..Default::default()
        };
        let kdl = serialize_session_layout(global_layout_manifest).unwrap();
        expect![[r#"
            layout {
                tab canvas_state="materialized" name="Tab #1" {
                    pane size=10 split_direction="vertical" {
                        pane size="50%"
                        pane size="50%"
                    }
                    pane split_direction="vertical" {
                        pane size=40
                        pane
                        pane size=40
                    }
                    pane size=10 split_direction="vertical" {
                        pane size="50%"
                        pane size="50%"
                    }
                }
            }
        "#]]
        .assert_eq(&kdl.0);

        let geoms = PANEGEOMS_JSON[3]
            .iter()
            .map(|pg| parse_panegeom_from_json(pg))
            .map(|geom| PaneLayoutManifest {
                geom,
                ..Default::default()
            })
            .collect();
        let tab_layout_manifest = TabLayoutManifest {
            tiled_panes: geoms,
            ..Default::default()
        };
        let global_layout_manifest = GlobalLayoutManifest {
            tabs: vec![("Tab #1".to_owned(), tab_layout_manifest)],
            ..Default::default()
        };
        let kdl = serialize_session_layout(global_layout_manifest).unwrap();
        expect![[r#"
            layout {
                tab canvas_state="materialized" name="Tab #1" {
                    pane split_direction="vertical" {
                        pane size="70%" {
                            pane split_direction="vertical" {
                                pane size="50%" {
                                    pane size="30%"
                                    pane size="30%"
                                    pane size="40%"
                                }
                                pane size="50%"
                            }
                            pane size=10
                        }
                        pane size="30%"
                    }
                }
            }
        "#]]
        .assert_eq(&kdl.0);

        let geoms = PANEGEOMS_JSON[4]
            .iter()
            .map(|pg| parse_panegeom_from_json(pg))
            .map(|geom| PaneLayoutManifest {
                geom,
                ..Default::default()
            })
            .collect();
        let tab_layout_manifest = TabLayoutManifest {
            tiled_panes: geoms,
            ..Default::default()
        };
        let global_layout_manifest = GlobalLayoutManifest {
            tabs: vec![("Tab #1".to_owned(), tab_layout_manifest)],
            ..Default::default()
        };
        let kdl = serialize_session_layout(global_layout_manifest).unwrap();
        expect![[r#"
            layout {
                tab canvas_state="materialized" name="Tab #1" {
                    pane size=5
                    pane split_direction="vertical" {
                        pane size=20
                        pane size="50%"
                        pane size="50%"
                        pane size=20
                    }
                    pane size=5
                }
            }
        "#]]
        .assert_eq(&kdl.0);
    }

    #[test]
    fn global_cwd() {
        let global_layout_manifest = GlobalLayoutManifest {
            global_cwd: Some(PathBuf::from("/path/to/m\"y/global cwd")),
            ..Default::default()
        };
        let kdl = serialize_session_layout(global_layout_manifest).unwrap();
        assert_snapshot!(kdl.0);
    }

    #[test]
    fn can_serialize_tab_name() {
        let global_layout_manifest = GlobalLayoutManifest {
            tabs: vec![("my \"tab \\name".to_owned(), TabLayoutManifest::default())],
            ..Default::default()
        };
        let kdl = serialize_session_layout(global_layout_manifest).unwrap();
        assert_snapshot!(kdl.0);
    }

    #[test]
    fn serializes_only_assigned_tab_instance_identity() {
        let assigned = GlobalLayoutManifest {
            tabs: vec![(
                "owned".to_owned(),
                TabLayoutManifest {
                    tab_instance_id: "33333333333333333333333333333333".to_owned(),
                    ..Default::default()
                },
            )],
            ..Default::default()
        };
        let assigned_kdl = serialize_session_layout(assigned).unwrap().0;
        assert!(assigned_kdl.contains("vc_tab_instance_id=\"33333333333333333333333333333333\""));

        let fresh = GlobalLayoutManifest {
            tabs: vec![("fresh".to_owned(), TabLayoutManifest::default())],
            ..Default::default()
        };
        let fresh_kdl = serialize_session_layout(fresh).unwrap().0;
        assert!(!fresh_kdl.contains("vc_tab_instance_id"));
    }
    #[test]
    fn can_serialize_tab_focus() {
        let tab_layout_manifest = TabLayoutManifest {
            is_focused: true,
            ..Default::default()
        };
        let global_layout_manifest = GlobalLayoutManifest {
            tabs: vec![("Tab #1".to_owned(), tab_layout_manifest)],
            ..Default::default()
        };
        let kdl = serialize_session_layout(global_layout_manifest).unwrap();
        assert_snapshot!(kdl.0);
    }
    #[test]
    fn can_serialize_tab_hide_floating_panes() {
        let tab_layout_manifest = TabLayoutManifest {
            hide_floating_panes: true,
            ..Default::default()
        };
        let global_layout_manifest = GlobalLayoutManifest {
            tabs: vec![("Tab #1".to_owned(), tab_layout_manifest)],
            ..Default::default()
        };
        let kdl = serialize_session_layout(global_layout_manifest).unwrap();
        assert_snapshot!(kdl.0);
    }
    #[test]
    fn can_serialize_tab_with_tiled_panes() {
        use crate::input::command::RunCommand;
        use crate::input::layout::RunPlugin;
        let mut plugin_configuration = BTreeMap::new();
        plugin_configuration.insert("key 1\"\\".to_owned(), "val 1\"\\".to_owned());
        plugin_configuration.insert("key 2\"\\".to_owned(), "val 2\"\\".to_owned());
        let tab_layout_manifest = TabLayoutManifest {
            tiled_panes: vec![
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 0,
                        y: 0,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: None,
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    run: Some(Run::Cwd(PathBuf::from("/tmp/\"my/cool cwd"))),
                    geom: PaneGeom {
                        x: 0,
                        y: 10,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: None,
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    run: Some(Run::EditFile(
                        PathBuf::from("/tmp/\"my/cool cwd/my-file"),
                        None,
                        None,
                    )),
                    geom: PaneGeom {
                        x: 0,
                        y: 20,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: None,
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    run: Some(Run::Command(RunCommand {
                        command: PathBuf::from("/tmp/\"my/cool cwd/command.sh"),
                        ..Default::default()
                    })),
                    geom: PaneGeom {
                        x: 0,
                        y: 30,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: None,
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    run: Some(Run::Command(RunCommand {
                        command: PathBuf::from("/tmp/\"my/cool cwd/command.sh"),
                        args: vec![
                            "--arg1".to_owned(),
                            "arg\"2".to_owned(),
                            "arg > \\3".to_owned(),
                        ],
                        ..Default::default()
                    })),
                    geom: PaneGeom {
                        x: 0,
                        y: 40,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: None,
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    run: Some(Run::Plugin(RunPluginOrAlias::RunPlugin(
                        RunPlugin::from_url("file:/tmp/\"my/cool cwd/plugin.wasm").unwrap(),
                    ))),
                    geom: PaneGeom {
                        x: 0,
                        y: 50,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: None,
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    run: Some(Run::Plugin(RunPluginOrAlias::RunPlugin(
                        RunPlugin::from_url("file:/tmp/\"my/cool cwd/plugin.wasm")
                            .unwrap()
                            .with_configuration(plugin_configuration),
                    ))),
                    geom: PaneGeom {
                        x: 0,
                        y: 60,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: None,
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    is_borderless: true,
                    geom: PaneGeom {
                        x: 0,
                        y: 70,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: None,
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    title: Some("my cool \\ \"pane_title\"".to_owned()),
                    is_focused: true,
                    pane_contents: Some("can has pane contents".to_owned()),
                    geom: PaneGeom {
                        x: 0,
                        y: 80,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: None,
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let global_layout_manifest = GlobalLayoutManifest {
            tabs: vec![("Tab with \"tiled panes\"".to_owned(), tab_layout_manifest)],
            ..Default::default()
        };
        let kdl = serialize_session_layout(global_layout_manifest).unwrap();
        assert_snapshot!(kdl.0);
    }
    #[test]
    fn can_serialize_tab_with_floating_panes() {
        use crate::input::command::RunCommand;
        use crate::input::layout::RunPlugin;
        let mut plugin_configuration = BTreeMap::new();
        plugin_configuration.insert("key 1\"\\".to_owned(), "val 1\"\\".to_owned());
        plugin_configuration.insert("key 2\"\\".to_owned(), "val 2\"\\".to_owned());
        let tab_layout_manifest = TabLayoutManifest {
            floating_panes: vec![
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 0,
                        y: 0,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: None,
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    run: Some(Run::Cwd(PathBuf::from("/tmp/\"my/cool cwd"))),
                    geom: PaneGeom {
                        x: 0,
                        y: 10,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: None,
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    run: Some(Run::EditFile(
                        PathBuf::from("/tmp/\"my/cool cwd/my-file"),
                        None,
                        None,
                    )),
                    geom: PaneGeom {
                        x: 0,
                        y: 20,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: None,
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    run: Some(Run::Command(RunCommand {
                        command: PathBuf::from("/tmp/\"my/cool cwd/command.sh"),
                        ..Default::default()
                    })),
                    geom: PaneGeom {
                        x: 0,
                        y: 30,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: None,
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    run: Some(Run::Command(RunCommand {
                        command: PathBuf::from("/tmp/\"my/cool cwd/command.sh"),
                        args: vec![
                            "--arg1".to_owned(),
                            "arg\"2".to_owned(),
                            "arg > \\3".to_owned(),
                        ],
                        ..Default::default()
                    })),
                    geom: PaneGeom {
                        x: 0,
                        y: 40,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: None,
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    run: Some(Run::Plugin(RunPluginOrAlias::RunPlugin(
                        RunPlugin::from_url("file:/tmp/\"my/cool cwd/plugin.wasm").unwrap(),
                    ))),
                    geom: PaneGeom {
                        x: 0,
                        y: 50,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: None,
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    run: Some(Run::Plugin(RunPluginOrAlias::RunPlugin(
                        RunPlugin::from_url("file:/tmp/\"my/cool cwd/plugin.wasm")
                            .unwrap()
                            .with_configuration(plugin_configuration),
                    ))),
                    geom: PaneGeom {
                        x: 0,
                        y: 60,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: None,
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    // note that in this case, `is_borderless` should be ignored because this is a
                    // floating pane
                    is_borderless: true,
                    geom: PaneGeom {
                        x: 0,
                        y: 70,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: None,
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    title: Some("my cool \\ \"pane_title\"".to_owned()),
                    is_focused: true,
                    pane_contents: Some("can has pane contents".to_owned()),
                    geom: PaneGeom {
                        x: 0,
                        y: 80,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: None,
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let global_layout_manifest = GlobalLayoutManifest {
            tabs: vec![(
                "Tab with \"floating panes\"".to_owned(),
                tab_layout_manifest,
            )],
            ..Default::default()
        };
        let kdl = serialize_session_layout(global_layout_manifest).unwrap();
        assert_snapshot!(kdl.0);
    }
    #[test]
    fn can_serialize_tab_with_stacked_panes() {
        let tab_layout_manifest = TabLayoutManifest {
            tiled_panes: vec![
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 0,
                        y: 0,
                        rows: Dimension::fixed(1),
                        cols: Dimension::fixed(10),
                        stacked: Some(0),
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 0,
                        y: 1,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: Some(0),
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 0,
                        y: 11,
                        rows: Dimension::fixed(1),
                        cols: Dimension::fixed(10),
                        stacked: Some(0),
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let global_layout_manifest = GlobalLayoutManifest {
            tabs: vec![("Tab with \"stacked panes\"".to_owned(), tab_layout_manifest)],
            ..Default::default()
        };
        let kdl = serialize_session_layout(global_layout_manifest).unwrap();
        assert_snapshot!(kdl.0);
    }
    #[test]
    fn can_serialize_tab_with_multiple_stacked_panes_in_the_same_node() {
        let tab_layout_manifest = TabLayoutManifest {
            tiled_panes: vec![
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 0,
                        y: 0,
                        rows: Dimension::fixed(1),
                        cols: Dimension::fixed(10),
                        stacked: Some(0),
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 0,
                        y: 1,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: Some(0),
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 0,
                        y: 11,
                        rows: Dimension::fixed(1),
                        cols: Dimension::fixed(10),
                        stacked: Some(0),
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 0,
                        y: 12,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: None,
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 0,
                        y: 22,
                        rows: Dimension::fixed(1),
                        cols: Dimension::fixed(10),
                        stacked: Some(1),
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 0,
                        y: 23,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: Some(1),
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 0,
                        y: 33,
                        rows: Dimension::fixed(1),
                        cols: Dimension::fixed(10),
                        stacked: Some(1),
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let global_layout_manifest = GlobalLayoutManifest {
            tabs: vec![("Tab with \"stacked panes\"".to_owned(), tab_layout_manifest)],
            ..Default::default()
        };
        let kdl = serialize_session_layout(global_layout_manifest).unwrap();
        assert_snapshot!(kdl.0);
    }
    #[test]
    fn can_serialize_tab_with_multiple_stacks_next_to_eachother() {
        let tab_layout_manifest = TabLayoutManifest {
            tiled_panes: vec![
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 0,
                        y: 0,
                        rows: Dimension::fixed(1),
                        cols: Dimension::fixed(10),
                        stacked: Some(0),
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 0,
                        y: 1,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: Some(0),
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 0,
                        y: 11,
                        rows: Dimension::fixed(1),
                        cols: Dimension::fixed(10),
                        stacked: Some(0),
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 0,
                        y: 12,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: None,
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 0,
                        y: 22,
                        rows: Dimension::fixed(1),
                        cols: Dimension::fixed(10),
                        stacked: Some(1),
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 0,
                        y: 23,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: Some(1),
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 0,
                        y: 33,
                        rows: Dimension::fixed(1),
                        cols: Dimension::fixed(10),
                        stacked: Some(1),
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 10,
                        y: 0,
                        rows: Dimension::fixed(1),
                        cols: Dimension::fixed(10),
                        stacked: Some(2),
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 10,
                        y: 1,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: Some(2),
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 10,
                        y: 11,
                        rows: Dimension::fixed(1),
                        cols: Dimension::fixed(10),
                        stacked: Some(2),
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 10,
                        y: 12,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: None,
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 10,
                        y: 22,
                        rows: Dimension::fixed(1),
                        cols: Dimension::fixed(10),
                        stacked: Some(3),
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 10,
                        y: 23,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: Some(3),
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 10,
                        y: 33,
                        rows: Dimension::fixed(1),
                        cols: Dimension::fixed(10),
                        stacked: Some(3),
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let global_layout_manifest = GlobalLayoutManifest {
            tabs: vec![("Tab with \"stacked panes\"".to_owned(), tab_layout_manifest)],
            ..Default::default()
        };
        let kdl = serialize_session_layout(global_layout_manifest).unwrap();
        assert_snapshot!(kdl.0);
    }
    fn completeness_tab(second_pane_x: Option<usize>) -> TabLayoutManifest {
        let pane = PaneLayoutManifest {
            geom: PaneGeom {
                rows: Dimension::fixed(10),
                cols: Dimension::fixed(10),
                ..Default::default()
            },
            pane_contents: Some("captured contents".to_owned()),
            ..Default::default()
        };
        let mut tiled_panes = vec![pane.clone()];
        if let Some(x) = second_pane_x {
            let mut second = pane;
            second.geom.x = x;
            tiled_panes.push(second);
        }
        TabLayoutManifest {
            tiled_panes,
            ..Default::default()
        }
    }

    #[test]
    fn incomplete_capture_rejects_valid_and_invalid_tabs_in_either_order() {
        let valid = completeness_tab(None);
        // Two 10-column panes with a 10-column hole cannot form a split layout.
        let invalid = completeness_tab(Some(20));
        assert!(get_tiled_panes_layout_from_panegeoms(&invalid.tiled_panes, None).is_none());
        for tabs in [
            vec![
                ("valid".into(), valid.clone()),
                ("invalid".into(), invalid.clone()),
            ],
            vec![("invalid".into(), invalid), ("valid".into(), valid)],
        ] {
            let result = serialize_session_layout(GlobalLayoutManifest {
                tabs,
                ..Default::default()
            });
            assert_eq!(
                result.unwrap_err(),
                "Incomplete session snapshot: failed to serialize a captured tab"
            );
        }
    }

    #[test]
    fn all_invalid_capture_rejects() {
        let invalid = completeness_tab(Some(20));
        assert!(
            serialize_session_layout(GlobalLayoutManifest {
                tabs: vec![
                    ("invalid-1".into(), invalid.clone()),
                    ("invalid-2".into(), invalid)
                ],
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn complete_and_empty_captures_preserve_existing_semantics() {
        let (kdl, contents) = serialize_session_layout(GlobalLayoutManifest {
            tabs: vec![
                ("one".into(), completeness_tab(None)),
                ("two".into(), completeness_tab(Some(10))),
            ],
            ..Default::default()
        })
        .unwrap();
        let document: KdlDocument = kdl.parse().unwrap();
        let tabs: Vec<_> = document
            .get("layout")
            .unwrap()
            .children()
            .unwrap()
            .nodes()
            .iter()
            .filter(|node| node.name().value() == "tab")
            .collect();
        assert_eq!(tabs.len(), 2);
        assert_eq!(
            tabs[0].get("name").unwrap().value().as_string(),
            Some("one")
        );
        assert_eq!(
            tabs[1].get("name").unwrap().value().as_string(),
            Some("two")
        );
        assert_eq!(contents.len(), 3);

        // A genuinely empty capture is not an incomplete capture. It retains
        // the existing layout document semantics without inventing a tab.
        let (kdl, contents) = serialize_session_layout(GlobalLayoutManifest::default()).unwrap();
        let document: KdlDocument = kdl.parse().unwrap();
        assert!(
            document
                .get("layout")
                .unwrap()
                .children()
                .unwrap()
                .nodes()
                .iter()
                .all(|node| node.name().value() != "tab")
        );
        assert!(contents.is_empty());
    }

    #[test]
    fn can_serialize_multiple_tabs() {
        let tab_1_layout_manifest = TabLayoutManifest {
            tiled_panes: vec![PaneLayoutManifest {
                geom: PaneGeom {
                    x: 0,
                    y: 0,
                    rows: Dimension::percent(100.0),
                    cols: Dimension::percent(100.0),
                    stacked: None,
                    is_pinned: false,
                    logical_position: None,
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        let tab_2_layout_manifest = TabLayoutManifest {
            tiled_panes: vec![
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 0,
                        y: 0,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: None,
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
                PaneLayoutManifest {
                    geom: PaneGeom {
                        x: 10,
                        y: 0,
                        rows: Dimension::fixed(10),
                        cols: Dimension::fixed(10),
                        stacked: None,
                        is_pinned: false,
                        logical_position: None,
                    },
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let global_layout_manifest = GlobalLayoutManifest {
            tabs: vec![
                ("First tab".to_owned(), tab_1_layout_manifest),
                ("Second tab".to_owned(), tab_2_layout_manifest),
            ],
            ..Default::default()
        };
        let kdl = serialize_session_layout(global_layout_manifest).unwrap();
        assert_snapshot!(kdl.0);
    }
    #[test]
    fn can_serialize_new_tab_template() {
        let tiled_panes_layout = TiledPaneLayout {
            children: vec![TiledPaneLayout::default(), TiledPaneLayout::default()],
            ..Default::default()
        };

        let floating_panes_layout = vec![
            FloatingPaneLayout::default(),
            FloatingPaneLayout::default(),
            FloatingPaneLayout::default(),
        ];
        let default_layout = Layout {
            template: Some((tiled_panes_layout, floating_panes_layout)),
            ..Default::default()
        };
        let default_layout = Box::new(default_layout);
        let global_layout_manifest = GlobalLayoutManifest {
            default_layout,
            ..Default::default()
        };
        let kdl = serialize_session_layout(global_layout_manifest).unwrap();
        assert_snapshot!(kdl.0);
    }
    #[test]
    fn can_serialize_swap_tiled_panes() {
        let tiled_panes_layout = TiledPaneLayout {
            children: vec![TiledPaneLayout::default(), TiledPaneLayout::default()],
            ..Default::default()
        };
        let mut default_layout = Layout::default();
        let mut swap_tiled_layout_1 = BTreeMap::new();
        let mut swap_tiled_layout_2 = BTreeMap::new();
        swap_tiled_layout_1.insert(LayoutConstraint::MaxPanes(1), tiled_panes_layout.clone());
        swap_tiled_layout_1.insert(LayoutConstraint::MinPanes(1), tiled_panes_layout.clone());
        swap_tiled_layout_1.insert(LayoutConstraint::ExactPanes(1), tiled_panes_layout.clone());
        swap_tiled_layout_1.insert(LayoutConstraint::NoConstraint, tiled_panes_layout.clone());
        swap_tiled_layout_2.insert(LayoutConstraint::MaxPanes(2), tiled_panes_layout.clone());
        swap_tiled_layout_2.insert(LayoutConstraint::MinPanes(2), tiled_panes_layout.clone());
        swap_tiled_layout_2.insert(LayoutConstraint::ExactPanes(2), tiled_panes_layout.clone());
        swap_tiled_layout_2.insert(LayoutConstraint::NoConstraint, tiled_panes_layout.clone());

        let swap_tiled_layouts = vec![
            (swap_tiled_layout_1, None),
            (swap_tiled_layout_2, Some("swap_tiled_layout_2".to_owned())),
        ];
        default_layout.swap_tiled_layouts = swap_tiled_layouts;
        let default_layout = Box::new(default_layout);
        let global_layout_manifest = GlobalLayoutManifest {
            default_layout,
            ..Default::default()
        };
        let kdl = serialize_session_layout(global_layout_manifest).unwrap();
        assert_snapshot!(kdl.0);
    }
    #[test]
    fn can_serialize_swap_floating_panes() {
        let floating_panes_layout = vec![
            FloatingPaneLayout::default(),
            FloatingPaneLayout::default(),
            FloatingPaneLayout::default(),
        ];
        let mut default_layout = Layout::default();
        let mut swap_floating_layout_1 = BTreeMap::new();
        let mut swap_floating_layout_2 = BTreeMap::new();
        swap_floating_layout_1.insert(LayoutConstraint::MaxPanes(1), floating_panes_layout.clone());
        swap_floating_layout_1.insert(LayoutConstraint::MinPanes(1), floating_panes_layout.clone());
        swap_floating_layout_1.insert(
            LayoutConstraint::ExactPanes(1),
            floating_panes_layout.clone(),
        );
        swap_floating_layout_1.insert(
            LayoutConstraint::NoConstraint,
            floating_panes_layout.clone(),
        );
        swap_floating_layout_2.insert(LayoutConstraint::MaxPanes(2), floating_panes_layout.clone());
        swap_floating_layout_2.insert(LayoutConstraint::MinPanes(2), floating_panes_layout.clone());
        swap_floating_layout_2.insert(
            LayoutConstraint::ExactPanes(2),
            floating_panes_layout.clone(),
        );
        swap_floating_layout_2.insert(
            LayoutConstraint::NoConstraint,
            floating_panes_layout.clone(),
        );

        let swap_floating_layouts = vec![
            (swap_floating_layout_1, None),
            (
                swap_floating_layout_2,
                Some("swap_floating_layout_2".to_owned()),
            ),
        ];
        default_layout.swap_floating_layouts = swap_floating_layouts;
        let default_layout = Box::new(default_layout);
        let global_layout_manifest = GlobalLayoutManifest {
            default_layout,
            ..Default::default()
        };
        let kdl = serialize_session_layout(global_layout_manifest).unwrap();
        assert_snapshot!(kdl.0);
    }

    // utility functions
    fn parse_panegeom_from_json(data_str: &str) -> PaneGeom {
        //
        // Expects this input
        //
        //  r#"{ "x": 0, "y": 1, "rows": { "constraint": "Percent(100.0)", "inner": 43 }, "cols": { "constraint": "Percent(100.0)", "inner": 211 }, "is_stacked": false }"#,
        //
        let data: HashMap<String, Value> = serde_json::from_str(data_str).unwrap();
        PaneGeom {
            x: data["x"].to_string().parse().unwrap(),
            y: data["y"].to_string().parse().unwrap(),
            rows: get_dim(&data["rows"]),
            cols: get_dim(&data["cols"]),
            stacked: None,
            is_pinned: false,
            logical_position: None,
        }
    }

    fn get_dim(dim_hm: &Value) -> Dimension {
        let constr_str = dim_hm["constraint"].to_string();

        if constr_str.contains("Fixed") {
            let value = &constr_str[7..constr_str.len() - 2];
            Dimension::fixed(value.parse().unwrap())
        } else if constr_str.contains("Percent") {
            let value = &constr_str[9..constr_str.len() - 2];
            let mut dim = Dimension::percent(value.parse().unwrap());
            dim.set_inner(dim_hm["inner"].to_string().parse().unwrap());
            dim
        } else {
            panic!("Constraint is nor a percent nor fixed");
        }
    }
    fn canvas_snapshot_roundtrip(layout: &Layout) -> (Layout, String) {
        let space = PaneGeom {
            rows: Dimension::fixed(50),
            cols: Dimension::fixed(160),
            ..Default::default()
        };
        let tabs = layout
            .tabs()
            .into_iter()
            .enumerate()
            .map(|(index, (name, tiled, floating))| {
                let tiled_panes = tiled
                    .position_panes_in_space(&space, None, false, false)
                    .unwrap()
                    .into_iter()
                    .map(|(pane, geom)| PaneLayoutManifest {
                        geom,
                        cwd: pane.run.as_ref().and_then(Run::get_cwd),
                        run: pane.run,
                        title: pane.name,
                        is_borderless: pane.borderless.unwrap_or(false),
                        is_focused: pane.focus.unwrap_or(false),
                        pane_contents: pane.pane_initial_contents,
                        default_fg: pane.default_fg,
                        default_bg: pane.default_bg,
                    })
                    .collect();
                let floating_panes = floating
                    .into_iter()
                    .map(|pane| PaneLayoutManifest {
                        geom: PaneGeom {
                            x: pane.x.map(|v| v.to_fixed(160)).unwrap_or(0),
                            y: pane.y.map(|v| v.to_fixed(50)).unwrap_or(0),
                            cols: Dimension::fixed(
                                pane.width.map(|v| v.to_fixed(160)).unwrap_or(20),
                            ),
                            rows: Dimension::fixed(
                                pane.height.map(|v| v.to_fixed(50)).unwrap_or(10),
                            ),
                            is_pinned: pane.pinned.unwrap_or(false),
                            ..Default::default()
                        },
                        cwd: pane.run.as_ref().and_then(Run::get_cwd),
                        run: pane.run,
                        title: pane.name,
                        is_borderless: pane.borderless.unwrap_or(false),
                        is_focused: pane.focus.unwrap_or(false),
                        pane_contents: pane.pane_initial_contents,
                        default_fg: pane.default_fg,
                        default_bg: pane.default_bg,
                    })
                    .collect();
                (
                    name.unwrap_or_default(),
                    TabLayoutManifest {
                        tab_instance_id: tiled.tab_instance_id.unwrap_or_default(),
                        tiled_panes,
                        floating_panes,
                        is_focused: layout.focused_tab_index == Some(index),
                        hide_floating_panes: tiled.hide_floating_panes,
                    },
                )
            })
            .collect();
        let (serialized, contents) = serialize_session_layout(GlobalLayoutManifest {
            default_layout: Box::new(layout.clone()),
            tabs,
            ..Default::default()
        })
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        for (name, value) in contents {
            std::fs::write(directory.path().join(name), value).unwrap();
        }
        let parsed = Layout::from_kdl(
            &serialized,
            Some(directory.path().join("snapshot.kdl").display().to_string()),
            None,
            None,
        )
        .unwrap();
        (parsed, serialized)
    }

    fn canvas_leaves(root: &TiledPaneLayout) -> Vec<&TiledPaneLayout> {
        if root.children.is_empty() {
            vec![root]
        } else {
            root.children.iter().flat_map(canvas_leaves).collect()
        }
    }

    fn canvas_role_counts(root: &TiledPaneLayout) -> BTreeMap<String, usize> {
        let mut roles = BTreeMap::new();
        for pane in canvas_leaves(root) {
            let (_, config) = extract_plugin_and_config(&pane.run);
            if let Some(config) = config {
                if config.inner().get("session_canvas").map(String::as_str) == Some("true") {
                    if let Some(role) = config.inner().get("session_canvas_kind") {
                        *roles.entry(role.clone()).or_insert(0) += 1;
                    }
                }
            }
        }
        roles
    }

    #[test]
    fn canvas_repeated_snapshot_retains_current_tabs_and_semantic_future() {
        let builtin_source = include_str!("../assets/layouts/default.kdl").replace(
            "plugin location=\"compact-bar\" {",
            "plugin location=\"compact-bar\" cwd=\"/tmp/chrome\" {",
        );
        let builtin = Layout::from_kdl(&builtin_source, None, None, None).unwrap();
        let mut layout = Layout::from_kdl(r#"layout {
            new_tab_template { pane name="future" command="sleep" { args "600"; }; }
            tab name="work" focus=true hide_floating_panes=true vc_tab_instance_id="saved-work" {
                pane name="shell" cwd="/tmp/work"
                pane name="command" command="sleep" cwd="/tmp/command" { args "600"; }
                pane name="ordinary-manager" { plugin location="session-manager"; }
                pane name="ordinary-bar" { plugin location="zellij:compact-bar"; }
                floating_panes { pane name="float" x=7 y=8 width=30 height=12 pinned=true focus=true; }
            }
            tab name="other" vc_tab_instance_id="saved-other" { pane; }
            tab name="explicit" canvas_state="materialized" vc_tab_instance_id="saved-explicit" {
                pane name="explicit-shell"
            }
        }"#, None, None, None).unwrap();
        layout.session_layer = builtin.session_layer;
        layout.tabs[0].1.children[0].pane_initial_contents =
            Some("exact shell\n\u{1b}[31mred\u{1b}[0m\n".into());
        let original_layer = layout.session_layer.clone();
        let expected_roles = BTreeMap::from([
            ("compact-bar".into(), 1),
            ("session-manager".into(), 1),
            ("status-bar".into(), 1),
        ]);
        for _ in 0..2 {
            layout = canvas_snapshot_roundtrip(&layout).0;
            assert_eq!(layout.session_layer, original_layer);
            assert_eq!(
                layout.template.as_ref().unwrap().0.canvas_phase,
                CanvasLayoutPhase::Content
            );
            let tabs = layout.tabs();
            assert_eq!(tabs.len(), 3);
            assert_eq!(tabs[0].0.as_deref(), Some("work"));
            assert_eq!(tabs[0].1.tab_instance_id.as_deref(), Some("saved-work"));
            assert_eq!(tabs[1].1.tab_instance_id.as_deref(), Some("saved-other"));
            assert_eq!(tabs[2].1.tab_instance_id.as_deref(), Some("saved-explicit"));
            assert_eq!(layout.focused_tab_index, Some(0));
            assert!(tabs[0].1.hide_floating_panes);
            for (_, tiled, _) in &tabs[..2] {
                assert_eq!(canvas_role_counts(tiled), expected_roles);
            }
            assert!(canvas_role_counts(&tabs[2].1).is_empty());
            assert_eq!(tabs[2].1.pane_count(), 1);
            let leaves = canvas_leaves(&tabs[0].1);
            assert_eq!(leaves.len(), 7, "four content panes and three chrome panes");
            let shell = leaves
                .iter()
                .find(|p| p.name.as_deref() == Some("shell"))
                .unwrap();
            assert_eq!(
                shell.pane_initial_contents.as_deref(),
                Some("exact shell\n\u{1b}[31mred\u{1b}[0m\n")
            );
            assert_eq!(
                shell.run.as_ref().and_then(Run::get_cwd),
                Some(PathBuf::from("/tmp/work"))
            );
            let command = leaves
                .iter()
                .find(|p| p.name.as_deref() == Some("command"))
                .unwrap();
            let Some(Run::Command(run)) = &command.run else {
                panic!("lost command");
            };
            assert_eq!(run.command, PathBuf::from("sleep"));
            assert_eq!(run.args, vec!["600"]);
            assert_eq!(run.cwd, Some(PathBuf::from("/tmp/command")));
            assert!(run.hold_on_start);
            assert!(command.pane_initial_contents.is_none());
            assert_eq!(tabs[0].2.len(), 1);
            let float = &tabs[0].2[0];
            assert_eq!(float.x, Some(PercentOrFixed::Fixed(7)));
            assert_eq!(float.y, Some(PercentOrFixed::Fixed(8)));
            assert_eq!(float.width, Some(PercentOrFixed::Fixed(30)));
            assert_eq!(float.height, Some(PercentOrFixed::Fixed(12)));
            assert_eq!(float.pinned, Some(true));
            assert_eq!(float.focus, Some(true));
            let future = layout.new_tab().0;
            assert_eq!(canvas_role_counts(&future), expected_roles);
            assert_eq!(future.pane_count(), 4);
        }
        // Utils owner replacement keeps existing materialized canvases stable.
        // Screen's atomic adoption and live PTY continuity require the separate cut.
        let existing_tabs = layout.tabs();
        let replacement = Layout::from_kdl(
            r#"layout {
            session_layer { children; pane name="replacement-chrome"; }
            new_tab_template { pane name="replacement-content"; }
        }"#,
            None,
            None,
            None,
        )
        .unwrap();
        layout.session_layer = replacement.session_layer;
        layout.template = replacement.template;
        assert_eq!(layout.tabs(), existing_tabs);
        let future = layout.new_tab().0;
        assert!(canvas_role_counts(&future).is_empty());
        assert_eq!(future.pane_count(), 2);
    }

    #[test]
    fn semantic_layer_slots_roundtrip_without_sibling_loss() {
        for direction in ["horizontal", "vertical"] {
            for slot in 0..=2 {
                for nested in [false, true] {
                    let mut nodes = vec![
                        r#"pane name="before" size=3"#,
                        r#"pane name="after" size=4"#,
                    ];
                    nodes.insert(slot, "children");
                    let body = format!("{};", nodes.join("; "));
                    let body = if nested {
                        format!(
                            r#"pane name="outer-before"; pane split_direction="{direction}" {{ {body} }}; pane name="outer-after";"#
                        )
                    } else {
                        body
                    };
                    let source = format!(
                        r#"layout {{ session_layer split_direction="{direction}" {{ {body} }}; tab {{ pane name="content"; }}; }}"#
                    );
                    let mut layout = Layout::from_kdl(&source, None, None, None).unwrap();
                    let expected = layout.session_layer.clone();
                    for _ in 0..2 {
                        // Empty capture tests semantic serialization without geometry reconstruction.
                        let (saved, _) = serialize_session_layout(GlobalLayoutManifest {
                            default_layout: Box::new(layout),
                            ..Default::default()
                        })
                        .unwrap();
                        layout = Layout::from_kdl(&saved, None, None, None).unwrap();
                        assert_eq!(
                            layout.session_layer, expected,
                            "slot={slot}, nested={nested}: {saved}"
                        );
                        assert_eq!(
                            layout
                                .session_layer
                                .as_ref()
                                .unwrap()
                                .0
                                .children_block_count(),
                            1
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn inline_and_external_swaps_resolve_once_and_keep_placeholders() {
        let source = r#"layout {
            session_layer { pane name="chrome"; children; }
            tab { pane; }
            swap_tiled_layout name="inline" {
                tab max_panes=5 { pane split_direction="vertical" { pane name="left"; children; pane name="right"; }; }
            }
            swap_floating_layout name="floats" {
                floating_panes { pane x=4 y=6 width=20 height=9 pinned=true focus=true; }
            }
        }"#;
        let external = r#"swap_tiled_layout name="external" {
            tab min_panes=2 { pane stacked=true { children; }; }
        }"#;
        let mut layout =
            Layout::from_kdl(source, None, Some(("swaps.kdl", external)), None).unwrap();
        let floating = layout.swap_floating_layouts.clone();
        for _ in 0..3 {
            assert_eq!(layout.swap_tiled_layouts.len(), 2);
            assert!(
                layout.swap_tiled_layouts[0]
                    .0
                    .contains_key(&LayoutConstraint::MaxPanes(5))
            );
            assert!(
                layout.swap_tiled_layouts[1]
                    .0
                    .contains_key(&LayoutConstraint::MinPanes(2))
            );
            for (variants, _) in &layout.swap_tiled_layouts {
                for tiled in variants.values() {
                    assert_eq!(tiled.canvas_phase, CanvasLayoutPhase::Materialized);
                    assert_eq!(tiled.children_block_count(), 1);
                    assert_eq!(
                        canvas_leaves(tiled)
                            .iter()
                            .filter(|p| p.name.as_deref() == Some("chrome"))
                            .count(),
                        1
                    );
                }
            }
            assert_eq!(layout.swap_floating_layouts, floating);
            let (saved, _) = serialize_session_layout(GlobalLayoutManifest {
                default_layout: Box::new(layout),
                ..Default::default()
            })
            .unwrap();
            layout = Layout::from_kdl(&saved, None, None, None).unwrap();
        }
    }

    #[test]
    fn parser_derived_empty_cwd_roots_and_command_templates_survive() {
        for source in [
            r#"layout { new_tab_template cwd="/tmp/future"; tab { pane; }; }"#,
            r#"layout { tab_template name="empty" {}; swap_tiled_layout name="example" { empty cwd="/tmp/swap"; }; tab { pane; }; }"#,
            r#"layout { new_tab_template; tab { pane; }; }"#,
            r#"layout { new_tab_template { pane command="sleep" { args "600"; }; }; tab { pane; }; }"#,
            r#"layout { swap_tiled_layout { tab { pane command="sleep" { args "600"; }; }; }; tab { pane; }; }"#,
            r#"layout { new_tab_template { pane split_direction="vertical" { pane; pane stacked=true { pane; pane; }; }; }; tab { pane; }; }"#,
        ] {
            let mut layout = Layout::from_kdl(source, None, None, None).unwrap();
            let original_count = layout.new_tab().0.pane_count();
            for _ in 0..2 {
                let (saved, _) = serialize_session_layout(GlobalLayoutManifest {
                    default_layout: Box::new(layout),
                    ..Default::default()
                })
                .unwrap();
                layout = Layout::from_kdl(&saved, None, None, None).unwrap();
                assert_eq!(layout.new_tab().0.pane_count(), original_count, "{saved}");
                if source.contains("/tmp/future") {
                    assert_eq!(
                        layout.new_tab().0.run.as_ref().and_then(Run::get_cwd),
                        Some(PathBuf::from("/tmp/future"))
                    );
                    assert!(layout.template.as_ref().unwrap().0.children.is_empty());
                }
                if source.contains("/tmp/swap") {
                    let swap = layout.swap_tiled_layouts[0].0.values().next().unwrap();
                    assert_eq!(
                        swap.run.as_ref().and_then(Run::get_cwd),
                        Some(PathBuf::from("/tmp/swap"))
                    );
                    assert!(swap.children.is_empty());
                }
                if source.contains("sleep") {
                    let future = if source.contains("swap_tiled_layout") {
                        layout.swap_tiled_layouts[0]
                            .0
                            .values()
                            .next()
                            .unwrap()
                            .clone()
                    } else {
                        layout.new_tab().0
                    };
                    let leaves = canvas_leaves(&future);
                    assert_eq!(leaves.len(), 1);
                    let Some(Run::Command(command)) = &leaves[0].run else {
                        panic!("lost command");
                    };
                    assert_eq!(command.command, PathBuf::from("sleep"));
                    assert_eq!(command.args, vec!["600"]);
                }
            }
        }
    }
}
