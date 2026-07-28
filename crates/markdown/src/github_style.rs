use std::sync::Arc;

use gpui::{
    App, BorderStyle, EdgesRefinement, FontFallbacks, FontStyle, FontWeight, HighlightStyle, Hsla,
    StyleRefinement, TextStyleRefinement, UnderlineStyle, Window, rgb, rgba,
};
use settings::Settings as _;
use theme::{Appearance, SyntaxTheme};
use theme_settings::ThemeSettings;
use ui::prelude::*;

use crate::{BlockQuoteKindColors, HeadingLevelStyles, MarkdownStyle};

/// Colors verified against `github-markdown-css` (sindresorhus/github-markdown-css) for the
/// page/body values, and against the Primer color scale used by `primer/github-vscode-theme`
/// for the syntax token values.
struct GithubPalette {
    page_background: Hsla,
    text: Hsla,
    muted_text: Hsla,
    link: Hsla,
    border: Hsla,
    code_block_background: Hsla,
    inline_code_background: Hsla,
    keyword: Hsla,
    string: Hsla,
    comment: Hsla,
    function: Hsla,
    constant: Hsla,
    type_name: Hsla,
    tag: Hsla,
    property: Hsla,
    variable: Hsla,
    boolean: Hsla,
    deleted: Hsla,
    inserted: Hsla,
}

fn hex(value: u32) -> Hsla {
    rgb(value).into()
}

fn hex_alpha(value: u32) -> Hsla {
    rgba(value).into()
}

fn light_palette() -> GithubPalette {
    GithubPalette {
        page_background: hex(0xffffff),
        text: hex(0x1f2328),
        muted_text: hex(0x6e7781),
        link: hex(0x0969da),
        border: hex(0xd1d9e0),
        code_block_background: hex(0xf6f8fa),
        inline_code_background: hex_alpha(0x818b981f),
        keyword: hex(0xcf222e),
        string: hex(0x0a3069),
        comment: hex(0x6e7781),
        function: hex(0x8250df),
        constant: hex(0x0550ae),
        type_name: hex(0x953800),
        tag: hex(0x116329),
        property: hex(0x0550ae),
        variable: hex(0x953800),
        boolean: hex(0x0550ae),
        deleted: hex(0xa40e26),
        inserted: hex(0x116329),
    }
}

fn dark_palette() -> GithubPalette {
    GithubPalette {
        page_background: hex(0x0d1117),
        text: hex(0xf0f6fc),
        muted_text: hex(0xb1bac4),
        link: hex(0x4493f8),
        border: hex(0x3d444d),
        code_block_background: hex(0x151b23),
        inline_code_background: hex_alpha(0x656c7633),
        keyword: hex(0xff7b72),
        string: hex(0xa5d6ff),
        comment: hex(0xb1bac4),
        function: hex(0xd2a8ff),
        constant: hex(0x79c0ff),
        type_name: hex(0xffa657),
        tag: hex(0x7ee787),
        property: hex(0x79c0ff),
        variable: hex(0xffa657),
        boolean: hex(0x79c0ff),
        deleted: hex(0xffc1ba),
        inserted: hex(0x7ee787),
    }
}

fn palette_for(appearance: Appearance) -> GithubPalette {
    if appearance.is_light() {
        light_palette()
    } else {
        dark_palette()
    }
}

/// The page background GitHub renders a markdown document on, independent of the active
/// editor theme. Used by the markdown preview's outer container.
pub fn github_page_background(appearance: Appearance) -> Hsla {
    palette_for(appearance).page_background
}

fn github_syntax_theme(palette: &GithubPalette) -> Arc<SyntaxTheme> {
    let solid = |color: Hsla| HighlightStyle {
        color: Some(color),
        ..Default::default()
    };
    let italic = |color: Hsla| HighlightStyle {
        color: Some(color),
        font_style: Some(FontStyle::Italic),
        ..Default::default()
    };
    let bold = |color: Hsla| HighlightStyle {
        color: Some(color),
        font_weight: Some(FontWeight::BOLD),
        ..Default::default()
    };

    Arc::new(SyntaxTheme::new([
        ("attribute".to_string(), solid(palette.property)),
        ("boolean".to_string(), solid(palette.boolean)),
        ("comment".to_string(), italic(palette.comment)),
        ("comment.doc".to_string(), italic(palette.comment)),
        ("constant".to_string(), solid(palette.constant)),
        ("constructor".to_string(), solid(palette.function)),
        ("diff.minus".to_string(), solid(palette.deleted)),
        ("diff.plus".to_string(), solid(palette.inserted)),
        ("embedded".to_string(), solid(palette.text)),
        ("emphasis".to_string(), italic(palette.text)),
        ("emphasis.strong".to_string(), bold(palette.text)),
        ("enum".to_string(), solid(palette.type_name)),
        ("function".to_string(), solid(palette.function)),
        ("hint".to_string(), italic(palette.muted_text)),
        ("keyword".to_string(), solid(palette.keyword)),
        ("label".to_string(), solid(palette.property)),
        ("link_text".to_string(), solid(palette.link)),
        ("link_uri".to_string(), solid(palette.link)),
        ("namespace".to_string(), solid(palette.type_name)),
        ("number".to_string(), solid(palette.constant)),
        ("operator".to_string(), solid(palette.text)),
        ("predictive".to_string(), italic(palette.muted_text)),
        ("preproc".to_string(), solid(palette.function)),
        ("primary".to_string(), solid(palette.text)),
        ("property".to_string(), solid(palette.property)),
        ("punctuation".to_string(), solid(palette.text)),
        ("punctuation.bracket".to_string(), solid(palette.text)),
        ("punctuation.delimiter".to_string(), solid(palette.text)),
        (
            "punctuation.list_marker".to_string(),
            solid(palette.muted_text),
        ),
        ("punctuation.markup".to_string(), solid(palette.muted_text)),
        ("punctuation.special".to_string(), solid(palette.property)),
        ("selector".to_string(), solid(palette.type_name)),
        ("selector.pseudo".to_string(), solid(palette.function)),
        ("string".to_string(), solid(palette.string)),
        ("string.escape".to_string(), solid(palette.type_name)),
        ("string.regex".to_string(), solid(palette.string)),
        ("string.special".to_string(), solid(palette.type_name)),
        (
            "string.special.symbol".to_string(),
            solid(palette.type_name),
        ),
        ("tag".to_string(), solid(palette.tag)),
        ("text.literal".to_string(), solid(palette.string)),
        ("title".to_string(), bold(palette.text)),
        ("type".to_string(), solid(palette.type_name)),
        ("variable".to_string(), solid(palette.variable)),
        ("variable.special".to_string(), solid(palette.constant)),
        ("variant".to_string(), solid(palette.function)),
    ]))
}

fn font_fallbacks(names: &[&str]) -> FontFallbacks {
    FontFallbacks::from_fonts(names.iter().map(|name| name.to_string()).collect())
}

/// Builds a [`MarkdownStyle`] that renders like a GitHub-flavored markdown document: fixed
/// colors, fonts and syntax highlighting drawn entirely from GitHub's own rendering (plus a
/// few glamour-inspired structural touches for blockquotes and list markers), independent of
/// the active editor theme. The only thing read from the active theme is whether it is broadly
/// light or dark, via [`theme::ActiveTheme::appearance`].
pub fn github_style(window: &Window, cx: &App) -> MarkdownStyle {
    github_style_for_appearance(cx.theme().appearance(), window, cx)
}

/// Like [`github_style`], but rendered for an explicit appearance rather than
/// the active theme's. The palette is fixed, so light-or-dark is the only thing
/// left to choose -- which is what lets a reader keep a light document open in a
/// dark editor.
pub fn github_style_for_appearance(
    appearance: Appearance,
    window: &Window,
    cx: &App,
) -> MarkdownStyle {
    let palette = palette_for(appearance);
    let theme_settings = ThemeSettings::get_global(cx);

    let buffer_font_size = theme_settings.buffer_font_size(cx);
    let body_font_size = theme_settings.markdown_preview_font_size(cx);

    // GitHub's system-font stacks aren't installed as such on most Linux desktops, so the
    // primary family is picked from fonts actually present here, with GitHub's real stack
    // kept as fallbacks for platforms (macOS/Windows) where those names do resolve.
    let body_fallbacks = font_fallbacks(&[
        "-apple-system",
        "BlinkMacSystemFont",
        "Segoe UI",
        "Helvetica",
        "Arial",
        "sans-serif",
    ]);
    let code_fallbacks = font_fallbacks(&[
        "ui-monospace",
        "SFMono-Regular",
        "SF Mono",
        "Menlo",
        "Consolas",
        "Liberation Mono",
        "monospace",
    ]);

    // "Noto Sans" (GitHub's own mid-stack fallback) isn't installed as a plain
    // Latin family on most Linux desktops -- only CJK/Arabic/etc. variants
    // are -- so hardcoding it here left GPUI silently substituting some other
    // font. `markdown_preview_font_family()` resolves to whatever Zed's own
    // preview already uses (falling back to the guaranteed-loaded UI font),
    // so this always renders in a real font instead of an unpredictable
    // substitute. GitHub's real stack stays as fallbacks for platforms where
    // those names do resolve (macOS/Windows).
    let mut base_text_style = window.text_style();
    base_text_style.refine(&TextStyleRefinement {
        font_family: Some(theme_settings.markdown_preview_font_family().clone()),
        font_fallbacks: Some(body_fallbacks),
        font_features: Some(Default::default()),
        font_size: Some(rems(1.0).into()),
        line_height: Some((body_font_size * 1.5).into()),
        color: Some(palette.text),
        ..Default::default()
    });

    let heading_style = |size_ratio: f32| TextStyleRefinement {
        font_size: Some(rems(size_ratio).into()),
        font_weight: Some(FontWeight::SEMIBOLD),
        color: Some(palette.text),
        ..Default::default()
    };

    MarkdownStyle {
        base_text_style,
        selection_background_color: palette.link.opacity(0.25),
        rule_color: palette.border,
        block_quote_border_color: palette.border,
        secondary_border_color: palette.border,
        muted_panel_background: palette.code_block_background,
        muted_text_color: palette.muted_text,
        code_surface_color: palette.code_block_background,
        strong_text_color: palette.text,
        table_header_background: palette.page_background,
        block_quote_kind_colors: BlockQuoteKindColors {
            note: palette.link,
            tip: palette.inserted,
            important: palette.function,
            warning: palette.type_name,
            caution: palette.keyword,
        },
        code_block_overflow_x_scroll: true,
        code_block: StyleRefinement {
            padding: EdgesRefinement {
                top: Some(px(16.).into()),
                left: Some(px(16.).into()),
                right: Some(px(16.).into()),
                bottom: Some(px(16.).into()),
            },
            margin: EdgesRefinement {
                top: Some(px(16.).into()),
                left: Some(px(0.).into()),
                right: Some(px(0.).into()),
                bottom: Some(px(16.).into()),
            },
            border_style: Some(BorderStyle::Solid),
            border_widths: EdgesRefinement {
                top: Some(px(1.).into()),
                left: Some(px(1.).into()),
                right: Some(px(1.).into()),
                bottom: Some(px(1.).into()),
            },
            border_color: Some(palette.border),
            background: Some(palette.code_block_background.into()),
            text: TextStyleRefinement {
                font_family: Some(theme_settings.markdown_preview_code_font_family().clone()),
                font_fallbacks: Some(code_fallbacks.clone()),
                font_size: Some(buffer_font_size.into()),
                font_weight: Some(FontWeight::NORMAL),
                color: Some(palette.text),
                ..Default::default()
            },
            ..Default::default()
        },
        inline_code: TextStyleRefinement {
            font_family: Some(theme_settings.markdown_preview_code_font_family().clone()),
            font_fallbacks: Some(code_fallbacks),
            font_size: Some(buffer_font_size.into()),
            font_weight: Some(FontWeight::NORMAL),
            color: Some(palette.text),
            background_color: Some(palette.inline_code_background),
            ..Default::default()
        },
        link: TextStyleRefinement {
            color: Some(palette.link),
            underline: Some(UnderlineStyle {
                color: Some(palette.link.opacity(0.5)),
                thickness: px(1.),
                ..Default::default()
            }),
            ..Default::default()
        },
        soft_break_as_hard_break: false,
        heading_level_styles: Some(HeadingLevelStyles {
            h1: Some(heading_style(2.0)),
            h2: Some(heading_style(1.5)),
            h3: Some(heading_style(1.25)),
            h4: Some(heading_style(1.0)),
            h5: Some(heading_style(0.875)),
            h6: Some(heading_style(0.85)),
        }),
        heading: StyleRefinement {
            text: TextStyleRefinement {
                color: Some(palette.text),
                font_weight: Some(FontWeight::SEMIBOLD),
                ..Default::default()
            },
            ..Default::default()
        },
        heading_border_color: Some(palette.border),
        // github-markdown-css only underlines h1/h2; the shared default
        // (h1/h2/h3) exists for every other markdown consumer that sets
        // `heading_border_color` (e.g. the notebook cell preview), so this
        // is overridden here rather than changed globally.
        heading_border_levels: [true, true, false, false, false, false],
        syntax: github_syntax_theme(&palette),
        // Paragraphs and list items otherwise get a hardcoded
        // `line_height(rems(1.3))` from markdown.rs's own rendering code,
        // silently overriding the `1.5x` line-height set above on
        // `base_text_style` -- `rems(1.3)` is relative to the global UI rem
        // size, not `body_font_size`, so it renders visibly tighter than
        // intended once the preview font size differs from that base. This
        // overrides just the line-height value used, while leaving each
        // block's own margin (`mb_2`/`mb_1`) untouched -- unlike the
        // `height_is_multiple_of_line_height` escape hatch, which drops both
        // together and would need a compensating flex/gap container (that
        // breaks table cells' `h_full()` sizing, since it changes the
        // ancestor chain's height-resolution context for percentage heights).
        paragraph_line_height: Some((body_font_size * 1.5).into()),
        ..Default::default()
    }
}
