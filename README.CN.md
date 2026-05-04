<p align="center">
  <img src="ui/assets/waywallen-ui.svg" alt="Waywallen" width="128" />
</p>

<h1 align="center">Waywallen</h1>

<p align="center"><strong> Wallpaper Manager for Linux </strong></p>

<a href="README.md">English README</a>

---

Waywallen 是一个为 Linux 桌面打造的动态壁纸方案  
最初是 wallpaper engine plugin for kde  

---

## 界面

<p align="center">
  <img src="ui/assets/main_page.png" alt="Waywallen 主界面" width="720" />
</p>

## 快速开始

### 安装

**预编译包** —— 到 [Releases 页面](https://github.com/waywallen/waywallen/releases) 下载最新版本。

**从源码构建** —— 见 [BUILD.md](BUILD.md)。

### 桌面集成

| 桌面 | 集成 |
|------|------|
| **KDE Plasma** | [waywallen-display](https://github.com/waywallen/waywallen-display/) |
| **Niri** | zwlr_layer_shell_v1 |
| **Sway** | zwlr_layer_shell_v1 |
| **GNOME** | 规划中 |

## 兼容性

| 项目 | 现状 |
|------|------|
| 图片壁纸 | ✅ |
| 场景壁纸 | ✅ [open-wallpaper-engine](https://github.com/waywallen/open-wallpaper-engine) |
| 视频壁纸 | ✅ |
| 网页壁纸 | ⚠️ 规划中 |

## 配置
1. 打开 `System Settings` - `wallpaper`
2. 修改 `Wallpaper type` 为 `Waywallen`,并确保 `Display module` 为 `Embedded`
3. 从 `Application Launcher` 启动 `Waywallen` 或直接运行 `waywallen`
4. 添加 `Source`，默认的 `Wallpaper Engine` 数据文件夹位于 `~/.local/share/Steam/steamapps/workshop/content/431960/`
5. 现在应当可以使用来自 `Wallpaper Engine` 的动态壁纸了。