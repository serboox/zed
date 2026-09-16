use std::path::Path;

/// Tell cargo to rebuild when the embedded language files change.
///
/// `rust_embed` reads `src/` at compile time, and cargo tracks only the Rust
/// sources it can see. Adding a language directory therefore changed nothing
/// cargo knew about: the crate was not rebuilt, the new language was missing
/// from the embedded set, and the editor panicked at startup asking for a
/// config that was not there.
fn main() {
    watch(Path::new("src"));
}

fn watch(directory: &Path) {
    println!("cargo:rerun-if-changed={}", directory.display());
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.path().is_dir() {
            watch(&entry.path());
        }
    }
}
