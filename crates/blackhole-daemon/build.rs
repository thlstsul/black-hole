// build.rs - Embed icon resource into Windows executable.
// Used for installer DisplayIcon and taskbar/desktop icons.

use std::path::PathBuf;

fn main() {
    #[cfg(target_os = "windows")]
    {
        let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let ico = manifest_dir.join("../../assets/icons/blackhole.ico");
        let png = manifest_dir.join("../../assets/icons/blackhole.png");

        let icon = if ico.exists() { ico } else { png };

        if icon.exists() {
            let mut res = winresource::WindowsResource::new();
            res.set_icon(icon.to_str().unwrap());
            res.compile().expect("Failed to embed Windows resource");
            println!("cargo:rerun-if-changed={}", icon.display());
        } else {
            println!(
                "cargo:warning=Icon not found: {}, using default",
                icon.display()
            );
        }
    }
}
