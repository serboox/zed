use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow};
use pdfium_render::prelude::*;

/// The release the editor is built against, mirroring `script/fetch-pdfium`. The
/// two have to name the same one: the script fills the cache, and this reads it.
const PDFIUM_RELEASE: &str = "chromium/7999";

/// Where the engine is looked for, in order. The environment comes first so a
/// reader can point at a library of their own without rebuilding; the cache the
/// fetch script fills comes next; the directory beside the editor last, which is
/// where a packaged build carries it.
fn candidate_directories() -> Vec<PathBuf> {
    let mut places = Vec::new();
    if let Ok(named) = std::env::var("ZED_PDFIUM_PATH") {
        places.push(PathBuf::from(named));
    }
    places.push(paths::temp_dir().join("pdfium").join(cached_release_dir()));
    if let Ok(editor) = std::env::current_exe()
        && let Some(beside) = editor.parent()
    {
        places.push(beside.to_path_buf());
    }
    places
}

/// The tag carries a slash, which the script turns into a dash so it names one
/// directory rather than two.
fn cached_release_dir() -> String {
    PDFIUM_RELEASE.replace('/', "-")
}

fn library_file_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "pdfium.dll"
    } else if cfg!(target_os = "macos") {
        "libpdfium.dylib"
    } else {
        "libpdfium.so"
    }
}

/// The engine, bound once. Binding walks the places above and stops at the first
/// library that loads.
pub fn bind() -> Result<Pdfium> {
    let file = library_file_name();
    let mut tried = Vec::new();
    for directory in candidate_directories() {
        let path = directory.join(file);
        if !path.exists() {
            tried.push(path);
            continue;
        }
        match Pdfium::bind_to_library(&path) {
            Ok(bindings) => return Ok(Pdfium::new(bindings)),
            Err(error) => {
                log::warn!("the PDF engine at {} did not load: {error}", path.display());
                tried.push(path);
            }
        }
    }
    // The system's own copy, for a machine that packages one.
    if let Ok(bindings) = Pdfium::bind_to_system_library() {
        return Ok(Pdfium::new(bindings));
    }
    Err(anyhow!(
        "no PDF engine found. Run script/fetch-pdfium to put one in the cache. Looked in: {}",
        tried
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// One page of a document, rendered at a chosen width.
pub struct RenderedPage {
    pub width: u32,
    pub height: u32,
    /// Straight BGRA, which is what the editor's image type takes.
    pub pixels: Vec<u8>,
}

/// Renders `page_index` of the document at `path` so that it is `width` wide.
/// The height follows the page's own proportions.
pub fn render_page(
    engine: &Pdfium,
    path: &Path,
    page_index: PdfPageIndex,
    width: u32,
) -> Result<RenderedPage> {
    let document = engine
        .load_pdf_from_file(path, None)
        .with_context(|| format!("opening {}", path.display()))?;
    let page = document
        .pages()
        .get(page_index)
        .with_context(|| format!("page {page_index} of {}", path.display()))?;

    let config = PdfRenderConfig::new().set_target_width(width as i32);
    let bitmap = page
        .render_with_config(&config)
        .context("rendering the page")?;
    let image = bitmap
        .as_image()
        .context("reading the rendered page")?
        .into_rgba8();
    let (width, height) = (image.width(), image.height());

    // RGBA out of the engine, BGRA into the editor.
    let mut pixels = image.into_raw();
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }

    Ok(RenderedPage {
        width,
        height,
        pixels,
    })
}

/// A page's own size, in the points a PDF is laid out in. Screen positions are
/// turned into these before the engine is asked anything about them.
pub struct PageSize {
    pub width: f32,
    pub height: f32,
}

/// The size of every page, in order.
pub fn page_sizes(engine: &Pdfium, path: &Path) -> Result<Vec<PageSize>> {
    let document = engine
        .load_pdf_from_file(path, None)
        .with_context(|| format!("opening {}", path.display()))?;
    Ok(document
        .pages()
        .iter()
        .map(|page| PageSize {
            width: page.width().value,
            height: page.height().value,
        })
        .collect())
}

/// The text of `page_index` inside the given rectangle, which is in page points
/// with the origin at the bottom left, as PDF lays a page out.
pub fn text_in_rect(
    engine: &Pdfium,
    path: &Path,
    page_index: PdfPageIndex,
    bottom: f32,
    left: f32,
    top: f32,
    right: f32,
) -> Result<String> {
    let document = engine
        .load_pdf_from_file(path, None)
        .with_context(|| format!("opening {}", path.display()))?;
    let page = document
        .pages()
        .get(page_index)
        .with_context(|| format!("page {page_index} of {}", path.display()))?;
    let text = page.text().context("reading the page's text")?;
    Ok(text.inside_rect(PdfRect::new_from_values(bottom, left, top, right)))
}

/// How many pages the document at `path` holds.
pub fn page_count(engine: &Pdfium, path: &Path) -> Result<PdfPageIndex> {
    let document = engine
        .load_pdf_from_file(path, None)
        .with_context(|| format!("opening {}", path.display()))?;
    Ok(document.pages().len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_library_is_named_for_the_platform_it_is_loaded_on() {
        let file = library_file_name();
        assert!(
            file.contains("pdfium"),
            "the engine's library has to be recognisable, got {file}"
        );
        #[cfg(target_os = "linux")]
        assert_eq!(file, "libpdfium.so");
    }

    #[test]
    fn the_cache_directory_is_one_level_deep() {
        let directory = cached_release_dir();
        assert!(
            !directory.contains('/') && !directory.contains('\\'),
            "the release tag must name a single directory, got {directory}"
        );
        assert!(
            directory.contains("chromium"),
            "the directory has to name the release it holds, got {directory}"
        );
    }

    #[test]
    fn the_environment_is_looked_at_before_the_cache() {
        // SAFETY: single-threaded test, and the variable is read back at once.
        unsafe { std::env::set_var("ZED_PDFIUM_PATH", "/somewhere/of/my/own") };
        let places = candidate_directories();
        unsafe { std::env::remove_var("ZED_PDFIUM_PATH") };

        assert_eq!(
            places.first().map(|path| path.display().to_string()),
            Some("/somewhere/of/my/own".to_string()),
            "a library named by the reader has to win over the cached one"
        );
    }
}
