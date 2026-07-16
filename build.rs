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
    }
}
