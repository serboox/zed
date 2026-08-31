use crate::runner::RunnerRowResult;
use crate::store::ApiClientStore;
use api_client::CollectionId;
use editor::Editor;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, ScrollHandle, SharedString,
    Window,
};
use ui::{
    Icon, IconName, IconSize, Label, LabelSize, ScrollAxes, Scrollbars, WithScrollbar, cyberpunk,
    prelude::*,
};
use util::ResultExt;
use workspace::{Item, item::ItemEvent};

enum RunStatus {
    Idle,
    Running,
    Done(Vec<RunnerRowResult>),
    Error(String),
}

pub struct RunnerView {
    focus_handle: FocusHandle,
    store: Entity<ApiClientStore>,
    selected_collection: Option<CollectionId>,
    data_file_editor: Entity<Editor>,
    results_scroll_handle: ScrollHandle,
    status: RunStatus,
}

impl RunnerView {
    pub fn new(store: Entity<ApiClientStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let selected_collection = store
            .read(cx)
            .collections
            .first()
            .map(|collection| collection.id);
        let data_file_editor = cx.new(|cx| {
            let mut editor = Editor::multi_line(window, cx);
            editor.set_placeholder_text(
                "Optional CSV or JSON-array data file -- one iteration per row/object.",
                window,
                cx,
            );
            editor
        });

        Self {
            focus_handle: cx.focus_handle(),
            store,
            selected_collection,
            data_file_editor,
            results_scroll_handle: ScrollHandle::new(),
            status: RunStatus::Idle,
        }
    }

    fn select_collection(&mut self, id: CollectionId, cx: &mut Context<Self>) {
        self.selected_collection = Some(id);
        cx.notify();
    }

    fn run(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(collection_id) = self.selected_collection else {
            return;
        };
        let data_text = self.data_file_editor.read(cx).text(cx);
        let iterations = match crate::runner::parse_data_file(&data_text) {
            Ok(iterations) => iterations,
            Err(error) => {
                self.status = RunStatus::Error(format!("Data file: {error}"));
                cx.notify();
                return;
            }
        };

        let (requests, base_environment, collection_variables, client) = {
            let store = self.store.read(cx);
            let requests = crate::runner::requests_for_collection(&store.requests, collection_id);
            let base_environment = store
                .active_environment()
                .map(|environment| {
                    environment
                        .variables
                        .iter()
                        .filter(|variable| variable.enabled)
                        .map(|variable| {
                            (variable.key.clone(), variable.value_for_send().to_string())
                        })
                        .collect()
                })
                .unwrap_or_default();
            let collection_variables = store
                .collections
                .iter()
                .find(|collection| collection.id == collection_id)
                .map(|collection| {
                    collection
                        .variables
                        .iter()
                        .filter(|variable| variable.enabled)
                        .map(|variable| {
                            (variable.key.clone(), variable.value_for_send().to_string())
                        })
                        .collect()
                })
                .unwrap_or_default();
            (
                requests,
                base_environment,
                collection_variables,
                store.http_client.clone(),
            )
        };

        if requests.is_empty() {
            self.status = RunStatus::Error("This collection has no requests to run.".to_string());
            cx.notify();
            return;
        }

        self.status = RunStatus::Running;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let results = cx
                .background_spawn(async move {
                    crate::runner::run_collection(
                        requests,
                        iterations,
                        base_environment,
                        collection_variables,
                        client,
                    )
                    .await
                })
                .await;
            this.update(cx, |this, cx| {
                this.status = RunStatus::Done(results);
                cx.notify();
            })
            .log_err();
        })
        .detach();
    }
}

impl Focusable for RunnerView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for RunnerView {}

impl Item for RunnerView {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Collection Runner".into()
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(ItemEvent)) {}
}

impl Render for RunnerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().colors().border;
        let collections = self.store.read(cx).collections.clone();
        let selected_collection = self.selected_collection;

        let mut collection_row = h_flex().gap_2();
        for collection in &collections {
            let is_selected = selected_collection == Some(collection.id);
            let collection_id = collection.id;
            collection_row =
                collection_row.child(
                    div()
                        .id(SharedString::from(format!(
                            "runner-collection-{collection_id}"
                        )))
                        .debug_selector(move || format!("runner-collection-{collection_id}"))
                        .px_2()
                        .py_0p5()
                        .rounded_md()
                        .cursor_pointer()
                        .when(is_selected, |el| {
                            el.bg(cx.theme().colors().element_selected)
                        })
                        .when(!is_selected, |el| {
                            el.hover(|el| el.bg(cx.theme().colors().element_hover))
                        })
                        .child(Label::new(collection.name.clone()).size(LabelSize::Small))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.select_collection(collection_id, cx)
                        })),
                );
        }

        let data_file_section = v_flex()
            .gap_1()
            .child(
                Label::new("Iteration Data (optional)")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .min_h(px(100.))
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .border_1()
                    .border_color(border)
                    .child(self.data_file_editor.clone()),
            );

        let run_button = div()
            .id("runner-run-hitbox")
            .debug_selector(|| "runner-run".to_string())
            .child(
                Button::new("runner-run", "Run Collection")
                    .style(cyberpunk::Rank::Accent.style())
                    .on_click(cx.listener(|this, _, window, cx| this.run(window, cx))),
            );

        let status_section: AnyElement = match &self.status {
            RunStatus::Idle => div().into_any_element(),
            RunStatus::Running => Label::new("Running...")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element(),
            RunStatus::Error(message) => Label::new(message.clone())
                .size(LabelSize::Small)
                .color(Color::Error)
                .into_any_element(),
            RunStatus::Done(results) => {
                let summary = crate::runner::summarize_run(results);
                let mut list = v_flex()
                    .id("runner-results-list")
                    .gap_1()
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .track_scroll(&self.results_scroll_handle)
                    .child(Label::new(summary).size(LabelSize::Small));
                for (index, row) in results.iter().enumerate() {
                    let (status_icon, status_color, status_text) = match (&row.error, row.status) {
                        (Some(error), _) => (IconName::XCircle, Color::Error, error.clone()),
                        (None, Some(status)) => {
                            if (200..300).contains(&status) {
                                (IconName::Check, Color::Success, format!("{status}"))
                            } else {
                                (IconName::Warning, Color::Warning, format!("{status}"))
                            }
                        }
                        (None, None) => (IconName::Dash, Color::Muted, "no response".to_string()),
                    };
                    list = list.child(
                        h_flex()
                            .id(SharedString::from(format!("runner-row-{index}")))
                            .debug_selector(move || format!("runner-row-{index}"))
                            .gap_2()
                            .child(
                                Label::new(format!("#{}", row.iteration_index + 1))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(Label::new(row.request_name.clone()).size(LabelSize::Small))
                            .child(
                                h_flex()
                                    .gap_1()
                                    .child(
                                        Icon::new(status_icon)
                                            .size(IconSize::Small)
                                            .color(status_color),
                                    )
                                    .child(
                                        Label::new(status_text)
                                            .size(LabelSize::Small)
                                            .color(status_color),
                                    ),
                            )
                            .child(
                                Label::new(format!(
                                    "{} passed / {} failed",
                                    row.passed_tests, row.failed_tests
                                ))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                            ),
                    );
                }
                list.custom_scrollbars(
                    Scrollbars::always_visible(ScrollAxes::Vertical)
                        .tracked_scroll_handle(&self.results_scroll_handle),
                    window,
                    cx,
                )
                .into_any_element()
            }
        };

        v_flex()
            .size_full()
            .p_3()
            .gap_2()
            .track_focus(&self.focus_handle)
            .child(
                Label::new("Collection")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(collection_row)
            .child(data_file_section)
            .child(run_button)
            .child(status_section)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ApiClientStore;
    use gpui::{TestAppContext, VisualTestContext};

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    async fn build_runner_view(
        cx: &mut TestAppContext,
    ) -> (
        Entity<ApiClientStore>,
        Entity<RunnerView>,
        VisualTestContext,
    ) {
        init_test(cx);
        let store = cx.new(|cx| ApiClientStore::new(cx));
        let window = cx.add_window({
            let store = store.clone();
            move |window, cx| RunnerView::new(store, window, cx)
        });
        let mut cx = VisualTestContext::from_window(window.into(), cx);
        let view = window.root(&mut cx).unwrap();
        (store, view, cx)
    }

    fn debug_center(
        cx: &mut VisualTestContext,
        selector: &'static str,
    ) -> gpui::Point<gpui::Pixels> {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("expected debug bounds for {selector}"))
            .center()
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn a_collection_with_no_requests_reports_an_error_rather_than_hanging(
        cx: &mut TestAppContext,
    ) {
        let (store, view, mut cx) = build_runner_view(cx).await;
        let collection_id = store.update(&mut cx, |store, cx| {
            store.create_collection("Empty".into(), cx)
        });
        view.update(&mut cx, |view, cx| {
            view.selected_collection = Some(collection_id);
            cx.notify();
        });
        draw(&mut cx);

        let run_button = debug_center(&mut cx, "runner-run");
        cx.simulate_click(run_button, gpui::Modifiers::none());
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert!(
                matches!(&view.status, RunStatus::Error(message) if message.contains("no requests"))
            );
        });
    }

    #[gpui::test]
    async fn a_malformed_data_file_reports_an_error_without_starting_a_run(
        cx: &mut TestAppContext,
    ) {
        let (store, view, mut cx) = build_runner_view(cx).await;
        let collection_id =
            store.update(&mut cx, |store, cx| store.create_collection("A".into(), cx));
        store.update(&mut cx, |store, cx| {
            store.create_request(collection_id, "Ping".into(), None, cx);
        });
        view.update(&mut cx, |view, cx| {
            view.selected_collection = Some(collection_id);
            cx.notify();
        });
        draw(&mut cx);

        let data_editor = view.read_with(&cx, |view, _| view.data_file_editor.clone());
        view.update_in(&mut cx, |_, window, cx| {
            data_editor.update(cx, |editor, cx| editor.set_text("[{not json", window, cx));
        });

        let run_button = debug_center(&mut cx, "runner-run");
        cx.simulate_click(run_button, gpui::Modifiers::none());
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert!(matches!(&view.status, RunStatus::Error(message) if message.starts_with("Data file:")));
        });
    }

    #[gpui::test]
    async fn clicking_a_collection_chip_selects_it(cx: &mut TestAppContext) {
        let (store, view, mut cx) = build_runner_view(cx).await;
        let collection_id = store.update(&mut cx, |store, cx| {
            store.create_collection("Selectable".into(), cx)
        });
        draw(&mut cx);

        let selector: &'static str =
            Box::leak(format!("runner-collection-{collection_id}").into_boxed_str());
        let chip = debug_center(&mut cx, selector);
        cx.simulate_click(chip, gpui::Modifiers::none());
        cx.run_until_parked();

        view.read_with(&cx, |view, _| {
            assert_eq!(view.selected_collection, Some(collection_id))
        });
    }

    #[gpui::test]
    async fn a_long_result_list_overflows_the_viewport_and_becomes_scrollable(
        cx: &mut TestAppContext,
    ) {
        let (_store, view, mut cx) = build_runner_view(cx).await;
        let results: Vec<RunnerRowResult> = (0..50)
            .map(|index| RunnerRowResult {
                request_name: format!("Request {index}"),
                iteration_index: 0,
                status: Some(200),
                passed_tests: 0,
                failed_tests: 0,
                error: None,
            })
            .collect();
        view.update(&mut cx, |view, cx| {
            view.status = RunStatus::Done(results);
            cx.notify();
        });
        draw(&mut cx);

        view.read_with(&cx, |view, _| {
            assert!(
                view.results_scroll_handle.max_offset().y > gpui::px(0.),
                "50 result rows must overflow the viewport and produce a scrollable range"
            );
        });
    }
}
