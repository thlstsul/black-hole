# Blackhole IME

跨平台中文输入法，基于 Rust 构建，支持 Windows（TSF）和 Linux（IBus）。

## 特性

- **拼音 & 双拼** — 内置全拼和多种双拼方案，支持模糊音
- **高性能引擎** — 基于音节图的解码器，整句输入与词语组合混合检索
- **用户词典** — 自动学习用户输入习惯，越用越准
- **跨平台 UI** — 基于 egui 的现代候选窗口，支持亮色/暗色主题，DWM 圆角
- **系统托盘** — Windows 系统托盘快速切换方案、打开设置
- **NSIS 安装包** — Windows 一键安装，自动注册 IME；卸载时自动清理注册表
- **Linux 打包** — deb + AppImage 格式，支持 IBus 自动注册

## 项目结构

```
crates/
├── blackhole-shared/    # 共享数据类型：按键事件、候选词、UI 命令、设置
├── blackhole-engine/    # 输入法引擎：拼音/双拼编码、词典、解码器、排序器
├── blackhole-platform/  # 平台适配层：Windows TSF / Linux IBus
├── blackhole-ui/        # UI 层：候选窗口、系统托盘、设置面板
└── blackhole-daemon/    # 守护进程：组装各模块，协调引擎-平台-UI 线程通信

assets/
├── dicts/               # 词库文件（Rime 格式）
└── icons/               # 应用图标

platforms/
├── windows/             # NSIS 安装模板、图标生成脚本
└── linux/               # IBus 组件 XML 模板

Makefile.toml             # cargo-make 构建与打包任务（跨平台）
```

## 技术栈

| 层 | 技术 |
|---|---|
| 语言 | Rust 2024 edition (MSRV 1.85) |
| 引擎 | 音节图解码、SQLite 词库、N-gram 语言模型 |
| Windows 平台 | TSF (Text Services Framework), COM |
| Linux 平台 | IBus, DBus (zbus) |
| UI | egui + eframe (候选窗口), tray-icon (系统托盘) |
| 打包 | cargo-packager + NSIS / deb / AppImage, cargo-make |

## 构建

### 环境要求

- Rust 1.85+
- [cargo-make](https://github.com/sagiegurari/cargo-make) — 构建与打包任务管理
- Windows: NSIS (makensis) 用于打包
- Linux: IBus 开发库

### 开发构建

```bash
# 安装 cargo-make
cargo install cargo-make --locked

# 构建 daemon + platform
cargo make build-daemon build-platform

# 或直接使用 cargo
cargo build -p blackhole-daemon -p blackhole-platform
```

### 发布构建

```bash
cargo build --release -p blackhole-daemon -p blackhole-platform
```

### 打包

```bash
# 一键打包（自动检测平台，首次自动安装 cargo-packager）
cargo make pack

# 或指定平台
cargo make pack-win      # Windows → NSIS 安装包 (.exe)
cargo make pack-linux    # Linux   → deb + AppImage

# 清理
cargo make clean
```

打包产物输出到 `target/release/`：
- Windows: `blackhole_0.1.0_x64-setup.exe`
- Linux: `blackhole-ime_0.1.0_amd64.deb`, `blackhole-ime_0.1.0_amd64.AppImage`

打包配置统一在 `crates/blackhole-daemon/Cargo.toml` 的 `[package.metadata.packager]` 中。

## 安装

### Windows

1. 运行安装包 `blackhole_0.1.0_x64-setup.exe`
2. 以管理员身份安装，安装程序会自动注册 IME 组件
3. 在系统输入法列表中选择 "Blackhole IME"

### Linux

```bash
# deb 包
sudo dpkg -i blackhole-ime_0.1.0_amd64.deb

# 或 AppImage
chmod +x blackhole-ime_0.1.0_amd64.AppImage
./blackhole-ime_0.1.0_amd64.AppImage
```

安装后重启 IBus (`ibus restart`)，在输入法列表中选择 "Blackhole"。

### 卸载

**Windows**：通过开始菜单或控制面板卸载，卸载程序会自动：
- 取消注册 IME 组件 (`regsvr32 /u`)
- 清理残留注册表项（CTF TIP、CLSID）
- 删除安装目录

**Linux**：
```bash
sudo dpkg -r blackhole-ime
```

## 输入方案

| 方案 | 说明 |
|---|---|
| 全拼 (Pinyin) | 标准汉语拼音输入 |
| 双拼 (Shuangpin) | 多种双拼方案（微软双拼、小鹤双拼等） |

使用 `Ctrl+Shift+F12` 切换方案，或通过系统托盘菜单切换。

## 快捷键

| 快捷键 | 功能 |
|---|---|
| 空格 | 上屏首选候选 |
| 数字键 1-9 | 选择对应候选 |
| ↑ / ↓ | 切换候选 |
| Esc | 清除输入 |
| Ctrl+Shift+F12 | 切换输入方案 |

## 许可

MIT License
