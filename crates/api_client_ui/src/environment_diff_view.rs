use std::any::{Any, TypeId};
use std::sync::Arc;

use anyhow::Result;
use buffer_diff::BufferDiff;
use editor::{Editor, EditorEvent, EditorSettings, MultiBuffer, SplittableEditor};
use gpui::{
    AnyElement, App, AppContext as _, AsyncWindowContext, Context, Entity, EventEmitter,
    FocusHandle, Focusable, Font, IntoElement, Render, Task, WeakEntity, Window,
};
use language::{Buffer, HighlightedText, LanguageRegistry};
use settings::Settings as _;
use ui::{Color, Icon, IconName, Label, LabelSize, SharedString, prelude::*};
use workspace::{
    Item, ItemHandle as _, ItemNavHistory, ToolbarItemLocation, Workspace,
    item::{ItemEvent, TabContentParams},
    searchable::SearchableItemHandle,
};

use crate::response_view::{ResponseData, diffable_body_text};

/// What one environment answered. A failure is a side of the comparison too --
/// "stage answered and production did not" is exactly the kind of thing this
/// tab exists to show, so it is shown in the diff rather than swallowed.
#[derive(Clone)]
pub struct SideOfTheComparison {
    pub environment: SharedString,
    pub outcome: Result<ResponseData, SharedString>,
}

impl SideOfTheComparison {
    /// The text this side contributes to the diff.
    fn body(&self) -> String {
        match &self.outcome {
            Ok(response) => diffable_body_text(response),
            Err(error) => format!("Request failed: {error}"),
        }
    }

    /// Status, time and size, which is what tells apart two answers whose
    /// bodies happen to be equal.
    fn how_it_went(&self) -> SharedString {
        match &self.outcome {
            Ok(response) => format!(
                "{} {} · {} ms · {}",
                response.status,
                response.status_text,
                response.elapsed_ms,
                how_big(response.size_bytes)
            )
            .into(),
            Err(_) => "failed".into(),
        }
    }

    fn went_well(&self) -> bool {
        matches!(&self.outcome, Ok(response) if (200..400).contains(&response.status))
    }
}

fn how_big(bytes: usize) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        _ => format!("{:.1} KB", bytes as f64 / 1024.),
    }
}

/// Which comparison a tab is showing, so asking for the same one again reuses
/// the tab instead of stacking another copy of it beside it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct WhatIsCompared {
    pub request_id: api_client::RequestId,
    pub left: api_client::EnvironmentId,
    pub right: api_client::EnvironmentId,
}

/// Whether the comparison went into a tab of its own or into the one already
/// showing this very pair, which hands the answers back to be shown there.
enum WhereItWent {
    Opened(Entity<EnvironmentDiffView>),
    AlreadyOpen(
        Entity<EnvironmentDiffView>,
        SideOfTheComparison,
        SideOfTheComparison,
    ),
}

/// Two answers to one request, from two environments, diffed the way a commit
/// is diffed.
pub struct EnvironmentDiffView {
    editor: Entity<SplittableEditor>,
    left_buffer: Entity<Buffer>,
    right_buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
    languages: Option<Arc<LanguageRegistry>>,
    request_name: SharedString,
    left: SideOfTheComparison,
    right: SideOfTheComparison,
    /// Worked out when the answers arrive, not while rendering: pretty-printing
    /// both bodies on every frame is real work on a response of any size.
    bodies_differ: bool,
    compared: WhatIsCompared,
    /// Held so a comparison run again cancels the diff still being worked out
    /// for the run before it, which would otherwise land last and win.
    recomputing_the_diff: Task<()>,
    /// Held for the same reason: a language looked up for the run before could
    /// otherwise arrive after this run's and colour it as the wrong shape.
    colouring_the_bodies: Task<()>,
}

impl EnvironmentDiffView {
    /// Opens the comparison in the workspace, reusing a tab already showing
    /// this very pair for this very request.
    pub async fn open(
        compared: WhatIsCompared,
        request_name: SharedString,
        left: SideOfTheComparison,
        right: SideOfTheComparison,
        workspace: WeakEntity<Workspace>,
        cx: &mut AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        let languages = workspace
            .update(cx, |workspace, _| workspace.app_state().languages.clone())
            .ok();
        let left_text = left.body();
        let right_text = right.body();
        let bodies_differ = left_text != right_text;
        let left_buffer = cx.new(|cx| Buffer::local(left_text, cx));
        let right_buffer = cx.new(|cx| Buffer::local(right_text, cx));
        let diff = build_the_diff(&left_buffer, &right_buffer, cx).await;

        // Looking for a tab for this pair and inserting one happen in the same
        // turn: doing the lookup before the buffers are built would let two
        // comparisons of one pair both find nothing and open a tab each.
        let opened = workspace.update_in(cx, move |workspace, window, cx| {
            if let Some(view) = workspace
                .items_of_type::<Self>(cx)
                .find(|view| view.read(cx).compared == compared)
            {
                return WhereItWent::AlreadyOpen(view, left, right);
            }
            let project = workspace.project().clone();
            let workspace_entity = cx.entity();
            let view = cx.new(|cx| {
                let multibuffer = cx.new(|cx| {
                    let mut multibuffer = MultiBuffer::singleton(right_buffer.clone(), cx);
                    multibuffer.add_diff(diff.clone(), cx);
                    multibuffer
                });
                let editor = cx.new(|cx| {
                    SplittableEditor::new(
                        EditorSettings::get_global(cx).diff_view_style,
                        multibuffer,
                        project,
                        workspace_entity,
                        window,
                        cx,
                    )
                });
                Self {
                    editor,
                    left_buffer,
                    right_buffer,
                    diff,
                    languages,
                    request_name,
                    left,
                    right,
                    bodies_differ,
                    compared,
                    recomputing_the_diff: Task::ready(()),
                    colouring_the_bodies: Task::ready(()),
                }
            });
            workspace.active_pane().update(cx, |pane, cx| {
                pane.add_item(Box::new(view.clone()), true, true, None, window, cx);
            });
            WhereItWent::Opened(view)
        })?;

        match opened {
            WhereItWent::Opened(view) => {
                view.update_in(cx, |view, _window, cx| view.colour_the_bodies(cx))?;
                Ok(view)
            }
            WhereItWent::AlreadyOpen(view, left, right) => {
                view.update_in(cx, |view, window, cx| view.show(left, right, window, cx))?;
                workspace.update_in(cx, |workspace, window, cx| {
                    workspace.activate_item(&view, true, true, window, cx);
                })?;
                Ok(view)
            }
        }
    }

    /// Puts a fresh pair of answers in front of the reader, for a comparison
    /// run again on a tab that is already open.
    pub fn show(
        &mut self,
        left: SideOfTheComparison,
        right: SideOfTheComparison,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let left_text = left.body();
        let right_text = right.body();
        self.bodies_differ = left_text != right_text;
        self.left = left;
        self.right = right;
        self.left_buffer.update(cx, |buffer, cx| {
            buffer.set_text(left_text, cx);
        });
        self.right_buffer.update(cx, |buffer, cx| {
            buffer.set_text(right_text, cx);
        });
        self.colour_the_bodies(cx);
        cx.notify();

        let left_buffer = self.left_buffer.clone();
        let right_buffer = self.right_buffer.clone();
        let diff = self.diff.clone();
        self.recomputing_the_diff = cx.spawn_in(window, async move |_, cx| {
            let left_text = left_buffer.read_with(cx, |buffer, _| buffer.snapshot().text());
            let right_snapshot = right_buffer.read_with(cx, |buffer, _| buffer.snapshot());
            diff.update(cx, |diff, cx| {
                diff.set_base_text(Some(left_text.into()), right_snapshot.text.clone(), cx)
            })
            .await;
        });
    }

    /// Gives both bodies the language they are written in, so the diff reads
    /// like the response tab rather than like flat text.
    fn colour_the_bodies(&mut self, cx: &mut Context<Self>) {
        let Some(languages) = self.languages.clone() else {
            return;
        };
        let Some(name) = self.language_of_the_bodies() else {
            return;
        };
        let left_buffer = self.left_buffer.clone();
        let right_buffer = self.right_buffer.clone();
        let base_text_buffer = self.diff.read(cx).base_text_buffer().clone();
        self.colouring_the_bodies = cx.spawn(async move |_, cx| {
            let Ok(language) = languages.language_for_name(name).await else {
                return;
            };
            for buffer in [left_buffer, right_buffer, base_text_buffer] {
                buffer.update(cx, |buffer, cx| {
                    buffer.set_language(Some(language.clone()), cx);
                });
            }
        });
    }

    /// Both sides have to agree on the language: two different shapes have
    /// nothing to gain from being highlighted as one of them.
    fn language_of_the_bodies(&self) -> Option<&'static str> {
        let shape_of = |side: &SideOfTheComparison| match &side.outcome {
            Ok(response) => {
                crate::response_view::pretty_print_body(&response.body, response.content_type())
                    .map(|(_, shape)| shape)
            }
            Err(_) => None,
        };
        match (shape_of(&self.left), shape_of(&self.right)) {
            (Some(left), Some(right)) if left == right => Some(left),
            (Some(only), None) | (None, Some(only)) => Some(only),
            _ => None,
        }
    }

    fn render_side(&self, side: &SideOfTheComparison, which: &'static str) -> AnyElement {
        h_flex()
            .gap_1p5()
            .items_center()
            .child(
                Icon::new(IconName::Pin)
                    .size(ui::IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(Label::new(side.environment.clone()).size(LabelSize::Small))
            .child(Label::new(side.how_it_went()).size(LabelSize::Small).color(
                if side.went_well() {
                    Color::Success
                } else {
                    Color::Error
                },
            ))
            .debug_selector(|| format!("environment-diff-{which}"))
            .into_any_element()
    }
}

/// The diff itself: the left environment's answer is the base text the right
/// one is read against, so the hunks read as "what production has that stage
/// does not".
async fn build_the_diff(
    left_buffer: &Entity<Buffer>,
    right_buffer: &Entity<Buffer>,
    cx: &mut AsyncWindowContext,
) -> Entity<BufferDiff> {
    let left_snapshot = left_buffer.read_with(cx, |buffer, _| buffer.snapshot());
    let right_snapshot = right_buffer.read_with(cx, |buffer, _| buffer.snapshot());
    let languages = right_buffer.read_with(cx, |buffer, _| buffer.language_registry());

    let diff = cx.new(|cx| {
        BufferDiff::new(
            &right_snapshot.text,
            right_snapshot.language().cloned(),
            languages,
            cx,
        )
    });
    diff.update(cx, |diff, cx| {
        diff.set_base_text(
            Some(left_snapshot.text().into()),
            right_snapshot.text.clone(),
            cx,
        )
    })
    .await;
    diff
}

impl EventEmitter<EditorEvent> for EnvironmentDiffView {}

impl Focusable for EnvironmentDiffView {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Item for EnvironmentDiffView {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff).color(Color::Muted))
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .color(if params.selected {
                Color::Default
            } else {
                Color::Muted
            })
            .into_any_element()
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        format!(
            "{}: {} ↔ {}",
            self.request_name, self.left.environment, self.right.environment
        )
        .into()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(
            format!(
                "{} against {} and {}",
                self.request_name, self.left.environment, self.right.environment
            )
            .into(),
        )
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("API Environment Comparison Opened")
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.deactivated(window, cx);
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        cx: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else {
            self.editor.act_as_type(type_id, cx)
        }
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.rhs_editor().update(cx, |editor, _| {
                editor.set_nav_history(Some(nav_history));
            })
        });
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor.update(cx, |editor, cx| {
            editor
                .rhs_editor()
                .update(cx, |editor, cx| editor.navigate(data, window, cx))
        })
    }

    fn breadcrumb_location(&self, _: &App) -> ToolbarItemLocation {
        ToolbarItemLocation::PrimaryLeft
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<Font>)> {
        self.editor.breadcrumbs(cx)
    }

    fn can_save(&self, _cx: &App) -> bool {
        false
    }
}

impl Render for EnvironmentDiffView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The two answers' own facts sit above the diff, on one row: a diff of
        // equal bodies still differs in status, time and size.
        let heading = h_flex()
            .id("environment-diff-heading")
            .debug_selector(|| "environment-diff-heading".to_string())
            .flex_none()
            .w_full()
            .px_2()
            .py_1()
            .gap_3()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .bg(cx.theme().colors().background)
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(self.render_side(&self.left, "left"))
                    .child(
                        Icon::new(IconName::ArrowRight)
                            .size(ui::IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(self.render_side(&self.right, "right")),
            )
            .child(
                Label::new(if self.bodies_differ {
                    "bodies differ"
                } else {
                    "identical bodies"
                })
                .size(LabelSize::Small)
                .color(Color::Muted),
            );

        v_flex()
            .size_full()
            .child(heading)
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::test::editor_test_context::assert_state_with_diff;
    use gpui::{BorrowAppContext, TestAppContext, VisualTestContext};
    use project::{FakeFs, Project};
    use settings::{DiffViewStyle, SettingsStore};
    use unindent::unindent;
    use workspace::MultiWorkspace;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.diff_view_style = Some(DiffViewStyle::Unified);
                });
            });
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    fn answered(environment: &str, status: u16, body: &str) -> SideOfTheComparison {
        SideOfTheComparison {
            environment: environment.to_string().into(),
            outcome: Ok(ResponseData {
                status,
                status_text: "OK".into(),
                elapsed_ms: 42,
                size_bytes: body.len(),
                headers: vec![("content-type".into(), "application/json".into())],
                body: body.as_bytes().to_vec(),
                cookies: Vec::new(),
                timings: api_client::Timings::default(),
            }),
        }
    }

    fn a_pair() -> WhatIsCompared {
        WhatIsCompared {
            request_id: uuid::Uuid::new_v4(),
            left: uuid::Uuid::new_v4(),
            right: uuid::Uuid::new_v4(),
        }
    }

    async fn a_workspace(cx: &mut TestAppContext) -> (Entity<Workspace>, &mut VisualTestContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        (workspace, cx)
    }

    /// The two answers land in a tab that reads like a commit's diff: the left
    /// environment's body is the base text, the right one's is what changed.
    #[gpui::test]
    async fn the_two_answers_are_shown_as_a_diff(cx: &mut TestAppContext) {
        let (workspace, cx) = a_workspace(cx).await;
        let view = workspace
            .update_in(cx, |workspace, window, cx| {
                let workspace = workspace.weak_handle();
                window.spawn(cx, async move |cx| {
                    EnvironmentDiffView::open(
                        a_pair(),
                        "Get users".into(),
                        answered("Staging", 200, "{\n  \"total\": 1\n}"),
                        answered("Production", 200, "{\n  \"total\": 2\n}"),
                        workspace,
                        cx,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        cx.run_until_parked();

        assert_state_with_diff(
            &view.read_with(cx, |view, cx| view.editor.read(cx).rhs_editor().clone()),
            cx,
            &unindent(
                "
                  ˇ{
                -   \"total\": 1
                +   \"total\": 2
                  }",
            ),
        );

        view.read_with(cx, |view, cx| {
            assert_eq!(
                view.tab_content_text(0, cx),
                "Get users: Staging ↔ Production",
                "the tab has to name the request and both environments"
            );
            assert!(view.bodies_differ);
        });
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.items_of_type::<EnvironmentDiffView>(cx).count(),
                1,
                "the comparison opens in the workspace, as a tab of its own"
            );
        });
    }

    /// Both sides' status, time and size are on the heading row: two bodies can
    /// be equal while the answers are not.
    #[gpui::test]
    async fn the_heading_says_how_each_side_went(cx: &mut TestAppContext) {
        let (workspace, cx) = a_workspace(cx).await;
        let _view = workspace
            .update_in(cx, |workspace, window, cx| {
                let workspace = workspace.weak_handle();
                window.spawn(cx, async move |cx| {
                    EnvironmentDiffView::open(
                        a_pair(),
                        "Get users".into(),
                        answered("Staging", 200, "{}"),
                        SideOfTheComparison {
                            environment: "Production".into(),
                            outcome: Err("connection refused".into()),
                        },
                        workspace,
                        cx,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("environment-diff-left").is_some()
                && cx.debug_bounds("environment-diff-right").is_some(),
            "both sides of the comparison are painted, failure included"
        );
        assert!(cx.debug_bounds("environment-diff-heading").is_some());
    }

    /// Running the same comparison again puts the fresh answers in the tab that
    /// is already open, rather than stacking another copy of it beside it.
    #[gpui::test]
    async fn comparing_the_same_pair_again_reuses_the_tab(cx: &mut TestAppContext) {
        let (workspace, cx) = a_workspace(cx).await;
        let pair = a_pair();
        let open = |left: SideOfTheComparison,
                    right: SideOfTheComparison,
                    cx: &mut VisualTestContext| {
            workspace.update_in(cx, |workspace, window, cx| {
                let workspace = workspace.weak_handle();
                window.spawn(cx, async move |cx| {
                    EnvironmentDiffView::open(pair, "Get users".into(), left, right, workspace, cx)
                        .await
                })
            })
        };

        let first = open(
            answered("Staging", 200, "{\n  \"total\": 1\n}"),
            answered("Production", 200, "{\n  \"total\": 2\n}"),
            cx,
        )
        .await
        .unwrap();
        cx.run_until_parked();

        let again = open(
            answered("Staging", 200, "{\n  \"total\": 7\n}"),
            answered("Production", 200, "{\n  \"total\": 7\n}"),
            cx,
        )
        .await
        .unwrap();
        cx.run_until_parked();

        assert_eq!(
            first.entity_id(),
            again.entity_id(),
            "the same pair of environments has to reuse its tab"
        );
        workspace.read_with(cx, |workspace, cx| {
            assert_eq!(
                workspace.items_of_type::<EnvironmentDiffView>(cx).count(),
                1
            );
        });
        again.read_with(cx, |view, _| {
            assert!(
                !view.bodies_differ,
                "and it has to be showing the answers from the second run"
            );
        });
        assert_state_with_diff(
            &again.read_with(cx, |view, cx| view.editor.read(cx).rhs_editor().clone()),
            cx,
            &unindent(
                "
                {
                  \"total\": 7
                }ˇ",
            ),
        );
    }

    #[test]
    fn a_failure_is_a_side_of_the_comparison_too() {
        let failed = SideOfTheComparison {
            environment: "Production".into(),
            outcome: Err("connection refused".into()),
        };
        assert_eq!(failed.body(), "Request failed: connection refused");
        assert_eq!(failed.how_it_went(), "failed");
        assert!(!failed.went_well());
    }

    #[test]
    fn a_body_is_pretty_printed_before_it_is_diffed() {
        let side = answered("Staging", 200, "{\"total\":1}");
        assert_eq!(
            side.body(),
            "{\n  \"total\": 1\n}",
            "diffing the servers' own whitespace would report differences that are not there"
        );
    }
}
