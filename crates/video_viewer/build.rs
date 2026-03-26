(fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
        if let Ok(gst_root) = std::env::var("GSTREAMER_1_0_ROOT_MSVC_X86_64") {
            println!("cargo:rustc-link-search=native={}/lib", gst_root);
        }
    }
}
