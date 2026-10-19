fn main() {
    // Embeds resources/app.rc (icon id 1 + version info) when present.
    let rc = std::path::Path::new("resources/app.rc");
    println!("cargo:rerun-if-changed=resources/app.rc");
    println!("cargo:rerun-if-changed=resources/app.ico");
    if rc.exists() {
        embed_resource::compile(rc, embed_resource::NONE)
            .manifest_optional()
            .expect("embedding resources");
    }
}
