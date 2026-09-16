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
    build_the_go_template_parser();
}

/// Compile the Go template parser that lives under `vendor/`.
///
/// The crate published beside that grammar pins `tree-sitter` 0.19, whose
/// `Language` is a different type from the 0.26 this workspace uses, so the
/// parser is built here and declared against the workspace's own.
fn build_the_go_template_parser() {
    let vendor = Path::new("../../vendor/tree-sitter-gotmpl/src");
    println!("cargo:rerun-if-changed={}", vendor.display());
    cc::Build::new()
        .include(vendor)
        .file(vendor.join("parser.c"))
        // The generated parser is not written to any project's warning
        // settings, and its warnings are not ours to act on.
        .warnings(false)
        .compile("tree_sitter_gotmpl");
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
