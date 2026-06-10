<h1 align="center">
  <br>
  <img src="https://raw.githubusercontent.com/zellij-org/zellij/main/assets/logo.png" alt="logo" width="200">
  <br>
  VC Frame ⚒ (vibecrafted runtime)
  <br>
  <br>
</h1>

<p align="center">
  <img src="https://raw.githubusercontent.com/zellij-org/zellij/main/assets/demo.gif" alt="demo">
</p>
<h4 align="center">
  [<a href="https://zellij.dev/documentation/installation">Installation</a>]
  [<a href="https://zellij.dev/screencasts/">Screencasts & Tutorials</a>]
  [<a href="https://zellij.dev/documentation/configuration">Configuration</a>]
  [<a href="https://zellij.dev/documentation/layouts">Layouts</a>]
  [<a href="https://zellij.dev/documentation/faq">FAQ</a>]
</h4>
<p align="center">
  <a href="https://discord.gg/CrUAFH3"><img alt="Discord Chat" src="https://img.shields.io/discord/771367133715628073?color=5865F2&label=discord&style=flat-square"></a>
  <a href="https://matrix.to/#/#zellij_general:matrix.org"><img alt="Matrix Chat" src="https://img.shields.io/matrix/zellij_general:matrix.org?color=1d7e64&label=matrix%20chat&style=flat-square&logo=matrix"></a>
  <a href="https://zellij.dev/documentation/"><img alt="VC Frame documentation" src="https://img.shields.io/badge/vc--frame-documentation-fc0060?style=flat-square"></a>
</p>

<br>
    <p align="center">
    <picture>
      <source media="(prefers-color-scheme: dark)" srcset="https://github.com/user-attachments/assets/bc5daac4-140a-4b83-8729-71c944ee1100">
      <img src="https://github.com/user-attachments/assets/55156624-a71a-46b5-939e-f562e3b2dd7f" alt="Sponsored by ">
    </picture>
    &nbsp;
    &nbsp;
    <a href="https://www.gresearch.com/">
        <picture>
          <source media="(prefers-color-scheme: dark)" srcset="https://github.com/user-attachments/assets/d609936a-abf8-4406-8cfc-889f76a09d74">
          <img src="https://github.com/user-attachments/assets/742ae902-fe9d-41c6-baf2-4bc143061da3" alt="gresearch logo">
        </picture>
    </a>
</p>

# What is this?

VC Frame is a vibecrafted runtime and terminal workspace built on the Zellij core. It is aimed at developers, operators, AI-agent workflows, and anyone who lives in the terminal. Similar programs are sometimes called "Terminal Multiplexers".

VC Frame keeps the Zellij philosophy that one must not sacrifice simplicity for power, while adding a fork-owned surface for Vibecrafted operator workflows.

VC Frame is geared toward beginner and power users alike - allowing deep customizability, personal automation through [layouts](https://zellij.dev/documentation/layouts.html), true multiplayer collaboration, unique UX features such as floating and stacked panes, and a [plugin system](https://zellij.dev/documentation/plugins.html) allowing one to create plugins in any language that compiles to WebAssembly.

VC Frame includes a built-in [web-client](https://zellij.dev/tutorials/web-client/), making a terminal optional.

You can get started by building `vc-frame` locally or using the compatibility `zellij` alias.

For more details about our future plans, read about upcoming features in our [roadmap](#roadmap).

## How do I install it?

The canonical local install path is:

```bash
make install
```

This installs `vc-frame` and keeps `zellij` as a compatibility symlink for existing sessions and scripts.

#### Installing from `main`
Installing VC Frame from an arbitrary development branch is not recommended for daily use. Development branches represent pre-release code, are constantly being worked on, and may contain broken or unusable features.

That being said - no-one will stop you from using it (and bug reports involving new features are greatly appreciated), but please consider using the latest release instead as detailed at the top of this section.

## How do I start a development environment?

* Clone the project
* In the project folder, for debug builds run: `cargo xtask run`
* To run all tests: `cargo xtask test`

For more build commands, see [CONTRIBUTING.md](CONTRIBUTING.md).

## Configuration
For configuring VC Frame, please see the [Configuration Documentation](https://zellij.dev/documentation/configuration.html).

## VibeCrafted Shell Layouts
This fork also ships built-in VibeCrafted operator layouts meant to back the
`vibecrafted` flow when repo-owned config is not available:

- `vibecrafted` — operator-first shell surface
- `vc-dashboard` — mission control monitoring grid
- `vc-workflow` — implementation workspace
- `vc-marbles` — convergence workspace
- `vc-research` — synthesis + research swarm workspace

Use them the same way as the stock built-ins, for example:

```bash
vc-frame -l vibecrafted
vc-frame -l vc-dashboard
vc-frame setup --dump-layout vibecrafted
vc-frame setup --dump-layout vc-dashboard
```

They are exposed as first-class built-ins, so they also surface in layout
discovery flows such as the session/layout management UIs instead of behaving
like ad-hoc repo-only files.

The shell-provider layouts resolve mission-control helpers from the standard
home store first, then from a companion repo checkout at
`~/Libraxis/vibecrafted` via `VIBECRAFTED_COMPANION_ROOT`, and finally from
repo-local stores. `vc-dashboard` also acts as a branded control hub for the
native VC Frame surfaces we lean on most: live monitoring, session atlas, layout
forge, configuration control, plugin curation, workspace navigation, sharing,
and the VibeCrafted shell guide.

### Installing repo-owned layouts into `~/.config/zellij/layouts/`

The Vibecrafted framework ships its canonical layouts (`dashboard`, `marbles`,
`operator`, `research`, `workflow`) as real `.kdl` files under
`<vibecrafted-root>/config/zellij/layouts/`. To make them visible to stock
`vc-frame --layout <name>` invocations, run:

```bash
vc-frame setup --install-vibecrafted-layouts
# or with explicit root:
vc-frame setup --install-vibecrafted-layouts --vibecrafted-root /path/to/vibecrafted
```

The installer:

1. **Resolves the framework root dynamically.** Order: `--vibecrafted-root`
   flag → `$VIBECRAFTED_HOME` env (with a `tools/vibecrafted-current` fallback
   for the standard `$HOME/.vibecrafted` user-home convention) → `which
   vibecrafted` canonicalized and walked up until a directory containing
   `config/zellij/layouts/` is found. If none of these succeed, or if the
   resolved path lacks a populated layouts directory, the installer exits
   non-zero with a clear error — silent installs against a wrong path are
   refused.
2. **Enumerates layouts from the live filesystem listing** of
   `<root>/config/zellij/layouts/*.kdl`. There is no hardcoded list in the
   Rust source. Add a `foo.kdl` to the repo, re-run the installer, and
   `~/.config/zellij/layouts/foo.kdl` appears without any code change.
3. **Cleans up stale symlinks.** On every run, symlinks under
   `~/.config/zellij/layouts/` whose target either no longer exists or points
   into the vibecrafted tree are removed. Non-symlink files (your hand-written
   layouts) and symlinks pointing at unrelated frameworks are left alone.
4. **Applies a data-driven alias map.** If
   `<root>/config/zellij/layouts/aliases.txt` exists, each line in the form
   `old=new` installs a compatibility symlink at
   `~/.config/zellij/layouts/<old>` pointing at the current
   `<root>/config/zellij/layouts/<new>` layout file. Lines starting with `#`
   and blank lines are ignored. Aliases whose `<new>` target no longer exists
   are dropped (and any pre-existing broken symlink for `<old>` is removed)
   rather than silently kept as broken links. Edit `aliases.txt`, re-run the
   installer — no rebuild needed.
5. **Prints a summary** listing every symlink created, re-pointed, already
   correct, stale-removed, alias installed, alias dropped, and non-symlink
   file preserved. Re-runs are idempotent — running the installer twice in a
   row produces identical filesystem state and identical summary output.

Example `aliases.txt` mapping the legacy names the Vibecrafted framework
shipped before the canonical rename:

```
# Legacy compatibility map — keeps old layout names working after rename.
vc-dashboard.kdl=dashboard.kdl
vc-marbles.kdl=marbles.kdl
vc-research.kdl=research.kdl
vc-workflow.kdl=workflow.kdl
implement-dual.kdl=workflow.kdl
research-grid.kdl=research.kdl
vibecraft.kdl=operator.kdl
vibecrafted.kdl=operator.kdl
```

## About issues in this repository
Issues in this repository, whether open or closed, do not necessarily indicate a problem or a bug in the software. They only indicate that the reporter wanted to communicate their experiences or thoughts to the maintainers. The Zellij maintainers do their best to go over and reply to all issue reports, but unfortunately cannot promise these will always be dealt with or even read. Your understanding is appreciated.

## Roadmap
Presented here is the project roadmap, divided into three main sections.

These are issues that are either being actively worked on or are planned for the near future.

***If you'll click on the image, you'll be led to an SVG version of it on the website where you can directly click on every issue***

[![roadmap](https://github.com/user-attachments/assets/bb55d213-4a68-4c84-ae72-7db5c9bf94fb)](https://zellij.dev/roadmap)

## Origin of the Name
[From Wikipedia, the free encyclopedia](https://en.wikipedia.org/wiki/Zellij)

Zellij (Arabic: الزليج, romanized: zillīj; also spelled zillij or zellige) is a style of mosaic tilework made from individually hand-chiseled tile pieces. The pieces were typically of different colours and fitted together to form various patterns on the basis of tessellations, most notably elaborate Islamic geometric motifs such as radiating star patterns composed of various polygons. This form of Islamic art is one of the main characteristics of architecture in the western Islamic world. It is found in the architecture of Morocco, the architecture of Algeria, early Islamic sites in Tunisia, and in the historic monuments of al-Andalus (in the Iberian Peninsula).

## License

MIT

## Sponsored by
<a href="https://terminaltrove.com/"><img src="https://avatars.githubusercontent.com/u/121595180?s=200&v=4" width="80px"></a>
