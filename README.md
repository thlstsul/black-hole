# 黑洞输入法

跨平台中文输入法，基于 Rust 构建，支持 Windows（TSF）和 Linux（IBus）。

## 特性

- **拼音 & 双拼** — 内置全拼和多种双拼方案（小鹤、微软、搜狗等），支持模糊音（zh/z、n/l 等）、简拼扩展与常见拼写纠错
- **高性能引擎** — 基于音节图的维特比解码器，整句输入与词语组合混合检索，Unigram + Bigram 语言模型评分
- **用户词典** — 基于 Rime UserDb（`.userdb.txt`，与 librime 互通），自动学习用户输入习惯，越用越准
- **跨平台 UI** — 基于 egui / eframe (wgpu) 的现代候选窗口，支持亮色/暗色/跟随系统主题，Windows DWM 圆角
- **中英文切换** — 轻按 Ctrl 快速切换中英（松开时生效，不与其他键组合）；Windows 上还可左键点击语言栏按钮切换；切换时未上屏内容直接上屏保留
- **TSF 语言栏按钮** — Windows 输入法指示器旁的专属按钮，右键菜单快速切换方案、主题、打开设置、退出，中英状态跨进程同步
- **设置面板** — 主题、默认方案、候选窗参数、按键绑定（热更新实时生效）、开机自启动
- **NSIS 安装包** — Windows 一键安装，自动注册 IME；卸载时自动清理注册表
- **Linux 打包** — deb + AppImage 格式，支持 IBus 自动注册

## 项目结构

```
crates/
├── black-hole-shared/    # 共享数据类型：按键事件、候选词、引擎/UI 命令、设置、按键绑定
├── black-hole-engine/    # 输入法引擎：拼音/双拼方案、音节图、解码器、语言模型、用户词典（含 CLI 调试工具 black-hole-cli）
├── black-hole-platform/  # 平台适配层：Windows TSF / Linux IBus、IPC、开机自启动
├── black-hole-ui/        # UI 层：候选窗口、设置面板
├── black-hole-daemon/    # 守护进程：组装各模块，协调引擎-平台-UI 线程通信
└── icon-gen/             # 从 SVG 生成多尺寸 ICO 图标和托盘 PNG

assets/
├── dicts/                # 词库文件（Rime 格式，rime-ice git 子模块）
└── icons/                # 应用图标

platforms/
├── windows/              # NSIS 安装模板、IME 注册/反注册脚本
└── linux/                # IBus 组件 XML 模板

Makefile.toml             # cargo-make 构建与打包任务（跨平台）
```

## 技术栈

| 层 | 技术 |
|---|---|
| 语言 | Rust 2024 edition (MSRV 1.85) |
| 引擎 | 音节图解码（维特比）、rime-dict 词库（`.dict.yaml` → 二进制缓存）、Unigram/Bigram 语言模型、UserDb 用户词典 |
| Windows 平台 | TSF (Text Services Framework), COM, Win32 |
| Linux 平台 | IBus, DBus (zbus) |
| UI | egui / eframe + wgpu + winit (候选窗口) |
| 打包 | cargo-packager + NSIS / deb / AppImage, cargo-make |

## 构建

### 环境要求

- Rust 1.85+
- [cargo-make](https://github.com/sagiegurari/cargo-make) — 构建与打包任务管理
- 词库子模块：克隆时使用 `git clone --recurse-submodules`（`assets/dicts` 为 rime-ice 子模块）
- Windows: NSIS (makensis) 用于打包（cargo-packager 也可自动安装）
- Linux: IBus 开发库

### 开发构建

```bash
# 安装 cargo-make
cargo install cargo-make --locked

# 构建 daemon + platform（一键任务）
cargo make build-all

# 或直接使用 cargo
cargo build -p black-hole-daemon -p black-hole-platform
```

引擎自带命令行调试工具：

```bash
cargo run -p black-hole-engine -- --help
```

### 发布构建

```bash
cargo build --release -p black-hole-daemon -p black-hole-platform
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

打包产物输出到 `target/release/`（NSIS 安装包为 `.exe`，Linux 为 `.deb` / `.AppImage`，具体文件名由 cargo-packager 根据 `[package.metadata.packager]` 生成）。

打包配置统一在 `crates/black-hole-daemon/Cargo.toml` 的 `[package.metadata.packager]` 中。

## 安装

### Windows

1. 运行安装包（NSIS 安装程序）
2. 以管理员身份安装，安装程序会自动注册 IME 组件
3. 在系统输入法列表中选择 "Black-Hole IME"

### Linux

```bash
# deb 包
sudo dpkg -i black-hole-ime_*.deb

# 或 AppImage
chmod +x black-hole-ime_*.AppImage
./black-hole-ime_*.AppImage
```

安装后重启 IBus (`ibus restart`)，在输入法列表中选择 Black-Hole。

### 卸载

**Windows**：通过开始菜单或控制面板卸载，卸载程序会自动：
- 取消注册 IME 组件 (`regsvr32 /u`)
- 清理残留注册表项（CTF TIP、CLSID）
- 删除安装目录

**Linux**：
```bash
sudo dpkg -r black-hole-ime
```

## 输入方案

| 方案 | 说明 |
|---|---|
| 全拼 (Pinyin) | 标准汉语拼音输入，支持简拼、模糊音与纠错 |
| 双拼 (Shuangpin) | 多种双拼方案（小鹤、微软、搜狗等） |

使用 `Ctrl+Shift+F12` 切换方案，或通过 Windows 语言栏按钮右键菜单切换。

## 快捷键

| 快捷键 | 功能 |
|---|---|
| 空格 | 上屏首选候选 |
| 数字键 1-9 | 选择对应候选 |
| ↑ / ↓ | 切换候选 |
| Esc | 清除输入 |
| Ctrl（按下后单独松开） | 切换中英文输入模式 |
| Ctrl+Shift+F12 | 切换输入方案 |

> 按键绑定可在设置面板中自定义，保存后实时生效（无需重启）。

## 许可

Apache License 2.0
