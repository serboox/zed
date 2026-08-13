use editor::Editor;
use gpui::{AnyElement, App, Entity, Window};
use ui::{TintColor, Tooltip, prelude::*};
use workspace::Pane;
use workspace::item::ItemHandle;
use workspace::preview_appearance::{preview_appearance, set_preview_appearance};

use crate::open_split_preview;
use crate::split_preview_view::{PreviewLayout, SplitPreviewView};

pub fn init(cx: &mut App) {
    workspace::register_item_overlay(cx, render_layout_switch);
}

/// The three-way layout switch, carried at the right end of the tab bar of the
/// pane it applies to. A document this editor can preview -- Markdown, HTML, an
/// OpenAPI contract -- offers the choice from the page it is being edited on, so
/// a reader does not have to know that a command exists in order to find the
/// preview at all.
fn render_layout_switch(
    item: &dyn ItemHandle,
    pane: &Entity<Pane>,
    _window: &mut Window,
    cx: &mut App,
) -> Option<AnyElement> {
    let target = SwitchTarget::for_item(item, pane, cx)?;
    let selected = target.layout(cx);

    Some(
        h_flex()
            .id("preview-layout-switch")
            .debug_selector(|| "preview-layout-switch".into())
            .gap_px()
            .children(PreviewLayout::ALL.map(|layout| {
                let target = target.clone();
                div()
                    .debug_selector(move || format!("preview-layout-{}", layout.to_db()))
                    .child(
                        IconButton::new(("preview-layout", layout.to_db() as usize), layout.icon())
                            .icon_size(IconSize::Small)
                            .toggle_state(layout == selected)
                            // Which layout is in force has to be visible at rest.
                            // `toggle_state` alone leaves a `ButtonLike` looking
                            // exactly as it did, so the chosen one is named in
                            // both the icon's color and a filled background.
                            .selected_icon_color(Color::Selected)
                            .selected_style(ButtonStyle::Tinted(TintColor::Accent))
                            .tooltip(Tooltip::text(layout.label()))
                            .on_click(move |_, window, cx| target.choose(layout, window, cx)),
                    )
            }))
            // The palette a document is read in belongs with the choice of how
            // it is read: same panel, right after the layouts.
            .child(
                div()
                    .debug_selector(|| "preview-appearance".into())
                    .child(render_appearance_button(cx)),
            )
            .into_any_element(),
    )
}

fn render_appearance_button(cx: &mut App) -> impl IntoElement {
    let appearance = preview_appearance(cx);
    // Two positions, and the letter says which: L for light, D for dark. An icon
    // would have to stand for a palette, which no icon does.
    Button::new("preview-appearance", appearance.initial())
        .label_size(LabelSize::Small)
        .tooltip(Tooltip::text(appearance.tooltip()))
        .on_click(move |_, _, cx| set_preview_appearance(appearance.next(), cx))
}

#[derive(Clone)]
enum SwitchTarget {
    /// A plain editor whose document can be previewed, and the pane holding it.
    Previewable(Entity<Editor>, Entity<Pane>),
    /// A document already opened next to its preview.
    Split(Entity<SplitPreviewView>),
}

impl SwitchTarget {
    fn for_item(item: &dyn ItemHandle, pane: &Entity<Pane>, cx: &App) -> Option<Self> {
        // A split preview reports itself as its inner editor, so it has to be
        // recognized before the editor case below.
        if let Some(view) = item.downcast::<SplitPreviewView>() {
            return Some(Self::Split(view));
        }
        let editor = item.act_as::<Editor>(cx)?;
        // Offered only when there is something to preview: a plain YAML file is
        // not an OpenAPI contract, and the switch must not claim otherwise.
        open_split_preview::preview_kind_for(&editor, cx)
            .is_some()
            .then_some(Self::Previewable(editor, pane.clone()))
    }

    fn layout(&self, cx: &App) -> PreviewLayout {
        match self {
            Self::Split(view) => view.read(cx).layout(),
            Self::Previewable(..) => PreviewLayout::Editor,
        }
    }

    fn choose(&self, layout: PreviewLayout, window: &mut Window, cx: &mut App) {
        match self {
            Self::Split(view) => view.update(cx, |view, cx| view.set_layout(layout, window, cx)),
            Self::Previewable(editor, pane) => {
                // The editor-only layout is what a plain editor tab already is.
                if layout == PreviewLayout::Editor {
                    return;
                }
                let Some(workspace) = pane.read(cx).workspace().upgrade() else {
                    return;
                };
                let editor = editor.clone();
                let pane = pane.clone();
                workspace.update(cx, |workspace, cx| {
                    open_split_preview::open_for_editor(
                        workspace, &pane, &editor, layout, window, cx,
                    );
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Modifiers, TestAppContext, VisualTestContext, px, size};
    use language::{Buffer, Language, LanguageConfig, LanguageMatcher};
    use project::Project;
    use serde_json::json;
    use settings::Settings as _;
    use std::sync::Arc;
    use util::path;
    use util::rel_path::rel_path;
    use workspace::{AppState, SplitDirection, Workspace};

    const CONTRACT: &str = "openapi: 3.0.3\ninfo:\n  title: Orders\n  version: 1.0.0\npaths: {}\n";

    fn yaml_language() -> Arc<Language> {
        Arc::new(Language::new(
            LanguageConfig {
                name: "YAML".into(),
                matcher: LanguageMatcher {
                    path_suffixes: vec!["yaml".into()],
                    ..LanguageMatcher::default()
                },
                ..LanguageConfig::default()
            },
            None,
        ))
    }

    fn markdown_language() -> Arc<Language> {
        Arc::new(Language::new(
            LanguageConfig {
                name: "Markdown".into(),
                matcher: LanguageMatcher {
                    path_suffixes: vec!["md".into()],
                    ..LanguageMatcher::default()
                },
                ..LanguageConfig::default()
            },
            None,
        ))
    }

    /// A workspace holding one real file, so that opening it goes through the
    /// project the way it does for a reader: the item carries a project entry,
    /// which is what makes the pane treat a second tab for it as a duplicate.
    async fn workspace_with_file<'a>(
        name: &str,
        contents: &str,
        language: Arc<Language>,
        cx: &'a mut TestAppContext,
    ) -> (Entity<Workspace>, &'a mut VisualTestContext) {
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            editor::init(cx);
            crate::init(cx);
            app_state
        });
        app_state
            .fs
            .as_fake()
            .insert_tree(path!("/project"), json!({ name: contents }))
            .await;
        let project = Project::test(app_state.fs.clone(), [path!("/project").as_ref()], cx).await;
        project.read_with(cx, |project, _| {
            project.languages().add(language);
        });
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));

        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("the project has a worktree")
                .read(cx)
                .id()
        });
        let opened = workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_path((worktree_id, rel_path(name)), None, true, window, cx)
        });
        opened.await.expect("the file opens");
        cx.run_until_parked();
        draw(cx);
        (workspace, cx)
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear();
        });
    }

    async fn open_contract_editor(
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, &mut VisualTestContext) {
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            editor::init(cx);
            crate::init(cx);
            app_state
        });
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));

        workspace.update_in(cx, |workspace, window, cx| {
            let buffer = cx.new(|cx| {
                let mut buffer = Buffer::local(CONTRACT, cx);
                buffer.set_language(Some(yaml_language()), cx);
                buffer
            });
            let editor = cx.new(|cx| Editor::for_buffer(buffer, None, window, cx));
            workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
        });
        cx.run_until_parked();
        draw(cx);
        (workspace, cx)
    }

    /// The switch used to be driven from a toolbar item, where clicking it
    /// changed the pane from inside the toolbar's own update and panicked. It is
    /// clicked here for real, through the painted button, so that path stays
    /// covered.
    #[gpui::test]
    async fn clicking_the_switch_over_a_contract_opens_the_split(cx: &mut TestAppContext) {
        let (workspace, cx) = open_contract_editor(cx).await;

        let switch = cx
            .debug_bounds("preview-layout-1")
            .expect("a contract must offer the layout switch over its editor");
        cx.simulate_click(switch.center(), Modifiers::none());
        cx.run_until_parked();
        draw(cx);

        let split = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<SplitPreviewView>())
        });
        let split = split.expect("choosing a layout must replace the tab with the split preview");
        assert_eq!(
            split.read_with(cx, |view, _| view.layout()),
            PreviewLayout::EditorAndPreview,
            "the chosen layout is the one that has to open"
        );
        assert!(
            cx.debug_bounds("preview-layout-switch").is_some(),
            "the switch has to stay reachable once the preview is open"
        );
    }

    /// The switch is painted over the active item of every pane, not only the
    /// focused one, so choosing a layout has to act on the pane it was clicked
    /// in rather than on whichever pane happens to be active.
    #[gpui::test]
    async fn the_switch_acts_on_the_pane_it_was_clicked_in(cx: &mut TestAppContext) {
        let (workspace, cx) = open_contract_editor(cx).await;

        let (contract_pane, other_pane) = workspace.update_in(cx, |workspace, window, cx| {
            let contract_pane = workspace.active_pane().clone();
            let other_pane =
                workspace.split_pane(contract_pane.clone(), SplitDirection::Right, window, cx);
            let buffer = cx.new(|cx| Buffer::local("name: orders\n", cx));
            let editor = cx.new(|cx| Editor::for_buffer(buffer, None, window, cx));
            workspace.add_item(
                other_pane.clone(),
                Box::new(editor),
                None,
                true,
                true,
                window,
                cx,
            );
            (contract_pane, other_pane)
        });
        cx.run_until_parked();
        draw(cx);

        assert_ne!(
            contract_pane.entity_id(),
            workspace.read_with(cx, |workspace, _| workspace.active_pane().entity_id()),
            "the contract has to sit in the pane that is not active for this to prove anything"
        );

        let switch = cx
            .debug_bounds("preview-layout-1")
            .expect("an inactive pane still offers the switch over its contract");
        cx.simulate_click(switch.center(), Modifiers::none());
        cx.run_until_parked();
        draw(cx);

        assert!(
            contract_pane.read_with(cx, |pane, _| pane
                .active_item()
                .and_then(|item| item.downcast::<SplitPreviewView>())
                .is_some()),
            "the split preview belongs in the pane the switch was clicked in"
        );
        assert!(
            other_pane.read_with(cx, |pane, _| pane
                .active_item()
                .and_then(|item| item.downcast::<SplitPreviewView>())
                .is_none()),
            "the other pane must keep the document it was showing"
        );
    }

    /// The pane counts a second tab for the same file as a duplicate and
    /// activates the tab it already has, so a split added before the old tab is
    /// closed is dropped and the file's tab simply disappears. A file opened
    /// through the project is the only way to reproduce that: a buffer with no
    /// file carries no project entry to collide on.
    #[gpui::test]
    async fn choosing_a_layout_for_a_file_replaces_its_tab_with_the_split(cx: &mut TestAppContext) {
        let (workspace, cx) = workspace_with_file("spec.yaml", CONTRACT, yaml_language(), cx).await;

        let switch = cx
            .debug_bounds("preview-layout-2")
            .expect("a contract opened from the project offers the switch");
        cx.simulate_click(switch.center(), Modifiers::none());
        cx.run_until_parked();
        draw(cx);

        let (item_count, split) = workspace.read_with(cx, |workspace, cx| {
            let pane = workspace.active_pane().read(cx);
            (
                pane.items_len(),
                pane.active_item()
                    .and_then(|item| item.downcast::<SplitPreviewView>()),
            )
        });
        assert_eq!(
            item_count, 1,
            "the split takes the tab's place instead of being added next to it"
        );
        let split = split.expect("the tab must hold the split preview, not be closed");
        assert_eq!(
            split.read_with(cx, |view, _| view.layout()),
            PreviewLayout::Preview,
            "the layout that was clicked is the one that opens"
        );
    }

    /// Replacing the tab must not run the save path: it reloads a saveable
    /// buffer from disk, which would throw away whatever the reader has typed
    /// and not saved yet.
    #[gpui::test]
    async fn edits_that_were_never_saved_survive_the_switch(cx: &mut TestAppContext) {
        let (workspace, cx) = workspace_with_file("spec.yaml", CONTRACT, yaml_language(), cx).await;

        let editor = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.act_as::<Editor>(cx))
                .expect("the file opened in an editor")
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.set_text(format!("{CONTRACT}# an unsaved thought\n"), window, cx);
        });
        cx.run_until_parked();
        draw(cx);
        assert!(
            editor.read_with(cx, |editor, cx| editor
                .buffer()
                .read(cx)
                .read(cx)
                .is_dirty()),
            "the buffer has to be dirty for this to prove anything"
        );

        let switch = cx
            .debug_bounds("preview-layout-1")
            .expect("the switch is offered for the edited contract");
        cx.simulate_click(switch.center(), Modifiers::none());
        cx.run_until_parked();
        draw(cx);

        let split = workspace
            .read_with(cx, |workspace, cx| {
                workspace
                    .active_item(cx)
                    .and_then(|item| item.downcast::<SplitPreviewView>())
            })
            .expect("the split opened");
        let text = split.read_with(cx, |view, cx| view.editor().read(cx).text(cx));
        assert!(
            text.contains("# an unsaved thought"),
            "the split has to carry the unsaved edit, got {text:?}"
        );
    }

    // The switch rides in the pane's tab bar, so it costs the window no row of its
    // own and covers no line of the document. Measured against the tab bar it sits
    // in and against the document below it, since "above the document" is the
    // whole point: a row over the first line is what this placement replaced.
    #[gpui::test]
    async fn the_switch_rides_in_the_tab_bar_above_the_document(cx: &mut TestAppContext) {
        let (_, cx) = workspace_with_file("spec.yaml", CONTRACT, yaml_language(), cx).await;

        let switch = cx
            .debug_bounds("preview-layout-switch")
            .expect("the switch is painted");
        let document = cx
            .debug_bounds("pane-item-area")
            .expect("the document area is painted");

        assert!(
            switch.size.width > px(0.) && switch.size.height > px(0.),
            "the switch has to occupy real screen area to be clickable"
        );
        assert!(
            document.size.width > px(0.) && document.size.height > px(0.),
            "the document must keep real area of its own: {:?}",
            document.size
        );
        assert!(
            switch.origin.y + switch.size.height <= document.origin.y,
            "the switch belongs above the document, covering none of it, \
             got {switch:?} against {document:?}"
        );
        assert!(
            switch.origin.x > document.origin.x + document.size.width / 2.,
            "the switch belongs at the right end of the row, got {switch:?}"
        );
    }

    // The tab bar's end section does not shrink, and these controls are added
    // ahead of the pane's own New / Split / Zoom. On a narrow pane that is how a
    // button ends up past the edge, where it cannot be clicked at all -- so the
    // whole group is measured against the window at a deliberately narrow width.
    #[gpui::test]
    async fn a_narrow_pane_keeps_every_tab_bar_button_reachable(cx: &mut TestAppContext) {
        let (_, cx) = workspace_with_file("spec.yaml", CONTRACT, yaml_language(), cx).await;

        for width in [px(900.), px(600.), px(420.)] {
            cx.simulate_resize(size(width, px(700.)));
            cx.run_until_parked();

            let viewport = cx.update(|window, _| window.viewport_size());
            let switch = cx
                .debug_bounds("preview-layout-switch")
                .expect("the switch is painted");
            let buttons = cx
                .debug_bounds("pane-tab-bar-buttons")
                .expect("the pane's own buttons are painted");

            assert!(
                buttons.origin.x >= px(0.)
                    && buttons.origin.x + buttons.size.width <= viewport.width,
                "at a width of {width:?} the tab bar's buttons painted {buttons:?}, \
                 past the {viewport:?} window"
            );
            assert!(
                switch.origin.x >= px(0.) && switch.origin.x + switch.size.width <= viewport.width,
                "at a width of {width:?} the switch painted {switch:?}, \
                 past the {viewport:?} window"
            );
        }
    }

    #[gpui::test]
    async fn every_layout_can_be_reached_by_clicking(cx: &mut TestAppContext) {
        let (workspace, cx) = workspace_with_file("spec.yaml", CONTRACT, yaml_language(), cx).await;

        let layout_of = |cx: &mut VisualTestContext| {
            workspace.read_with(cx, |workspace, cx| {
                workspace
                    .active_pane()
                    .read(cx)
                    .active_item()
                    .and_then(|item| item.downcast::<SplitPreviewView>())
                    .map(|view| view.read(cx).layout())
            })
        };

        for layout in PreviewLayout::ALL {
            let selector = match layout {
                PreviewLayout::Editor => "preview-layout-0",
                PreviewLayout::EditorAndPreview => "preview-layout-1",
                PreviewLayout::Preview => "preview-layout-2",
            };
            let button = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} has to be painted"));
            cx.simulate_click(button.center(), Modifiers::none());
            cx.run_until_parked();
            draw(cx);

            match layout {
                // Editor-only is what a plain tab already is, so the first
                // click on it must leave the document alone.
                PreviewLayout::Editor => assert_eq!(
                    layout_of(cx),
                    None,
                    "asking for the editor from a plain editor tab must change nothing"
                ),
                chosen => assert_eq!(
                    layout_of(cx),
                    Some(chosen),
                    "clicking {} has to leave the tab in that layout",
                    chosen.label()
                ),
            }
        }

        // Back to the editor from inside the split: the same button now has a
        // split preview to act on rather than a plain tab.
        let button = cx
            .debug_bounds("preview-layout-0")
            .expect("the editor button is painted over the split too");
        cx.simulate_click(button.center(), Modifiers::none());
        cx.run_until_parked();
        draw(cx);
        assert_eq!(
            layout_of(cx),
            Some(PreviewLayout::Editor),
            "the split stays, showing only its editor"
        );
    }

    #[gpui::test]
    async fn markdown_is_offered_the_same_switch(cx: &mut TestAppContext) {
        let (workspace, cx) = workspace_with_file(
            "notes.md",
            "# Notes\n\nSome prose.\n",
            markdown_language(),
            cx,
        )
        .await;

        let switch = cx
            .debug_bounds("preview-layout-1")
            .expect("markdown offers the switch as well");
        cx.simulate_click(switch.center(), Modifiers::none());
        cx.run_until_parked();
        draw(cx);

        assert!(
            workspace.read_with(cx, |workspace, cx| {
                workspace
                    .active_item(cx)
                    .and_then(|item| item.downcast::<SplitPreviewView>())
                    .is_some()
            }),
            "markdown has to open next to its preview from the same control"
        );
    }

    /// The reported bug: the switch opened the split and its preview half showed
    /// nothing at all. Opening the split is not enough -- the preview has to be
    /// showing the document.
    #[gpui::test]
    async fn the_markdown_split_shows_the_document_in_its_preview(cx: &mut TestAppContext) {
        const DOCUMENT: &str = "# Notes\n\nSome prose.\n";
        let (workspace, cx) =
            workspace_with_file("notes.md", DOCUMENT, markdown_language(), cx).await;

        let switch = cx
            .debug_bounds("preview-layout-1")
            .expect("markdown offers the switch");
        cx.simulate_click(switch.center(), Modifiers::none());
        cx.run_until_parked();
        draw(cx);

        let shown = workspace.read_with(cx, |workspace, cx| {
            let split = workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<SplitPreviewView>())
                .expect("the split is open");
            let preview = split
                .read(cx)
                .preview()
                .clone()
                .downcast::<markdown_preview::markdown_preview_view::MarkdownPreviewView>()
                .expect("a markdown split previews markdown");
            preview.read(cx).source(cx)
        });

        assert_eq!(
            shown.as_ref(),
            DOCUMENT,
            "the preview half has to be showing the document, not an empty page"
        );

        let half = cx
            .debug_bounds("split-preview-preview")
            .expect("the preview half is painted");
        let content = cx
            .debug_bounds("markdown-preview-content")
            .expect("the preview half paints the document, not an empty page");
        // The container that collapsed: it clips everything below it, so a page
        // laid out at full height still came out as a sliver of its own padding.
        let scroll = cx
            .debug_bounds("markdown-preview-scroll")
            .expect("the preview half has a scroll container");
        assert!(
            scroll.size.height > half.size.height / 2.,
            "the page's scroll container has to fill the half, not collapse to its \
             padding: half {:?}, container {:?}",
            half.size,
            scroll.size
        );

        // `inner` is the markdown element's own container, and its bounds are
        // recorded while painting: no entry means the page was laid out and then
        // never drawn, which is exactly how the half came out blank.
        let body = cx
            .debug_bounds("inner")
            .expect("the document has to be painted, not merely laid out");
        assert!(
            body.size.height > px(0.),
            "the painted document has to have real height: {:?}",
            body.size
        );

        // A heading, a paragraph and two list items cannot fit in a sliver a few
        // pixels tall -- that is what an empty-looking preview half measures.
        assert!(
            content.size.width > half.size.width / 2. && content.size.height > px(60.),
            "the document has to fill the preview half it was given: half {:?}, content {:?}",
            half.size,
            content.size
        );
    }

    /// Zoom, pressed while reading a document next to its source, has to scale
    /// the page. The font size of the editor half is an application-wide setting
    /// handled on the workspace, so without a handler here the keystroke walked
    /// straight past the page it was aimed at.
    #[gpui::test]
    async fn zoom_scales_the_page_being_read_not_the_code_beside_it(cx: &mut TestAppContext) {
        let (_workspace, cx) = workspace_with_file(
            "notes.md",
            "# Notes\n\nSome prose.\n",
            markdown_language(),
            cx,
        )
        .await;

        let switch = cx
            .debug_bounds("preview-layout-1")
            .expect("markdown offers the switch");
        cx.simulate_click(switch.center(), Modifiers::none());
        cx.run_until_parked();
        draw(cx);

        let (page_before, code_before) = cx.update(|_, cx| {
            let settings = theme_settings::ThemeSettings::get_global(cx);
            (
                settings.markdown_preview_font_size(cx),
                settings.buffer_font_size(cx),
            )
        });

        cx.dispatch_action(zed_actions::IncreaseBufferFontSize { persist: false });
        cx.run_until_parked();
        draw(cx);

        let (page_after, code_after) = cx.update(|_, cx| {
            let settings = theme_settings::ThemeSettings::get_global(cx);
            (
                settings.markdown_preview_font_size(cx),
                settings.buffer_font_size(cx),
            )
        });

        assert!(
            page_after > page_before,
            "the page has to grow: {page_before:?} -> {page_after:?}"
        );
        assert_eq!(
            code_after, code_before,
            "the code beside the page has to keep its own size"
        );
    }

    /// With no page on screen there is nothing here to zoom: the keystroke has to
    /// be left to the application rather than quietly scaling a hidden page.
    #[gpui::test]
    async fn zoom_leaves_a_hidden_page_alone(cx: &mut TestAppContext) {
        let (workspace, cx) =
            workspace_with_file("notes.md", "# Notes\n", markdown_language(), cx).await;

        let switch = cx
            .debug_bounds("preview-layout-1")
            .expect("markdown offers the switch");
        cx.simulate_click(switch.center(), Modifiers::none());
        cx.run_until_parked();
        draw(cx);
        let editor_only = cx
            .debug_bounds("preview-layout-0")
            .expect("the editor-only layout is offered over the split");
        cx.simulate_click(editor_only.center(), Modifiers::none());
        cx.run_until_parked();
        draw(cx);
        let layout = workspace.read_with(cx, |workspace, cx| {
            workspace
                .active_item(cx)
                .and_then(|item| item.downcast::<SplitPreviewView>())
                .map(|view| view.read(cx).layout())
        });
        assert_eq!(
            layout,
            Some(PreviewLayout::Editor),
            "only the editor is showing"
        );

        let page_before = cx.update(|_, cx| {
            theme_settings::ThemeSettings::get_global(cx).markdown_preview_font_size(cx)
        });
        cx.dispatch_action(zed_actions::IncreaseBufferFontSize { persist: false });
        cx.run_until_parked();
        let page_after = cx.update(|_, cx| {
            theme_settings::ThemeSettings::get_global(cx).markdown_preview_font_size(cx)
        });

        assert_eq!(
            page_after, page_before,
            "a page nobody is reading must not be scaled"
        );
    }

    #[gpui::test]
    async fn a_plain_document_is_offered_no_switch(cx: &mut TestAppContext) {
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            editor::init(cx);
            crate::init(cx);
            app_state
        });
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));

        workspace.update_in(cx, |workspace, window, cx| {
            let buffer = cx.new(|cx| {
                let mut buffer = Buffer::local("name: orders\nreplicas: 2\n", cx);
                buffer.set_language(Some(yaml_language()), cx);
                buffer
            });
            let editor = cx.new(|cx| Editor::for_buffer(buffer, None, window, cx));
            workspace.add_item_to_active_pane(Box::new(editor), None, true, window, cx);
        });
        cx.run_until_parked();
        draw(cx);

        assert!(
            cx.debug_bounds("preview-layout-switch").is_none(),
            "a YAML file that is not a contract has nothing to preview"
        );
    }
}
