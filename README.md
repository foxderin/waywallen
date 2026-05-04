<p align="center">
  <img src="ui/assets/waywallen-ui.svg" alt="Waywallen" width="128" />
</p>

<h1 align="center">Waywallen</h1>

<p align="center"><strong> Wallpaper Manager for Linux </strong></p>

<a href="README.CN.md">中文 README</a>

---

Waywallen is a dynamic wallpaper solution for Linux desktops.  
It started life as a Wallpaper Engine plugin for KDE.

---

## Screenshots

<p align="center">
  <img src="ui/assets/main_page.png" alt="Waywallen main page" width="720" />
</p>

## Quick Start

### Install

**Prebuilt binaries** — grab the latest archive from the [Releases page](https://github.com/waywallen/waywallen/releases).

**From source** — see [BUILD.md](BUILD.md).

### Desktop integration

| Desktop | Integration |
|---------|-------------|
| **KDE Plasma** | [waywallen-display](https://github.com/waywallen/waywallen-display/) |
| **Niri** | `zwlr_layer_shell_v1` |
| **Sway** | `zwlr_layer_shell_v1` |
| **GNOME** | ️planned |

## Compatibility

| Item | Status |
|------|--------|
| Image wallpapers | ✅ |
| Scene wallpapers | ✅ via [open-wallpaper-engine](https://github.com/waywallen/open-wallpaper-engine) |
| Video wallpapers | ✅ |
| Web wallpapers | ⚠️ planned |

## Configuration

1. Open `System Settings` - `Wallpapers`
2. Change `Wallpaper type` to `Waywallen` and ensure `Display module` is set to `Embedded`
3. Launch `Waywallen` from the `Application Launcher` or run `waywallen` directly
4. Add a `Source`. The default `Wallpaper Engine` data folder is located at `~/.local/share/Steam/steamapps/workshop/content/431960/`
5. You should now be able to use dynamic wallpapers from `Wallpaper Engine`.
