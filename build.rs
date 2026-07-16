fn main() {
    #[cfg(target_os = "macos")]
    {
        println!("cargo:rerun-if-changed=macos/shim/system_tap.m");
        println!("cargo:rerun-if-changed=macos/shim/system_tap.h");
        cc::Build::new()
            .file("macos/shim/system_tap.m")
            .flag("-fobjc-arc")
            .flag("-mmacosx-version-min=11.0")
            .compile("system_tap");
        println!("cargo:rustc-link-lib=framework=CoreAudio");
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
        println!("cargo:rustc-link-lib=framework=Foundation");
        // The shim's @available checks reference ___isPlatformVersionAtLeast
        // from compiler-rt, which rustc does not link by default.
        if let Ok(out) = std::process::Command::new("clang")
            .arg("--print-libgcc-file-name")
            .output()
        {
            let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !path.is_empty() && std::path::Path::new(&path).exists() {
                println!("cargo:rustc-link-arg={path}");
            }
        }
    }
}
