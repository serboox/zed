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

/// Does something with the engine this process reads PDFs through, with nothing
/// else doing anything with it at the same time.
///
/// Two things about the library make this the only way to reach it. It binds
/// itself into a place of its own, once for the whole process: a second binding
/// fails whatever it is asked to bind to, so an errand that binds for itself gets
/// an engine only if it is the first, and every other one is told there is none.
/// And it must not be called from two threads at once -- the crate's
/// `thread_safe` feature only says the engine may be *held* by several threads,
/// leaving the serialising to whoever holds it, and pages are drawn on a pool of
/// them. So it is bound once, kept, and lent out one caller at a time.
pub fn with_engine<R>(errand: impl FnOnce(&Pdfium) -> Result<R>) -> Result<R> {
    static ENGINE: OnceLock<Mutex<Option<&'static Pdfium>>> = OnceLock::new();
    let held = ENGINE.get_or_init(|| Mutex::new(None));
    let mut held = held
        .lock()
        .map_err(|_| anyhow!("the PDF engine was left locked by a thread that failed"))?;
    let engine = match *held {
        Some(engine) => engine,
        None => {
            // Kept for as long as the process runs, which is what the library
            // does with itself anyway; a failure leaves nothing behind, so a
            // library put in place later is still found.
            let bound: &'static Pdfium = Box::leak(Box::new(bind()?));
            *held = Some(bound);
            bound
        }
    };
    // The lock is held for the whole errand, not merely for the lending: the
    // document, its pages and everything read off them are the library's own
    // state, and another thread in there at the same time gives wrong answers or
    // brings the process down.
    errand(engine)
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
    path: &Path,
    page_index: PdfPageIndex,
    width: u32,
    quarter_turns: u8,
) -> Result<RenderedPage> {
    with_engine(|engine| {
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
pub fn page_text(path: &Path, page_index: PdfPageIndex) -> Result<String> {
    with_engine(|engine| {
        let document = engine
            .load_pdf_from_file(path, None)
            .with_context(|| format!("opening {}", path.display()))?;
        let page = document
            .pages()
            .get(page_index)
            .with_context(|| format!("page {page_index} of {}", path.display()))?;
        Ok(page.text().context("reading the page's text")?.all())
    })
}

/// A page's own size, in the points a PDF is laid out in. Screen positions are
/// turned into these before the engine is asked anything about them.
pub struct PageSize {
    pub width: f32,
    pub height: f32,
}

/// The size of every page, in order.
pub fn page_sizes(path: &Path) -> Result<Vec<PageSize>> {
    with_engine(|engine| {
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
    })
}

/// The text of `page_index` inside the given rectangle, which is in page points
/// with the origin at the bottom left, as PDF lays a page out.
pub fn text_in_rect(
    path: &Path,
    page_index: PdfPageIndex,
    bottom: f32,
    left: f32,
    top: f32,
    right: f32,
) -> Result<String> {
    with_engine(|engine| {
        let document = engine
            .load_pdf_from_file(path, None)
            .with_context(|| format!("opening {}", path.display()))?;
        let page = document
            .pages()
            .get(page_index)
            .with_context(|| format!("page {page_index} of {}", path.display()))?;
        let text = page.text().context("reading the page's text")?;
        Ok(text.inside_rect(PdfRect::new_from_values(bottom, left, top, right)))
    })
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

pub fn facts(path: &Path) -> Result<Facts> {
    with_engine(|engine| {
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
pub fn outline(path: &Path) -> Result<Vec<OutlineEntry>> {
    with_engine(|engine| {
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
    })
}

/// How many pages the document at `path` holds.
pub fn page_count(path: &Path) -> Result<PdfPageIndex> {
    with_engine(|engine| {
        let document = engine
            .load_pdf_from_file(path, None)
            .with_context(|| format!("opening {}", path.display()))?;
        Ok(document.pages().len())
    })
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

    /// The library is bound into a place of its own, once for the whole process,
    /// and it must not be called from two threads at a time. Both are pinned
    /// here: with a library to hand, pages are drawn from several threads at
    /// once, which is how the editor draws them. Without the lock this does not
    /// fail an assertion -- it brings the process down, which is what it did.
    #[test]
    fn the_engine_serves_one_caller_at_a_time() {
        let library_at_hand = std::env::var("ZED_PDFIUM_PATH").is_ok()
            || candidate_directories()
                .iter()
                .any(|directory| directory.join(library_file_name()).exists());

        if !library_at_hand {
            // Nothing to lend. Then the answer is an error rather than a panic,
            // and asking again is still allowed, so a library put in place later
            // is found.
            assert!(with_engine(|_| Ok(())).is_err());
            assert!(with_engine(|_| Ok(())).is_err());
            return;
        }

        // A document of our own making rather than one from the tree: what is
        // being checked is the engine under several callers, not what any
        // particular file holds.
        let written = std::env::temp_dir().join("zed-pdf-engine-test.pdf");
        let bytes = with_engine(|engine| {
            let mut document = engine.create_new_pdf()?;
            document
                .pages_mut()
                .create_page_at_index(PdfPagePaperSize::a4(), 0)?;
            Ok(document.save_to_bytes()?)
        })
        .expect("the engine can make a document");
        std::fs::write(&written, bytes).expect("the document can be written");

        assert_eq!(page_count(&written).expect("the pages are counted"), 1);

        // Several threads, each asking for something different, over and over:
        // what brings the library down is two of its calls overlapping, and one
        // round of eight would have to be unlucky to catch it.
        let went_well: Vec<bool> = std::thread::scope(|threads| {
            (0..12)
                .map(|at| {
                    let written = written.clone();
                    threads.spawn(move || {
                        for round in 0..15 {
                            let asked_for = 80 + ((at * 15 + round) % 40) * 5;
                            let drawn = match render_page(&written, 0, asked_for, 0) {
                                Ok(drawn) => drawn,
                                Err(_) => return false,
                            };
                            if drawn.width != asked_for
                                || drawn.pixels.len()
                                    != (drawn.width * drawn.height * 4) as usize
                            {
                                return false;
                            }
                            if page_count(&written).is_err()
                                || page_sizes(&written).is_err()
                                || page_text(&written, 0).is_err()
                                || facts(&written).is_err()
                            {
                                return false;
                            }
                        }
                        true
                    })
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|thread| thread.join().expect("no thread was lost"))
                .collect()
        });

        assert!(
            went_well.iter().all(|went_well| *went_well),
            "the engine gave a wrong or failed answer while several threads were \
             in it at once"
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
