use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

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
    places_to_look(std::env::var("ZED_PDFIUM_PATH").ok())
}

/// The same, with the reader's own choice handed in rather than read from the
/// environment: a test that set the variable would be setting it for whatever
/// else is running beside it in the same process.
fn places_to_look(named: Option<String>) -> Vec<PathBuf> {
    let mut places = Vec::new();
    if let Some(named) = named {
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

/// The engine this process reads PDFs through.
///
/// The library binds itself into a place of its own, once for the whole process:
/// a second binding fails, whatever it was asked to bind to. Every errand used to
/// bind for itself, so the first one won and the rest were told there was no
/// engine -- which is why a document opened with its page count read and not one
/// of its pages drawn. It is bound here once and lent out, and since the library
/// is not one that can be called from two threads at a time, the calls it is lent
/// for go through the crate's own lock.
pub fn engine() -> Result<&'static Pdfium> {
    static ENGINE: OnceLock<Mutex<Option<&'static Pdfium>>> = OnceLock::new();
    let held = ENGINE.get_or_init(|| Mutex::new(None));
    let mut held = held
        .lock()
        .map_err(|_| anyhow!("the PDF engine was left locked by a thread that failed"))?;
    if let Some(engine) = *held {
        return Ok(engine);
    }
    // Kept for as long as the process runs, which is what the library does with
    // itself anyway; a failure leaves nothing behind, so a library put in place
    // later is still found.
    let bound: &'static Pdfium = Box::leak(Box::new(bind()?));
    *held = Some(bound);
    Ok(bound)
}

/// Binds the library. Walks the places above and stops at the first that loads.
fn bind() -> Result<Pdfium> {
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
    quarter_turns: u8,
) -> Result<RenderedPage> {
    let document = engine
        .load_pdf_from_file(path, None)
        .with_context(|| format!("opening {}", path.display()))?;
    let page = document
        .pages()
        .get(page_index)
        .with_context(|| format!("page {page_index} of {}", path.display()))?;

    let config = PdfRenderConfig::new()
        .set_target_width(width as i32)
        .rotate(rotation_of(quarter_turns), true);
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

/// How far a page is turned, counted in quarter turns clockwise.
fn rotation_of(quarter_turns: u8) -> PdfPageRenderRotation {
    match quarter_turns % 4 {
        1 => PdfPageRenderRotation::Degrees90,
        2 => PdfPageRenderRotation::Degrees180,
        3 => PdfPageRenderRotation::Degrees270,
        _ => PdfPageRenderRotation::None,
    }
}

/// The whole text of a page, for copying or searching.
pub fn page_text(engine: &Pdfium, path: &Path, page_index: PdfPageIndex) -> Result<String> {
    let document = engine
        .load_pdf_from_file(path, None)
        .with_context(|| format!("opening {}", path.display()))?;
    let page = document
        .pages()
        .get(page_index)
        .with_context(|| format!("page {page_index} of {}", path.display()))?;
    Ok(page.text().context("reading the page's text")?.all())
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

/// What a document says about itself, for the reader who asks.
pub struct Facts {
    pub pages: usize,
    pub title: Option<String>,
    pub author: Option<String>,
    pub producer: Option<String>,
    pub created: Option<String>,
    pub first_page: Option<PageSize>,
}

pub fn facts(engine: &Pdfium, path: &Path) -> Result<Facts> {
    let document = engine
        .load_pdf_from_file(path, None)
        .with_context(|| format!("opening {}", path.display()))?;
    let metadata = document.metadata();
    let tag = |which: PdfDocumentMetadataTagType| {
        metadata
            .get(which)
            .map(|tag| tag.value().to_string())
            .filter(|value| !value.trim().is_empty())
    };
    Ok(Facts {
        pages: document.pages().len() as usize,
        title: tag(PdfDocumentMetadataTagType::Title),
        author: tag(PdfDocumentMetadataTagType::Author),
        producer: tag(PdfDocumentMetadataTagType::Producer),
        created: tag(PdfDocumentMetadataTagType::CreationDate),
        first_page: document.pages().first().ok().map(|page| PageSize {
            width: page.width().value,
            height: page.height().value,
        }),
    })
}

/// One line of the document's own table of contents.
pub struct OutlineEntry {
    pub title: String,
    pub page: usize,
    pub depth: usize,
}

/// The document's table of contents, flattened, each line carrying how deep it
/// sits so the reader can see the shape of it. A line that leads nowhere -- some
/// documents carry those -- is left out, since there would be nothing to do with
/// it.
pub fn outline(engine: &Pdfium, path: &Path) -> Result<Vec<OutlineEntry>> {
    let document = engine
        .load_pdf_from_file(path, None)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut lines = Vec::new();
    for bookmark in document.bookmarks().iter() {
        let Some(title) = bookmark.title().filter(|title| !title.trim().is_empty()) else {
            continue;
        };
        let Some(page) = bookmark
            .destination()
            .and_then(|destination| destination.page_index().ok())
        else {
            continue;
        };
        let mut depth = 0;
        let mut above = bookmark.parent();
        while let Some(parent) = above {
            depth += 1;
            if depth > 8 {
                break;
            }
            above = parent.parent();
        }
        lines.push(OutlineEntry {
            title,
            page: page as usize,
            depth,
        });
    }
    Ok(lines)
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
    fn a_page_turns_a_full_circle_and_comes_back() {
        // Four quarter turns is where it started, and a fifth is the first again:
        // the count comes from a button pressed any number of times.
        assert_eq!(rotation_of(0), rotation_of(4));
        assert_eq!(rotation_of(1), rotation_of(5));
        assert_ne!(rotation_of(1), rotation_of(3));
    }

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

    /// The library is bound into a place of its own, once for the whole process.
    /// Binding it a second time fails, so what everything reads through has to be
    /// the same instance -- which is what this pins. With a library to hand it
    /// also draws a page, since "bound" and "able to draw" are not the same
    /// claim.
    #[test]
    fn the_engine_is_lent_out_rather_than_bound_again() {
        let library_at_hand = std::env::var("ZED_PDFIUM_PATH").is_ok()
            || candidate_directories()
                .iter()
                .any(|directory| directory.join(library_file_name()).exists());

        let first = engine();
        let second = engine();

        if !library_at_hand {
            // Nothing to lend. Then both answers are errors rather than panics,
            // and neither leaves anything behind that would stop a library put in
            // place later from being found.
            assert!(first.is_err() && second.is_err());
            return;
        }

        let first = first.expect("a library was there to bind");
        let second = second.expect("the second asking gets the same engine");
        assert!(
            std::ptr::eq(first, second),
            "each asking bound the library again, and every one after the first \
             is told the library is already bound -- which reads as no engine"
        );

        // A document made here rather than read from the tree: what is being
        // checked is that the engine works when it is lent out, not what any
        // particular file holds.
        let made = first
            .create_new_pdf()
            .expect("the engine can make a document");
        let bytes = {
            let mut document = made;
            document
                .pages_mut()
                .create_page_at_index(PdfPagePaperSize::a4(), 0)
                .expect("a page can be added");
            document.save_to_bytes().expect("the document can be saved")
        };
        let written = std::env::temp_dir().join("zed-pdf-engine-test.pdf");
        std::fs::write(&written, bytes).expect("the document can be written");

        assert_eq!(
            page_count(first, &written).expect("the pages can be counted"),
            1
        );
        let drawn = render_page(first, &written, 0, 200, 0).expect("the page draws");
        assert_eq!(drawn.width, 200);
        assert!(
            drawn.height > 200,
            "an A4 page is taller than it is wide: {} by {}",
            drawn.width,
            drawn.height
        );
        assert_eq!(
            drawn.pixels.len(),
            (drawn.width * drawn.height * 4) as usize,
            "four bytes to a pixel, which is what the editor's image type takes"
        );
        std::fs::remove_file(&written).ok();
    }

    #[test]
    fn the_reader_s_own_library_is_looked_at_before_the_cache() {
        let places = places_to_look(Some("/somewhere/of/my/own".to_string()));

        assert_eq!(
            places.first().map(|path| path.display().to_string()),
            Some("/somewhere/of/my/own".to_string()),
            "a library named by the reader has to win over the cached one"
        );
        assert!(
            places.len() > 1,
            "the cache is still looked at after it, or a reader who names a \
             library that has gone is left with none"
        );
    }
}
