//! Session utilities are not content.
//!
//! `compact-bar` / `tab-bar` / `status-bar` are chrome: one instance, owned by
//! the session layer. `vc-tab-title` and `link` are background workers. A
//! second `AddPlugin` for either class is how the content area fills up with
//! pinned frames titled `vc-frame:*`.

use zellij_utils::input::layout::Run;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UtilityPaneKind {
    /// The real instance is the session-layer row. A floating or extra copy is not.
    Chrome,
    /// Loaded once, suppressed. Never a visible pane.
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UtilitySpawn {
    /// No runtime yet. Load it suppressed; do not focus it.
    LoadSuppressed,
    /// A runtime already exists. Do not create a pane.
    Reuse,
    /// The caller asked for a visible content pane. That pane must not exist.
    RefuseVisible,
}

/// Leaf identity: `vc-frame:compact-bar`, `zellij:compact-bar`, and `compact-bar`
/// are the same utility.
pub fn utility_location_key(location: &str) -> String {
    location
        .rsplit([':', '/'])
        .next()
        .unwrap_or(location)
        .trim_end_matches(".wasm")
        .to_owned()
}

pub fn utility_pane_kind(location: &str) -> Option<UtilityPaneKind> {
    match utility_location_key(location).as_str() {
        "compact-bar" | "tab-bar" | "status-bar" => Some(UtilityPaneKind::Chrome),
        "vc-tab-title" | "link" => Some(UtilityPaneKind::Background),
        _ => None,
    }
}

/// Frame title when a utility pane is visible at all. Never the raw URL.
pub fn utility_pane_title(location: &str) -> Option<&'static str> {
    match utility_location_key(location).as_str() {
        "compact-bar" => Some("Tab bar"),
        "tab-bar" => Some("Tab bar"),
        "status-bar" => Some("Status bar"),
        "vc-tab-title" => Some("Tab titles"),
        "link" => Some("Links"),
        _ => None,
    }
}

pub fn utility_kind_of_invoked(run: &Option<Run>) -> Option<UtilityPaneKind> {
    let Run::Plugin(plugin) = run.as_ref()? else {
        return None;
    };
    utility_pane_kind(&plugin.location_string())
}

pub fn utility_key_of_invoked(run: &Option<Run>) -> Option<String> {
    let Run::Plugin(plugin) = run.as_ref()? else {
        return None;
    };
    utility_pane_kind(&plugin.location_string())
        .map(|_| utility_location_key(&plugin.location_string()))
}

/// `requested_visible` is the caller's placement, not a permission.
/// Background workers stay suppressed either way. Chrome that is already
/// running is reused. Chrome that is not running is not invented as a
/// content pane — the session layer owns that row.
pub fn decide_utility_spawn(
    kind: UtilityPaneKind,
    already_loaded: bool,
    requested_visible: bool,
) -> UtilitySpawn {
    match (kind, already_loaded, requested_visible) {
        (UtilityPaneKind::Background, false, _) => UtilitySpawn::LoadSuppressed,
        (UtilityPaneKind::Background, true, _) => UtilitySpawn::Reuse,
        (UtilityPaneKind::Chrome, true, _) => UtilitySpawn::Reuse,
        (UtilityPaneKind::Chrome, false, true) => UtilitySpawn::RefuseVisible,
        (UtilityPaneKind::Chrome, false, false) => UtilitySpawn::LoadSuppressed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spellings_of_one_utility_collapse_to_one_key() {
        assert_eq!(utility_location_key("vc-frame:compact-bar"), "compact-bar");
        assert_eq!(utility_location_key("zellij:compact-bar"), "compact-bar");
        assert_eq!(utility_location_key("compact-bar"), "compact-bar");
        assert_eq!(
            utility_location_key("vc-frame:vc-tab-title"),
            "vc-tab-title"
        );
        assert_eq!(utility_location_key("vc-frame:link"), "link");
    }

    #[test]
    fn a_second_background_spawn_reuses_the_existing_runtime() {
        assert_eq!(
            decide_utility_spawn(UtilityPaneKind::Background, true, true),
            UtilitySpawn::Reuse
        );
        assert_eq!(
            decide_utility_spawn(UtilityPaneKind::Background, false, true),
            UtilitySpawn::LoadSuppressed
        );
    }

    #[test]
    fn a_visible_compact_bar_request_does_not_invent_a_content_pane() {
        assert_eq!(
            decide_utility_spawn(UtilityPaneKind::Chrome, false, true),
            UtilitySpawn::RefuseVisible
        );
        assert_eq!(
            decide_utility_spawn(UtilityPaneKind::Chrome, true, true),
            UtilitySpawn::Reuse
        );
    }

    #[test]
    fn visible_titles_are_human() {
        assert_eq!(
            utility_pane_title("vc-frame:vc-tab-title"),
            Some("Tab titles")
        );
        assert_eq!(utility_pane_title("vc-frame:link"), Some("Links"));
        assert_eq!(utility_pane_title("vc-frame:compact-bar"), Some("Tab bar"));
        assert_eq!(utility_pane_title("vc-frame:strider"), None);
    }
}
