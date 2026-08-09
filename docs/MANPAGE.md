NAME
====

**vc-frame** - run vc-frame

DESCRIPTION
===========

vc-frame is a vibecrafted runtime and terminal workspace aimed at developers,
operators, AI-agent workflows, and anyone who loves the terminal. At its core,
it is a terminal multiplexer (similar to tmux and screen), but this is merely
its infrastructure layer.

vc-frame includes a layout system, and a plugin system allowing one to create
plugins in any language that compiles to WebAssembly.

To list currently running sessions run: `vc-frame list-sessions`
To attach to a currently running session run: `vc-frame attach [session-name]`

OPTIONS
=======

Run `vc-frame --help` to see available flags and subcommands.

CONFIGURATION
=============

vc-frame looks for configuration file in the following order:

1. the file provided with _--config_
2. under the path provided in the *VC_FRAME_CONFIG_FILE* environment variable
3. the default location (see FILES section)
4. the system location

Run `vc-frame setup --check` in order to see possible issues with the
configuration.

LAYOUTS
=======

Layouts are **KDL** files which vc-frame can load on startup when the _--layout_
flag is provided. YAML layout/config conversion (`convert-config`,
`convert-layout`, `convert-theme`) has been removed — KDL is the only supported
configuration format.

By default vc-frame will load a layout called `default` (file `default.kdl`),
but this can be changed with the `default_layout "name"` configuration option.


For example a file like this:
```
layout {
    pane split_direction="vertical" {
        pane
        pane split_direction="horizontal" {
            pane
            pane
        }
    }
}
```

will tell vc-frame to create this layout:
```
┌─────┬─────┐
│     │     │
├─────┤     │
│     │     │
└─────┴─────┘
```

CREATING LAYOUTS
----------------

A layout file is a nested tree of `pane` nodes. Each node describes either a
terminal pane (leaf), a split container, or a plugin pane.

Common attributes:
* __split_direction="horizontal|vertical"__ — how children are laid out.
* __size=\<n\>__ — fixed size in rows/columns, or a share of the parent.
* __plugin location="…"__ — load a compiled vc-frame plugin into the pane
  (see PLUGINS). Built-in plugins use the `vc-frame:` / short-name form.
* __command="…" / args "…"__ — run a command in the pane instead of a shell.
* __borderless true|false__ — hide pane frames for chrome rails.

Full layout syntax is documented upstream at
https://zellij.dev/documentation/layouts.html (KDL) and in the in-repo layout
fixtures under `zellij-utils/assets/layouts/`.

KEYBINDINGS
===========

vc-frame comes with a default set of keybindings which aims to fit as many users
as possible but that behaviour can be overridden or modified in user
configuration files. The information about bindings is available in the
_keybinds_ section of configuration. For example, to introduce a keybinding that
will create a new tab and go to tab 1 after pressing 'c' one can write:

```
keybinds {
    normal {
        bind "c" { NewTab; GoToTab 1; }
    }
}
```

where "normal" stands for a mode name (see MODES section), the bind body lists
the actions to be executed by vc-frame (see ACTIONS section), and the bind key
is the key or key combination. 

The default keybinds can be unbound either for a specific mode, or for every mode.
It supports either a list of `keybinds`, or a bool indicating that every keybind
should be unbound:

```
keybinds:
    unbind: true
```
Will unbind every default binding.

```
keybinds:
    unbind: [ Ctrl: 'p']
```
Will unbind every default `^P` binding for each mode.
```
keybinds:
    normal:
        - unbind: true
```
Will unbind every default keybind for the `normal` mode.
```
keybinds:
    normal:
        - unbind: [ Alt: 'n', Ctrl: 'g']
```
Will unbind every default keybind for `n` and `^g` for the `normal` mode.

ACTIONS
-------

* __Quit__ - quits vc-frame
* __SwitchToMode: <InputMode\>__ - switches to the specified input mode. See
  MODES section for possible values.
* __Resize: <Direction\>__ - resizes focused pane in the specified direction
  (one of: Left, Right, Up, Down).
* __FocusNextPane__ - switches focus to the next pane to the right or below if
  on  screen edge.
* __FocusPreviousPane__ - switches focus to the next pane to the left or above
  if on  screen edge.
* __SwitchFocus__ - left for legacy support. Switches focus to a pane with the
  next ID.
* __MoveFocus: <Direction\>__ -  moves focus in the specified direction (Left,
  Right, Up, Down).
* __Clear__ - clears current screen.
* __DumpScreen: [File\] [--pane-id <ID\>]__ - dumps the pane content to a file or STDOUT.
  If a file path is provided, writes the content to that file. If omitted, prints the content to STDOUT.
  If --pane-id is provided, dumps the specified pane; otherwise dumps the focused pane.
  <ID\> can be a bare integer (eg. 1), a terminal pane id (eg. terminal_1) or a plugin pane id (eg. plugin_1).
  A bare integer is equivalent to a terminal pane id with the same number.
* __DumpLayout: <File\>__ - dumps the screen in the specified or default file.
* __EditScrollback__ - replaces the current pane with the scrollback buffer.
* __ScrollUp__ - scrolls up 1 line in the focused pane.
* __ScrollDown__ - scrolls down 1 line in the focused pane.
* __PageScrollUp__ - scrolls up 1 page in the focused pane.
* __PageScrollDown__ - scrolls down 1 page in the focused pane.
* __ToggleFocusFullscreen__ - toggles between fullscreen focus pane and normal
  layout.
* __NewPane: <Direction\>__ - opens a new pane in the specified direction (Left,
  Right, Up, Down) relative to focus. 
* __CloseFocus__ - closes focused pane.
* __NewTab__ - creates a new tab.
* __GoToNextTab__ - goes to the next tab.
* __GoToPreviousTab__ - goes to previous tab.
* __CloseTab__ - closes current tab.
* __GoToTab: <Index\>__ - goes to the tab with the specified index number.
* __Detach__ - detach session and exit.
* __ToggleActiveSyncTab__ - toggle between sending text commands to all panes
  on the current tab and normal mode.
* __UndoRenameTab__ - undoes the changed tab name and reverts to the previous name.
* __UndoRenamePane__ - undoes the changed pane name and reverts to the previous name.
* __SetPaneColor__ - sets the default foreground and/or background color of a pane.


KEYS
----

* __Char: <character\>__ - a single character with no modifier.
* __Alt: <character\>__ - a single character with `Alt` key as modifier.
* __Ctrl: <character\>__ - a single character with `Ctrl` key as modifier.
* __F: <1-12\>__ - one of `F` keys (usually at the top of the keyboard).
* __Backspace__
* __Left / Right / Up / Down__ - arrow keys on the keyboard.
* __Home__
* __End__
* __PageUp / PageDown__
* __BackTab__ - a backward Tab key.
* __Delete__
* __Insert__
* __Esc__


MODES
-----

* __normal__ - the default startup mode of vc-frame. Provides the ability to
  switch to different modes, as well as some quick navigation shortcuts.
* __locked__ - disables all keybindings except the one that would switch the
  mode to normal (_ctrl-g_ by default). Useful when vc-frame's keybindings
  conflict with those of a chosen terminal app. 
* __tmux__ - provides convenience keybindings emulating simple tmux behaviour
* __pane__ - includes instructions that manipulate the panes (adding new panes,
  moving, closing).
* __tab__ - includes instructions that manipulate the tabs (adding new tabs,
  moving, closing).
* __resize__ - allows resizing of the focused pane.
* __scroll__ - allows scrolling within the focused pane.
* __renametab__ - is a "hidden" mode that can be passed to _SwitchToMode_
  action. It will trigger renaming of a tab.
* __renamepane__ - is a "hidden" mode that can be passed to _SwitchToMode_
  action. It will trigger renaming of a pane.
* __session__ - allows detaching from a session.


Theme
=====
A color theme can be defined either in truecolor, 256 or hex color format.
Truecolor:
```
fg: [0, 0, 0]
```
256:
```
fg: 0
```
Hex color:
```
fg: "#000000"
bg: "#000"
```
The color theme can be specified in the following way:
```
themes:
  default:
    fg: [0,0,0]
    bg: [0,0,0]
    black: [0,0,0]
    red: [0,0,0]
    green: [0,0,0]
    yellow: [0,0,0]
    blue: [0,0,0]
    magenta: [0,0,0]
    cyan: [0,0,0]
    white: [0,0,0]
    orange: [0,0,0]
```

If the theme is called `default`, then vc-frame will pick it on startup.
To specify a different theme, run vc-frame with:
```
vc-frame options --theme [NAME]
```
or put the name in the configuration file with `theme: [NAME]`.

PLUGINS
=======

vc-frame has a plugin system based on WebAssembly. Any language that can run on
WASI can be used to develop a plugin. To load a plugin include it in a layout
file. vc-frame comes with default plugins included: _status-bar_, _strider_,
_tab-bar_.

FILES
=====

Default user configuration directory location:
* Linux: _$XDG_CONFIG_HOME/vc-frame /home/alice/.config/vc-frame_
* macOS: _/Users/Alice/Library/Application Support/io.vetcoders.vc-frame_

Default user layout directory location:
* Subdirectory called `layouts` inside of the configuration directory.
* Linux: _$XDG_CONFIG_HOME/vc-frame/layouts /home/alice/.config/vc-frame/layouts_
* macOS: _/Users/Alice/Library/Application Support/io.vetcoders.vc-frame/layouts_

Default plugin directory location:
* Linux: _$XDG_DATA_HOME/vc-frame/plugins /home/alice/.local/share/vc-frame/plugins_
* macOS: _/Users/Alice/Library/Application Support/io.vetcoders.vc-frame/plugins_

Legacy Zellij directories are migrated automatically on first run and used
only as a fallback source:
* Linux: _$XDG_CONFIG_HOME/zellij /home/alice/.config/zellij_
* macOS: _/Users/Alice/Library/Application Support/com.Zellij-Contributors.zellij_


ENVIRONMENT
===========
VC_FRAME_CONFIG_FILE
  Path of vc-frame config to load.
VC_FRAME_CONFIG_DIR
  Path of the vc-frame config directory.



NOTES
=====

The manpage is meant to provide concise offline reference. For more detailed
instructions please visit: 

https://zellij.dev/documentation
