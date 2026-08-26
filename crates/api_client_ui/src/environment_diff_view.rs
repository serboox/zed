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
use ui::{Color, Icon, IconButton, IconName, Label, LabelSize, SharedString, Tooltip, prelude::*};
use workspace::{
    Item, ItemHandle as _, ItemNavHistory, ToolbarItemLocation, Workspace,
    item::{ItemEvent, TabContentParams},
    searchable::SearchableItemHandle,
};

use crate::response_view::{ResponseData, diffable_body_text};
use crate::store::ApiClientStore;

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
    ///
    /// An answer with nothing in it is said out loud rather than left as an
    /// empty pane: a 404 with no body against a 200 with one is a difference the
    /// reader has to be able to read, and two empty panes say nothing about
    /// which side was which.
    fn body(&self) -> String {
        match &self.outcome {
            Ok(response) => match diffable_body_text(response) {
                text if text.trim().is_empty() => {
                    format!("(no body — {} {})", response.status, response.status_text)
                }
                text => text,
            },
            Err(error) => format!("Request failed: {error}"),
        }
    }

    /// The status this side answered with, for telling two sides apart whose
    /// bodies happen to be equal.
    fn status(&self) -> Option<u16> {
        match &self.outcome {
            Ok(response) => Some(response.status),
            Err(_) => None,
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

/// Which of the two answers something is about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Left,
    Right,
}

impl Side {
    fn named(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
        }
    }
}

/// What one environment answered, in the shape the comparison shows it.
pub fn side_of_the_comparison(
    environment: SharedString,
    result: Result<api_client::HttpResponseSummary>,
) -> SideOfTheComparison {
    SideOfTheComparison {
        environment,
        outcome: match result {
            Ok(summary) => Ok(ResponseData::from_summary(summary)),
            Err(error) => Err(error.to_string().into()),
        },
    }
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
    /// Held so one side can be asked again from this tab: the request is still
    /// the store's, and only the store knows how to resolve it.
    store: Entity<ApiClientStore>,
    /// Which side is being asked again, if either.
    asking_again: Option<Side>,
    /// Held so asking one side again cancels an earlier ask of the same side.
    asking: Task<()>,
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
        store: Entity<ApiClientStore>,
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
                    store,
                    asking_again: None,
                    asking: Task::ready(()),
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
        // A whole comparison run again supersedes one side still being asked:
        // left alone, that ask would land afterwards and put half of an older
        // run back on screen.
        self.stop_asking();
        self.put_up(left, right, window, cx);
    }

    /// Forgets an ask still in flight. Dropping the task is what cancels it.
    fn stop_asking(&mut self) {
        self.asking_again = None;
        self.asking = Task::ready(());
    }

    /// Puts a pair of answers on screen and works out the diff between them.
    fn put_up(
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

    /// Sends the request again to one side's environment and puts the answer in
    /// its place, so what is already on screen can be read against a fresh one.
    fn ask_again(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        if self.asking_again.is_some() {
            return;
        }
        let environment = match side {
            Side::Left => self.compared.left,
            Side::Right => self.compared.right,
        };
        let named = match side {
            Side::Left => self.left.environment.clone(),
            Side::Right => self.right.environment.clone(),
        };
        let Some((client, resolved)) = self
            .store
            .read(cx)
            .what_to_send(self.compared.request_id, environment)
        else {
            return;
        };
        self.asking_again = Some(side);
        cx.notify();
        self.asking = cx.spawn_in(window, async move |this, cx| {
            let result = api_client::execute(&client, &resolved).await;
            this.update_in(cx, |this, window, cx| {
                // Not through `show`: that would drop the very task this is
                // running in. The ask is over by the time its answer is here.
                this.asking_again = None;
                let answer = side_of_the_comparison(named, result);
                let (left, right) = match side {
                    Side::Left => (answer, this.right.clone()),
                    Side::Right => (this.left.clone(), answer),
                };
                this.put_up(left, right, window, cx);
            })
            .ok();
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

    /// One side's own row: which environment answered, how it went, and the
    /// button that asks it again -- each side has its own, so what is on screen
    /// can be read against a fresh answer from either one.
    fn render_side(
        &self,
        side: &SideOfTheComparison,
        which: Side,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let named = which.named();
        let being_asked = self.asking_again == Some(which);
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
            .child(
                div()
                    .debug_selector(move || format!("environment-diff-ask-{named}"))
                    .child(
                        IconButton::new(
                            SharedString::from(format!("ask-again-{named}")),
                            IconName::ArrowCircle,
                        )
                        .icon_size(ui::IconSize::XSmall)
                        .disabled(self.asking_again.is_some())
                        .tooltip(Tooltip::text(match being_asked {
                            true => "Asking again…",
                            false => "Send this side again",
                        }))
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.ask_again(which, window, cx);
                            },
                        )),
                    ),
            )
            .debug_selector(move || format!("environment-diff-{named}"))
            .into_any_element()
    }

    /// Whether this request has a script that runs before it is sent. The
    /// comparison does not run it -- one script, two environments, and whatever
    /// it writes would be written twice -- so a request that has one is not
    /// compared quite as it is sent, and the reader is told rather than left to
    /// wonder why the answers look wrong.
    fn a_script_was_skipped(&self, cx: &App) -> bool {
        self.store
            .read(cx)
            .requests
            .iter()
            .find(|request| request.id == self.compared.request_id)
            .is_some_and(|request| !request.pre_request_script.trim().is_empty())
    }

    /// What the two answers amount to, in a few words.
    fn how_they_compare(&self) -> (SharedString, Color) {
        what_it_amounts_to(
            self.bodies_differ,
            self.left.status() != self.right.status(),
        )
    }
}

/// The bodies, and the statuses when those are what differ. A status is worth
/// saying out loud on its own: one environment answering 500 where the other
/// answers 200 with the same body is a difference the diff itself cannot show.
fn what_it_amounts_to(bodies_differ: bool, statuses_differ: bool) -> (SharedString, Color) {
    match (bodies_differ, statuses_differ) {
        (true, true) => ("bodies and statuses differ".into(), Color::Error),
        (true, false) => ("bodies differ".into(), Color::Muted),
        (false, true) => ("same body, different status".into(), Color::Error),
        (false, false) => ("identical answers".into(), Color::Muted),
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
                    .child(self.render_side(&self.left, Side::Left, cx))
                    .child(
                        Icon::new(IconName::ArrowRight)
                            .size(ui::IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(self.render_side(&self.right, Side::Right, cx)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .when(self.a_script_was_skipped(cx), |el| {
                        el.child(
                            div()
                                .debug_selector(|| "environment-diff-no-script".to_string())
                                .child(
                                    Label::new("pre-request script not run")
                                        .size(LabelSize::Small)
                                        .color(Color::Warning),
                                ),
                        )
                    })
                    .child({
                        let (what_it_amounts_to, colour) = self.how_they_compare();
                        div()
                            .debug_selector(|| "environment-diff-summary".to_string())
                            .child(
                                Label::new(what_it_amounts_to)
                                    .size(LabelSize::Small)
                                    .color(colour),
                            )
                    }),
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

    async fn a_workspace(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<ApiClientStore>,
        &mut VisualTestContext,
    ) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        let store = cx.new(ApiClientStore::new);
        (workspace, store, cx)
    }

    /// The two answers land in a tab that reads like a commit's diff: the left
    /// environment's body is the base text, the right one's is what changed.
    #[gpui::test]
    async fn the_two_answers_are_shown_as_a_diff(cx: &mut TestAppContext) {
        let (workspace, store, cx) = a_workspace(cx).await;
        let view = workspace
            .update_in(cx, |workspace, window, cx| {
                let workspace = workspace.weak_handle();
                window.spawn(cx, async move |cx| {
                    EnvironmentDiffView::open(
                        a_pair(),
                        "Get users".into(),
                        answered("Staging", 200, "{\n  \"total\": 1\n}"),
                        answered("Production", 200, "{\n  \"total\": 2\n}"),
                        store,
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
        let (workspace, store, cx) = a_workspace(cx).await;
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
                        store,
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
        let (workspace, store, cx) = a_workspace(cx).await;
        let pair = a_pair();
        let open =
            |left: SideOfTheComparison, right: SideOfTheComparison, cx: &mut VisualTestContext| {
                let store = store.clone();
                workspace.update_in(cx, |workspace, window, cx| {
                    let workspace = workspace.weak_handle();
                    window.spawn(cx, async move |cx| {
                        EnvironmentDiffView::open(
                            pair,
                            "Get users".into(),
                            left,
                            right,
                            store,
                            workspace,
                            cx,
                        )
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

    /// An answer with nothing in it is a fact, not an empty pane: a 404 that
    /// says nothing against a 200 that says something is exactly what the reader
    /// came to see.
    #[test]
    fn an_answer_with_no_body_says_which_status_it_came_with() {
        let empty = SideOfTheComparison {
            environment: "Production".into(),
            outcome: Ok(ResponseData {
                status: 404,
                status_text: "Not Found".into(),
                elapsed_ms: 12,
                size_bytes: 0,
                headers: Vec::new(),
                body: Vec::new(),
                cookies: Vec::new(),
                timings: api_client::Timings::default(),
            }),
        };
        assert_eq!(empty.body(), "(no body — 404 Not Found)");
        assert!(!empty.went_well());
        assert_eq!(empty.status(), Some(404));

        // Whitespace is nothing as well: a body of one newline reads as empty.
        let blank = answered("Staging", 204, "\n  \n");
        assert_eq!(blank.body(), "(no body — 204 OK)");
    }

    /// Two answers whose bodies match can still be different answers, and the
    /// summary has to say so rather than call them identical.
    #[test]
    fn a_status_that_differs_is_said_out_loud() {
        assert_eq!(
            what_it_amounts_to(false, true).0,
            "same body, different status"
        );
        assert_eq!(what_it_amounts_to(false, false).0, "identical answers");
        assert_eq!(what_it_amounts_to(true, false).0, "bodies differ");
        assert_eq!(
            what_it_amounts_to(true, true).0,
            "bodies and statuses differ"
        );
    }

    /// A whole comparison run again supersedes one side still being asked:
    /// otherwise that ask lands afterwards and puts half of an older run back
    /// on screen.
    #[gpui::test]
    async fn a_fresh_comparison_stops_a_side_still_being_asked(cx: &mut TestAppContext) {
        let (workspace, store, cx) = a_workspace(cx).await;
        let view = workspace
            .update_in(cx, |workspace, window, cx| {
                let workspace = workspace.weak_handle();
                let store = store.clone();
                window.spawn(cx, async move |cx| {
                    EnvironmentDiffView::open(
                        a_pair(),
                        "Get users".into(),
                        answered("Staging", 200, "{}"),
                        answered("Production", 200, "{}"),
                        store,
                        workspace,
                        cx,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        cx.run_until_parked();

        view.update_in(cx, |view, window, cx| {
            view.asking_again = Some(Side::Left);
            view.show(
                answered("Staging", 200, "{\n  \"total\": 1\n}"),
                answered("Production", 200, "{\n  \"total\": 2\n}"),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        view.read_with(cx, |view, _| {
            assert_eq!(
                view.asking_again, None,
                "the ask has to be forgotten, or its answer overwrites this one"
            );
            assert!(
                view.bodies_differ,
                "and the fresh pair is what is on screen"
            );
        });
    }

    /// A request with a script that runs before it is sent is not compared quite
    /// as it is sent, and the reader is told so rather than left to wonder.
    #[gpui::test]
    async fn a_script_the_comparison_skips_is_said_out_loud(cx: &mut TestAppContext) {
        let (workspace, store, cx) = a_workspace(cx).await;
        let collection_id = store.update(cx, |store, cx| store.create_collection("A".into(), cx));
        let request_id = store.update(cx, |store, cx| {
            store.create_request(collection_id, "Get users".into(), None, cx)
        });
        let compared = WhatIsCompared {
            request_id,
            left: uuid::Uuid::new_v4(),
            right: uuid::Uuid::new_v4(),
        };
        let opened = workspace.update_in(cx, |workspace, window, cx| {
            let workspace = workspace.weak_handle();
            let store = store.clone();
            window.spawn(cx, async move |cx| {
                EnvironmentDiffView::open(
                    compared,
                    "Get users".into(),
                    answered("Staging", 200, "{}"),
                    answered("Production", 200, "{}"),
                    store,
                    workspace,
                    cx,
                )
                .await
            })
        });
        opened.await.unwrap();
        cx.run_until_parked();
        let draw = |cx: &mut VisualTestContext| {
            cx.update(|window, cx| {
                window.refresh();
                let _ = window.draw(cx);
            });
            cx.run_until_parked();
        };
        draw(cx);
        assert!(
            cx.debug_bounds("environment-diff-no-script").is_none(),
            "a request without such a script has nothing to be told about"
        );

        store.update(cx, |store, cx| {
            store.update_request(request_id, cx, |request| {
                request.pre_request_script = "pm.environment.set('token', '1')".into();
            });
        });
        draw(cx);
        assert!(
            cx.debug_bounds("environment-diff-no-script").is_some(),
            "and one with it has to be told that the comparison did not run it"
        );
    }

    /// Each side can be asked again on its own, so what is on screen can be read
    /// against a fresh answer from either environment.
    ///
    /// What the button starts is the same send Send makes, and that is a real
    /// network round trip -- which this scheduler cannot drive, since a
    /// background thread waking a GPUI task is flagged as nondeterministic. What
    /// is asserted here is that each side has its own button and that the view
    /// knows which side is being asked.
    #[gpui::test]
    async fn each_side_has_its_own_button_to_ask_again(cx: &mut TestAppContext) {
        let (workspace, store, cx) = a_workspace(cx).await;
        let view = workspace
            .update_in(cx, |workspace, window, cx| {
                let workspace = workspace.weak_handle();
                window.spawn(cx, async move |cx| {
                    EnvironmentDiffView::open(
                        a_pair(),
                        "Get users".into(),
                        answered("Staging", 200, "{}"),
                        SideOfTheComparison {
                            environment: "Production".into(),
                            outcome: Ok(ResponseData {
                                status: 500,
                                status_text: "Internal Server Error".into(),
                                elapsed_ms: 8,
                                size_bytes: 0,
                                headers: Vec::new(),
                                body: Vec::new(),
                                cookies: Vec::new(),
                                timings: api_client::Timings::default(),
                            }),
                        },
                        store,
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

        let left = cx
            .debug_bounds("environment-diff-ask-left")
            .expect("the left side has a button of its own");
        let right = cx
            .debug_bounds("environment-diff-ask-right")
            .expect("the right side has one too");
        assert!(
            left.origin.x < right.origin.x,
            "one on each side, in that order: {left:?} then {right:?}"
        );
        assert!(
            cx.debug_bounds("environment-diff-summary").is_some(),
            "and the summary of what the two answers amount to is painted"
        );

        view.read_with(cx, |view, _| {
            assert_eq!(
                view.asking_again, None,
                "nothing is being asked again until the reader asks"
            );
            assert_eq!(
                view.right.body(),
                "(no body — 500 Internal Server Error)",
                "and an answer with nothing in it reads as its status"
            );
        });
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
