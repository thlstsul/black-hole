use std::path::PathBuf;

// ---------------------------------------------------------------------------
// IBus component XML template
// ---------------------------------------------------------------------------

const IBUS_COMPONENT_TEMPLATE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<component>
  <name>org.freedesktop.IBus.Black-Hole</name>
  <description>Black-Hole IME</description>
  <exec>{exec_path} --ibus</exec>
  <version>0.1.0</version>
  <author>Black-Hole IME Team</author>
  <license>MIT</license>
  <homepage>https://github.com/black-hole/ime</homepage>
  <textdomain>black-hole</textdomain>
  <engines>
    <engine>
      <name>black-hole</name>
      <language>zh</language>
      <license>MIT</license>
      <author>Black-Hole IME Team</author>
      <icon>/usr/share/ibus-black-hole/icons/black-hole.svg</icon>
      <layout>us</layout>
      <longname>Black-Hole</longname>
      <description>Black-Hole IME with Pinyin and Shuangpin support</description>
      <rank>99</rank>
      <symbol>CJ</symbol>
    </engine>
  </engines>
</component>
"#;

// ---------------------------------------------------------------------------
// Registration check
// ---------------------------------------------------------------------------

/// 检查 IBus 组件 XML 是否已部署到用户配置目录。
pub fn is_registered() -> bool {
    ibus_component_path().exists()
}

// ---------------------------------------------------------------------------
// Auto registration
// ---------------------------------------------------------------------------

/// 自动生成并部署 IBus 组件 XML 文件。
///
/// 将 XML 写入 `~/.config/ibus/component/black-hole.xml`，
/// 其中 `<exec>` 路径使用当前可执行文件的实际路径。
/// 写入成功后尝试执行 `ibus restart` 使其生效。
///
/// # Errors
///
/// - 无法获取当前可执行路径时返回错误
/// - 无法创建目录或写入文件时返回错误
pub fn register_ime() -> std::io::Result<()> {
    let component_dir = ibus_component_dir();
    std::fs::create_dir_all(&component_dir)?;

    let exec_path = std::env::current_exe()?.to_string_lossy().to_string();

    let xml_content = IBUS_COMPONENT_TEMPLATE.replace("{exec_path}", &exec_path);
    let xml_path = component_dir.join("black-hole.xml");
    std::fs::write(&xml_path, xml_content)?;

    tracing::info!("IBus component XML written to: {}", xml_path.display());

    // 尝试重启 ibus 使新组件生效
    match std::process::Command::new("ibus").arg("restart").output() {
        Ok(output) if output.status.success() => {
            tracing::info!("ibus restart succeeded");
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!("ibus restart failed: {}", stderr);
        }
        Err(e) => {
            tracing::warn!("Failed to run ibus restart: {}", e);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ibus_component_dir() -> PathBuf {
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."));
    home.join(".config/ibus/component")
}

fn ibus_component_path() -> PathBuf {
    ibus_component_dir().join("black-hole.xml")
}
