#![cfg(target_os = "linux")]
//! Linux：XDG autostart desktop 文件

use super::*;
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;

fn autostart_file() -> PathBuf {
    let config_dir = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".config"))
                .unwrap_or_default()
        });
    config_dir.join("autostart").join("black-hole-ime.desktop")
}

pub(super) fn set(enabled: bool) -> Result<(), PlatformError> {
    let path = autostart_file();
    if enabled {
        let exe = env::current_exe()
            .map_err(|e| PlatformError::Other(format!("cannot locate current exe: {e}")))?;
        let content = format!(
            "[Desktop Entry]\nType=Application\nName=Black-Hole IME\nComment=Black-Hole IME\nExec={}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
            exe.to_string_lossy()
        );
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                PlatformError::Other(format!("failed to create autostart dir: {e}"))
            })?;
        }
        fs::write(&path, content)
            .map_err(|e| PlatformError::Other(format!("failed to write autostart file: {e}")))?;
        Ok(())
    } else {
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(PlatformError::Other(format!(
                "failed to remove autostart file: {e}"
            ))),
        }
    }
}

pub(super) fn is_set() -> bool {
    autostart_file().exists()
}
