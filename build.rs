fn main() {
    // Embed the app icon (and version info) into the Windows executable.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("assets/sizematters.rc", embed_resource::NONE).manifest_optional().ok();
    }
    println!("cargo:rerun-if-changed=assets/sizematters.rc");
    println!("cargo:rerun-if-changed=assets/icon.ico");
}
