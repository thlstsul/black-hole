// build.rs - Embed icon resource into the DLL for Windows language bar display.
//
// Windows TSF calls RegisterProfile with the DLL path as icon_file
// and icon_index=0, extracting the first icon from the DLL's resource section.

use std::env;
use std::path::PathBuf;

fn main() {
    #[cfg(target_os = "windows")]
    {
        // Suppress LNK4104: link.exe wants the OLE-only exports
        // (DllGetClassObject / DllCanUnloadNow / DllRegisterServer /
        // DllUnregisterServer) marked PRIVATE, but they are consumed via
        // GetProcAddress by COM/regsvr32, so the warning is harmless.
        println!("cargo::rustc-cdylib-link-arg=/IGNORE:4104");

        let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
        let ico = manifest_dir.join("../../assets/icons/black-hole.ico");

        if ico.exists() {
            let mut res = winresource::WindowsResource::new();
            res.set_icon(ico.to_str().unwrap());
            res.compile()
                .expect("Failed to embed icon into DLL resource");
            println!("cargo:rerun-if-changed={}", ico.display());
        } else {
            println!(
                "cargo:warning=Icon not found: {}, language bar will show default icon",
                ico.display()
            );
        }
    }
}
