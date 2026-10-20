use std::path::Path;

/// Gives the process a fontconfig that knows two bundled fonts and names no
/// family for any generic one, which is what leaves the engine's own font
/// database pointing `serif`, `monospace` and `sans-serif` at fonts that are
/// not there. It has to be in place before the engine starts: the font database
/// is read once, then.
pub fn use_only_the_bundled_fonts(workspace: &Path) {
    let fonts = workspace.join("fonts");
    std::fs::create_dir_all(&fonts).expect("a directory for the fonts");
    let bundled = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/fonts");
    for font in [
        "ibm-plex-sans/IBMPlexSans-Regular.ttf",
        "lilex/Lilex-Regular.ttf",
    ] {
        let source = bundled.join(font);
        let file_name = source.file_name().expect("a font has a file name");
        std::fs::copy(&source, fonts.join(file_name))
            .unwrap_or_else(|error| panic!("copying {}: {error}", source.display()));
    }

    let configuration = workspace.join("fonts.conf");
    std::fs::write(
        &configuration,
        format!(
            "<?xml version=\"1.0\"?><fontconfig><dir>{}</dir></fontconfig>",
            fonts.display()
        ),
    )
    .expect("a fontconfig file");
    // SAFETY: nothing else in this test binary reads or writes the environment,
    // and the engine, which does read it, has not been started yet.
    unsafe { std::env::set_var("FONTCONFIG_FILE", &configuration) };
}
