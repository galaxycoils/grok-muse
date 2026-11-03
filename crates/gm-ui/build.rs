fn main() {
    #[cfg(target_os = "macos")]
    {
        println!("cargo:rerun-if-changed=src/mac_ui.m");
        cc::Build::new()
            .file("src/mac_ui.m")
            .flag("-fobjc-arc")
            .compile("gm_mac_ui");

        println!("cargo:rustc-link-lib=framework=AppKit");
        println!("cargo:rustc-link-lib=framework=Foundation");
    }
}
