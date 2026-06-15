# Strider Operator Artifacts Mode

Strider defaults to its generic file browser behavior: all entries, current
directory only, existing alphabetical ordering.

For the Vibecrafted operator artifacts pane, enable the opt-in artifacts mode:

```kdl
plugin location="strider" {
    pane_title "Artifacts"
    mode "artifacts"
    file_filter "*.md"
    sort_by "modified_desc"
}
```

In this mode Strider shows a flat recursive view of `*.md` files under the
resolved artifacts directory, sorted by file modification time newest first.
The recursive view is intentional because Vibecrafted artifacts are date-nested
under the artifacts root.

The operator layout may still set its pane `cwd` to the portable artifacts
expression. If that value reaches Strider unexpanded or invalid, artifacts mode
uses the Vibecrafted fallback order:

1. `$VIBECRAFTED_HOME/artifacts`
2. `$HOME/.vibecrafted/artifacts`
