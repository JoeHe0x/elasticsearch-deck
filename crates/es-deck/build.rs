//! Embeds the application icon into the Windows executable.

fn main() {
    #[cfg(windows)]
    embed_resource::compile("assets/icon.rc", embed_resource::NONE)
        .manifest_optional()
        .unwrap();
}
