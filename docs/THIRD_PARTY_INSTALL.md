# Upstream Zellij Packages

* [Packages](#packages)
    * [Arch Linux](#arch-linux)
    * [MacOS](#macos)
    * [Fedora Linux](#fedora-linux)
    * [Void Linux](#void-linux)

This page lists third-party packages for upstream `zellij`. They are useful
when you want stock Zellij compatibility, but they do **not** install the
VetCoders `vc-frame` runtime, its grayscale default surface, or its left
session rail.

For vc-frame itself:

- Source-build preview: `make install`
- Release-grade path: [RELEASE.md](RELEASE.md)
- Product surface: [VC_FRAME_OPERATOR_SURFACE.md](VC_FRAME_OPERATOR_SURFACE.md)

## Packages

 :warning: **These packages are not affiliated with the Zellij maintainers, are provided here for convenience, and should not be presented as vc-frame distribution channels.**

[![Packaging status](https://repology.org/badge/vertical-allrepos/zellij.svg)](https://repology.org/project/zellij/versions)

### Arch Linux
You can install the `zellij` package from the [official extra repository](https://archlinux.org/packages/extra/x86_64/zellij/):

```
pacman -S zellij
```

Or install from AUR repository with [AUR Helper](https://wiki.archlinux.org/title/AUR_helpers):

```
paru -S zellij-git
```

### Fedora Linux
You can install the `zellij` package from the [COPR](https://copr.fedorainfracloud.org/coprs/varlad/zellij/)

```
sudo dnf copr enable varlad/zellij 
sudo dnf install zellij
```

### MacOS
You can install `zellij` with [Homebrew on MacOS](https://formulae.brew.sh/formula/zellij):

```
brew install zellij
```

Or install with [MacPorts](https://ports.macports.org/port/zellij/details/):

```
sudo port install zellij
```

### Void Linux

```
sudo xbps-install zellij
```
