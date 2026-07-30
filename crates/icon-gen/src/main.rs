//! icon-gen - 从 SVG 生成多尺寸 ICO 图标和 tray PNG
//!
//! 完全替代原有的 PowerShell 脚本 (platforms/windows/generate_icon.ps1)，
//! 使用 resvg 库渲染 SVG，纯 Rust 实现 ICO 编码。
//!
//! 用法:
//!   cargo run -p icon-gen [SVG路径] [ICO输出路径]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// 需要生成的图标尺寸
const SIZES: [u16; 5] = [256, 64, 48, 32, 16];

fn main() {
    let repo_root =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into()))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();

    let args: Vec<String> = std::env::args().collect();
    let svg_path = args
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root.join("assets/icons/black-hole.svg"));
    let ico_path = args
        .get(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root.join("assets/icons/black-hole.ico"));

    if let Err(e) = run(&svg_path, &ico_path) {
        eprintln!("[ERROR] {e}");
        std::process::exit(1);
    }
}

fn run(svg_path: &Path, ico_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("[ICON] Generating icons from SVG...");

    if !svg_path.exists() {
        return Err(format!("SVG not found: {}", svg_path.display()).into());
    }

    let svg_data = fs::read(svg_path)?;

    // --- 加载并解析 SVG ---
    let opt = resvg::usvg::Options::default();
    let tree = resvg::usvg::Tree::from_data(&svg_data, &opt)?;
    eprintln!(
        "  SVG loaded: {}x{}",
        tree.size().width(),
        tree.size().height()
    );

    // --- 渲染各尺寸 PNG ---
    let mut png_entries: Vec<(u16, Vec<u8>)> = Vec::new();

    for &size in &SIZES {
        let width = size as u32;
        let height = size as u32;

        let scale_x = width as f32 / tree.size().width() as f32;
        let scale_y = height as f32 / tree.size().height() as f32;
        let ts = resvg::tiny_skia::Transform::from_scale(scale_x, scale_y);

        let mut pixmap =
            resvg::tiny_skia::Pixmap::new(width, height).ok_or("Failed to create pixmap")?;

        resvg::render(&tree, ts, &mut pixmap.as_mut());

        let png_data = pixmap.encode_png()?;
        png_entries.push((size, png_data));
        eprintln!("  Rendered {size}x{size}");
    }

    // --- 合并为 ICO ---
    let ico_dir = ico_path.parent().unwrap();
    if !ico_dir.exists() {
        fs::create_dir_all(ico_dir)?;
    }

    write_ico(&png_entries, ico_path)?;
    eprintln!("  [OK] ICO: {}", ico_path.display());

    // --- 提取 32x32 为 tray_icon.png ---
    if let Some((_, data)) = png_entries.iter().find(|(s, _)| *s == 32) {
        let tray_path = ico_dir.join("tray_icon.png");
        fs::write(&tray_path, data)?;
        eprintln!("  [+] tray_icon.png (32x32)");
    }

    eprintln!("[OK] Icon generation complete");
    Ok(())
}

/// 将多个 PNG 数据合并为一个 ICO 文件
fn write_ico(entries: &[(u16, Vec<u8>)], path: &Path) -> std::io::Result<()> {
    let count = entries.len() as u16;
    let mut buf: Vec<u8> = Vec::new();

    // --- ICO 文件头 (6 bytes) ---
    buf.write_all(&0u16.to_le_bytes())?; // reserved
    buf.write_all(&1u16.to_le_bytes())?; // type: 1 = ICO
    buf.write_all(&count.to_le_bytes())?; // image count

    // --- 目录项 (16 bytes each) ---
    let mut data_offset = (6 + (count as usize) * 16) as u32;

    // 先收集所有条目信息，再写数据
    struct DirEntry {
        width: u8,
        height: u8,
        data_size: u32,
        data_offset: u32,
    }

    let mut dir_entries = Vec::new();

    for (size, png_data) in entries {
        let w = if *size >= 256 { 0 } else { *size as u8 };
        let h = if *size >= 256 { 0 } else { *size as u8 };

        dir_entries.push(DirEntry {
            width: w,
            height: h,
            data_size: png_data.len() as u32,
            data_offset,
        });

        data_offset += png_data.len() as u32;
    }

    // 写入目录项
    for entry in &dir_entries {
        buf.write_all(&[entry.width])?; // width (0 = 256)
        buf.write_all(&[entry.height])?; // height (0 = 256)
        buf.write_all(&[0u8])?; // color palette
        buf.write_all(&[0u8])?; // reserved
        buf.write_all(&1u16.to_le_bytes())?; // color planes
        buf.write_all(&32u16.to_le_bytes())?; // bits per pixel
        buf.write_all(&entry.data_size.to_le_bytes())?;
        buf.write_all(&entry.data_offset.to_le_bytes())?;
    }

    // --- 写入图像数据 ---
    for (_, png_data) in entries {
        buf.write_all(png_data)?;
    }

    fs::write(path, &buf)
}
