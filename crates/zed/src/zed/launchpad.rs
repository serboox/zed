use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use db::kvp::KeyValueStore;
use editor::{Editor, EditorEvent};
use fuzzy_nucleo::{StringMatchCandidate, match_strings};
use gpui::{
    Action, AnyElement, App, Bounds, Context, Entity, FocusHandle, Focusable, PathPromptOptions,
    Pixels, ScrollHandle, Size, Subscription, Task, Window, WindowBounds, WindowHandle, point, px,
    size,
};
use platform_title_bar::PlatformTitleBar;
use recent_projects::{RecentProjectEntry, get_recent_projects};
use serde::{Deserialize, Serialize};
use theme::SystemAppearance;
use theme_settings::setup_ui_font;
use ui::{ButtonLike, KeyBinding, ListItem, ListItemSpacing, WithScrollbar, prelude::*};
use util::{ResultExt, paths::PathExt};
use uuid::Uuid;
use workspace::{
    AppState, OpenOptions, WorkspaceDb, client_side_decorations, open_new, open_paths,
};
use zed_actions::editor::{MoveDown, MoveUp};

use super::build_window_options;

/// How much of the display the launchpad takes when the reader has not placed it
/// themselves.
const WIDTH_OF_DISPLAY: f32 = 0.5;
const HEIGHT_OF_DISPLAY: f32 = 0.55;

/// The proportions alone are not enough at either extreme: an ultrawide display
/// would give a launcher over two thousand points across, where a project row
/// stretches its path halfway across the screen and reads as nothing, and a small
/// display would give one too cramped to read a path in at all.
const MIN_WIDTH: Pixels = px(640.);
const MAX_WIDTH: Pixels = px(900.);
const MIN_HEIGHT: Pixels = px(460.);
const MAX_HEIGHT: Pixels = px(720.);

/// Below this the launchpad is no longer worth opening as a window.
const MIN_WINDOW_SIZE: Size<Pixels> = size(px(420.), px(320.));

/// Height of one project row: the folder name with its path beneath.
const PROJECT_ROW_HEIGHT: Pixels = px(48.);

/// How many rows the list shows before it scrolls. Past this the filter field is
/// the way to find a project, not the scrollbar.
const ROWS_BEFORE_SCROLLING: usize = 8;

/// How many recent projects are read from the database. Every one of them is
/// checked against the file system first, so this is also a bound on that work.
const RECENT_PROJECTS_READ: usize = 100;

const BOUNDS_KEY: &str = "launchpad_window_bounds";

/// Where the launchpad was when the reader last left it. Kept apart from the
/// editor's own remembered bounds so the two windows do not overwrite each
/// other's place.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct StoredBounds {
    display: Uuid,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl StoredBounds {
    fn new(display: Uuid, bounds: Bounds<Pixels>) -> Self {
        Self {
            display,
            x: bounds.origin.x.into(),
            y: bounds.origin.y.into(),
            width: bounds.size.width.into(),
            height: bounds.size.height.into(),
        }
    }

    fn bounds(&self) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(self.x), px(self.y)),
            size: size(px(self.width), px(self.height)),
        }
    }
}

/// A display as the launchpad's placement needs it: what identifies it, and the
/// area it leaves free of the taskbar and the dock.
#[derive(Clone, Copy, Debug)]
struct DisplayShape {
    uuid: Option<Uuid>,
    visible: Bounds<Pixels>,
}

/// Keeps `wanted` within the limits, and within `available` above all: a display
/// smaller than the lower limit must not be given a window wider than itself,
/// which would open with its edges off screen.
fn fit(wanted: Pixels, min: Pixels, max: Pixels, available: Pixels) -> Pixels {
    wanted.clamp(min, max).min(available)
}

/// The size the launchpad opens at on a display with `visible` free.
fn size_for_display(visible: Size<Pixels>) -> Size<Pixels> {
    size(
        fit(
            visible.width * WIDTH_OF_DISPLAY,
            MIN_WIDTH,
            MAX_WIDTH,
            visible.width,
        ),
        fit(
            visible.height * HEIGHT_OF_DISPLAY,
            MIN_HEIGHT,
            MAX_HEIGHT,
            visible.height,
        ),
    )
}

fn fits_inside(inner: Bounds<Pixels>, outer: Bounds<Pixels>) -> bool {
    inner.origin.x >= outer.origin.x
        && inner.origin.y >= outer.origin.y
        && inner.right() <= outer.right()
        && inner.bottom() <= outer.bottom()
}

/// A place a window can actually be opened at. The numbers come back from a
/// file the reader can edit and from a display that may have changed since they
/// were written, so a rectangle that is empty, inside out or not a number at all
/// has to be turned away before it reaches the platform.
fn is_openable(bounds: Bounds<Pixels>) -> bool {
    [
        bounds.origin.x,
        bounds.origin.y,
        bounds.size.width,
        bounds.size.height,
    ]
    .into_iter()
    .all(|measure| f32::from(measure).is_finite())
        && bounds.size.width >= MIN_WINDOW_SIZE.width
        && bounds.size.height >= MIN_WINDOW_SIZE.height
}

/// Where the launchpad opens: where the reader last left it, or centred on the
/// leading display at a proportion of it. `displays` leads with the primary one.
/// A remembered place is refused when the display it names is gone, or when it
/// no longer fits inside that display, which is how a window ends up off screen
/// with no way back. `None` leaves the placement to the platform.
fn opening_bounds(
    stored: Option<StoredBounds>,
    displays: &[DisplayShape],
) -> Option<Bounds<Pixels>> {
    if let Some(stored) = stored
        && let Some(display) = displays
            .iter()
            .find(|display| display.uuid == Some(stored.display))
    {
        let bounds = stored.bounds();
        if is_openable(bounds) && fits_inside(bounds, display.visible) {
            return Some(bounds);
        }
    }

    let display = displays.first()?;
    let sized = size_for_display(display.visible.size);
    let centred = Bounds::centered_at(display.visible.center(), sized);
    // A display too small to hold even the smallest window the launchpad opens
    // is no place to put one: naming bounds under the window's own minimum
    // leaves the platform to reconcile two answers that disagree.
    is_openable(centred).then_some(centred)
}

fn display_shapes(cx: &App) -> Vec<DisplayShape> {
    let primary = cx.primary_display().and_then(|display| display.uuid().ok());
    let mut shapes: Vec<DisplayShape> = cx
        .displays()
        .into_iter()
        .map(|display| DisplayShape {
            uuid: display.uuid().ok(),
            visible: display.visible_bounds(),
        })
        .collect();
    // The primary display leads, so a launchpad with nothing remembered opens
    // where the reader is already looking.
    if let Some(primary) = primary
        && let Some(at) = shapes.iter().position(|shape| shape.uuid == Some(primary))
    {
        shapes.swap(0, at);
    }
    shapes
}

fn stored_bounds(cx: &App) -> Option<StoredBounds> {
    let json = KeyValueStore::global(cx)
        .read_kvp(BOUNDS_KEY)
        .log_err()
        .flatten()?;
    serde_json::from_str(&json).log_err()
}

/// One recent project as the launchpad draws and filters it.
struct Project {
    entry: RecentProjectEntry,
    /// One line whatever the project's root count: `full_path` separates several
    /// roots with newlines, which a label would not lay out.
    path: SharedString,
    /// Name and path together, so filtering finds a project by either.
    haystack: String,
}

impl Project {
    fn new(entry: RecentProjectEntry) -> Self {
        let path = entry
            .full_path
            .split('\n')
            .map(|path| PathBuf::from(path).compact().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(", ");
        let haystack = format!("{} {}", entry.name, path);
        Self {
            entry,
            path: path.into(),
            haystack,
        }
    }
}

/// The window a launch with nothing to reopen leaves the reader looking at: the
/// projects they had open before, and the three other ways into the editor.
pub struct Launchpad {
    app_state: Arc<AppState>,
    title_bar: Option<Entity<PlatformTitleBar>>,
    focus_handle: FocusHandle,
    filter: Entity<Editor>,
    /// Every recent project, newest first, in the order `get_recent_projects`
    /// returned them.
    projects: Vec<Project>,
    /// Indices into `projects` that the filter admits, in the order drawn.
    matches: Vec<usize>,
    selected: usize,
    scroll_handle: ScrollHandle,
    bounds_save_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Launchpad {
    /// Reads the recent projects, then opens the window with them already in it,
    /// so the list does not appear a frame after the window it belongs to. Gives
    /// back no window when the launch turned out not to need one.
    pub fn open(
        app_state: Arc<AppState>,
        cx: &mut App,
    ) -> Task<Result<Option<WindowHandle<Launchpad>>>> {
        let db = WorkspaceDb::global(cx);
        let fs = app_state.fs.clone();
        cx.spawn(async move |cx| {
            let recents = get_recent_projects(None, Some(RECENT_PROJECTS_READ), fs, &db).await;
            cx.update(|cx| {
                // Reading the history takes long enough for a path from the
                // command line, a URL or a reopened workspace to have answered
                // the launch meanwhile. The caller's check happened before that
                // wait, so it cannot stand in for this one.
                if !cx.windows().is_empty() {
                    return Ok(None);
                }
                Self::open_window(recents, app_state, cx).map(Some)
            })
        })
    }

    fn open_window(
        recents: Vec<RecentProjectEntry>,
        app_state: Arc<AppState>,
        cx: &mut App,
    ) -> Result<WindowHandle<Launchpad>> {
        // Everything that is not about size and placement -- decorations, the app
        // id, the icon, whether windows tab -- comes from the editor's own window
        // options, so the launchpad is the same kind of window as the editor.
        let mut options = build_window_options(None, cx);
        options.window_bounds =
            opening_bounds(stored_bounds(cx), &display_shapes(cx)).map(WindowBounds::Windowed);
        options.window_min_size = Some(MIN_WINDOW_SIZE);
        options.focus = true;
        options.show = true;
        // A launcher is not an editor window, so it does not belong in the
        // editor's group of system window tabs.
        options.tabbing_identifier = None;

        let handle = cx.open_window(options, |window, cx| {
            cx.new(|cx| Launchpad::new(recents, app_state, window, cx))
        })?;
        handle
            .update(cx, |_, window, _| {
                window.set_window_title("Zed");
                window.activate_window();
            })
            .log_err();
        Ok(handle)
    }

    fn new(
        recents: Vec<RecentProjectEntry>,
        app_state: Arc<AppState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter projects…", window, cx);
            editor
        });

        let subscriptions = vec![
            cx.subscribe(&filter, |this, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::Edited { .. }) {
                    this.update_matches(cx);
                }
            }),
            cx.observe_window_bounds(window, |this, window, cx| {
                this.remember_bounds(window, cx);
            }),
            // Only a window can say which appearance it was given, and the
            // application-wide guess can differ from it. An editor window settles
            // this the same way; without it the launchpad opens light in front of
            // a dark editor.
            cx.observe_window_appearance(window, |_, window, cx| {
                *SystemAppearance::global_mut(cx) = SystemAppearance(window.appearance().into());
                theme_settings::reload_theme(cx);
                theme_settings::reload_icon_theme(cx);
            }),
        ];

        let projects: Vec<Project> = recents.into_iter().map(Project::new).collect();
        let matches = (0..projects.len()).collect();

        // Focus goes to the filter field rather than the window: typing is the
        // fastest way through a long history, and the arrow keys reach the list
        // from there anyway.
        filter.focus_handle(cx).focus(window, cx);

        Self {
            app_state,
            title_bar: (!cfg!(target_os = "macos"))
                .then(|| cx.new(|cx| PlatformTitleBar::new("launchpad-title-bar", cx))),
            focus_handle: cx.focus_handle(),
            filter,
            projects,
            matches,
            selected: 0,
            scroll_handle: ScrollHandle::new(),
            bounds_save_task: None,
            _subscriptions: subscriptions,
        }
    }

    /// True on a genuine first run: nothing has ever been opened, so there is no
    /// list to draw and no history to filter.
    fn has_no_history(&self) -> bool {
        self.projects.is_empty()
    }

    fn update_matches(&mut self, cx: &mut Context<Self>) {
        let query = self.filter.read(cx).text(cx);
        let query = query.trim();

        self.matches = if query.is_empty() {
            (0..self.projects.len()).collect()
        } else {
            let candidates: Vec<StringMatchCandidate> = self
                .projects
                .iter()
                .enumerate()
                .map(|(index, project)| StringMatchCandidate::new(index, &project.haystack))
                .collect();
            match_strings(
                &candidates,
                query,
                fuzzy_nucleo::Case::smart_if_uppercase_in(query),
                fuzzy_nucleo::LengthPenalty::On,
                RECENT_PROJECTS_READ,
            )
            .into_iter()
            .map(|hit| hit.candidate_id)
            .collect()
        };

        self.selected = 0;
        self.scroll_handle.scroll_to_item(0);
        cx.notify();
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.matches.is_empty() {
            return;
        }
        self.selected = index.min(self.matches.len() - 1);
        self.scroll_handle.scroll_to_item(self.selected);
        cx.notify();
    }

    fn select_next(&mut self, cx: &mut Context<Self>) {
        if self.matches.is_empty() {
            return;
        }
        let next = if self.selected + 1 == self.matches.len() {
            0
        } else {
            self.selected + 1
        };
        self.select(next, cx);
    }

    fn select_previous(&mut self, cx: &mut Context<Self>) {
        if self.matches.is_empty() {
            return;
        }
        let previous = self
            .selected
            .checked_sub(1)
            .unwrap_or(self.matches.len() - 1);
        self.select(previous, cx);
    }

    fn selected_project(&self) -> Option<&Project> {
        let index = *self.matches.get(self.selected)?;
        self.projects.get(index)
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(project) = self.selected_project() else {
            return;
        };
        let paths = project.entry.paths.clone();
        self.open_project(paths, window, cx);
    }

    /// Opens a project the ordinary way and only then closes the launchpad. The
    /// order matters: while the launchpad is the only window, closing it first
    /// would take the window count to zero, which the application reads as its
    /// last window closing and quits on.
    fn open_project(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        let app_state = self.app_state.clone();
        let handle = window.window_handle().downcast::<Launchpad>();
        cx.spawn(async move |_, cx| {
            // A project that failed to open leaves the launchpad up, so the
            // reader still has somewhere to go.
            cx.update(|cx| open_paths(&paths, app_state, OpenOptions::default(), cx))
                .await?;
            if let Some(handle) = handle {
                handle.update(cx, |_, window, _| window.remove_window())?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn new_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let app_state = self.app_state.clone();
        let handle = window.window_handle().downcast::<Launchpad>();
        cx.spawn(async move |_, cx| {
            cx.update(|cx| {
                open_new(
                    OpenOptions::default(),
                    app_state,
                    cx,
                    |workspace, window, cx| {
                        Editor::new_file(workspace, &Default::default(), window, cx);
                    },
                )
            })
            .await?;
            if let Some(handle) = handle {
                handle.update(cx, |_, window, _| window.remove_window())?;
            }
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    /// Asks the platform for paths and opens them. The workspace-level actions
    /// that normally do this look for an editor window to prompt from and build
    /// an empty one when there is none, which from the launchpad would leave a
    /// stray editor behind the dialog, so the two steps are taken here instead.
    fn prompt_and_open(
        &mut self,
        options: PathPromptOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let paths = cx.prompt_for_paths(options);
        cx.spawn_in(window, async move |this, cx| {
            let Some(paths) = paths
                .await
                .log_err()
                .and_then(|paths| paths.log_err())
                .flatten()
            else {
                return;
            };
            if paths.is_empty() {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                this.open_project(paths, window, cx);
            })
            .log_err();
        })
        .detach();
    }

    fn remember_bounds(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.bounds_save_task.is_some() {
            return;
        }
        // A drag delivers a bounds change per frame; one write at the end of it
        // is enough.
        self.bounds_save_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(100))
                .await;
            let stored = this
                .update_in(cx, |this, window, cx| {
                    this.bounds_save_task.take();
                    // Only a windowed launchpad has a place worth remembering; a
                    // maximized or fullscreen one would come back as a window the
                    // reader never sized.
                    let WindowBounds::Windowed(bounds) = window.inner_window_bounds() else {
                        return None;
                    };
                    let display = window.display(cx)?.uuid().ok()?;
                    Some((
                        StoredBounds::new(display, bounds),
                        KeyValueStore::global(cx),
                    ))
                })
                .ok()
                .flatten();
            let Some((stored, kvp)) = stored else {
                return;
            };
            if let Some(json) = serde_json::to_string(&stored).log_err() {
                kvp.write_kvp(BOUNDS_KEY.to_string(), json).await.log_err();
            }
        }));
    }

    fn dismiss(&mut self, window: &mut Window, _cx: &mut Context<Self>) {
        window.remove_window();
    }

    fn render_project(
        &self,
        row: usize,
        project_index: usize,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let project = self.projects.get(project_index)?;
        Some(
            ListItem::new(row)
                .toggle_state(row == self.selected)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.select(row, cx);
                    this.confirm(window, cx);
                }))
                .child(
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .child(Label::new(project.entry.name.clone()).truncate())
                        .child(
                            Label::new(project.path.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_projects(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let rows: Vec<AnyElement> = self
            .matches
            .clone()
            .into_iter()
            .enumerate()
            .filter_map(|(row, project_index)| self.render_project(row, project_index, cx))
            .collect();

        if rows.is_empty() {
            return v_flex()
                .flex_1()
                .justify_center()
                .items_center()
                .child(
                    Label::new("No project matches that.")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .into_any_element();
        }

        // A height counted in rows is what makes the list scroll rather than grow:
        // it stops at ROWS_BEFORE_SCROLLING however long the history is.
        let shown = self.matches.len().min(ROWS_BEFORE_SCROLLING);
        v_flex()
            .id("launchpad-projects")
            .debug_selector(|| "launchpad-projects".into())
            .h(PROJECT_ROW_HEIGHT * shown as f32)
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .children(rows)
            .vertical_scrollbar_for(&self.scroll_handle, window, cx)
            .into_any_element()
    }

    fn render_way_in(
        &self,
        id: &'static str,
        label: &'static str,
        action: Box<dyn Action>,
        full_width: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let keybinding = KeyBinding::for_action_in(action.as_ref(), &self.focus_handle, cx);
        ButtonLike::new(id)
            .when(full_width, |button| button.full_width())
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .justify_between()
                    .child(Label::new(label).color(Color::Muted))
                    .child(keybinding),
            )
            .on_click(cx.listener(move |_, _, window, cx| {
                window.dispatch_action(action.boxed_clone(), cx);
            }))
    }

    fn render_ways_in(&self, stacked: bool, cx: &mut Context<Self>) -> AnyElement {
        let new_file = self.render_way_in(
            "launchpad-new-file",
            "New File",
            workspace::NewFile.boxed_clone(),
            stacked,
            cx,
        );
        let open_folder = self.render_way_in(
            "launchpad-open-folder",
            "Open a Folder…",
            workspace::Open::default().boxed_clone(),
            stacked,
            cx,
        );
        let open_file = self.render_way_in(
            "launchpad-open-file",
            "Open a File…",
            workspace::OpenFiles.boxed_clone(),
            stacked,
            cx,
        );

        if stacked {
            v_flex()
                .debug_selector(|| "launchpad-ways-in".into())
                .gap_1()
                .child(new_file)
                .child(open_folder)
                .child(open_file)
                .into_any_element()
        } else {
            h_flex()
                .debug_selector(|| "launchpad-ways-in".into())
                .gap_1()
                .child(new_file)
                .child(open_folder)
                .child(open_file)
                .into_any_element()
        }
    }
}

impl Focusable for Launchpad {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.filter.focus_handle(cx)
    }
}

impl Render for Launchpad {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A window without a workspace does not otherwise pick up the reader's
        // interface font.
        let ui_font = setup_ui_font(window, cx);
        // Copied out in one go rather than held as a borrow: the sections below
        // build their own elements from `cx`, which a live borrow of the theme
        // would rule out.
        let (background, text, border, border_variant, editor_background) = {
            let colors = cx.theme().colors();
            (
                colors.background,
                colors.text,
                colors.border,
                colors.border_variant,
                colors.editor_background,
            )
        };
        let has_no_history = self.has_no_history();

        let body = if has_no_history {
            v_flex()
                .gap_3()
                .child(
                    Label::new("No projects yet. Open a folder or a file to make one.")
                        .color(Color::Muted),
                )
                .child(self.render_ways_in(true, cx))
                .into_any_element()
        } else {
            v_flex()
                .gap_2()
                .flex_1()
                .min_h_0()
                .child(
                    h_flex()
                        .w_full()
                        // A definite height: an editor has none of its own, and a
                        // row that sizes itself to it would paint a sliver.
                        .h_9()
                        .flex_none()
                        .px_2()
                        .rounded_md()
                        .border_1()
                        .border_color(border)
                        .bg(editor_background)
                        .child(div().flex_1().child(self.filter.clone())),
                )
                .child(self.render_projects(window, cx))
                .into_any_element()
        };

        client_side_decorations(
            v_flex()
                .size_full()
                .font(ui_font)
                .bg(background)
                .text_color(text)
                .children(self.title_bar.clone())
                .child(
                    v_flex()
                        .id("launchpad")
                        .debug_selector(|| "launchpad".into())
                        .key_context("Launchpad")
                        .track_focus(&self.focus_handle)
                        .size_full()
                        .p_4()
                        .gap_3()
                        // The traffic lights float over the top-left corner of a
                        // transparent titlebar, where the heading would otherwise be.
                        .when(cfg!(target_os = "macos"), |content| content.pt_8())
                        .on_action(cx.listener(|this, _: &menu::SelectNext, _, cx| {
                            this.select_next(cx);
                        }))
                        .on_action(cx.listener(|this, _: &menu::SelectPrevious, _, cx| {
                            this.select_previous(cx);
                        }))
                        // The filter field has focus, and inside an editor the arrow
                        // keys resolve to the editor's own movement rather than to
                        // menu selection, so the list has to answer both.
                        .on_action(cx.listener(|this, _: &MoveDown, _, cx| {
                            this.select_next(cx);
                        }))
                        .on_action(cx.listener(|this, _: &MoveUp, _, cx| {
                            this.select_previous(cx);
                        }))
                        .on_action(cx.listener(|this, _: &menu::SelectFirst, _, cx| {
                            this.select(0, cx);
                        }))
                        .on_action(cx.listener(|this, _: &menu::SelectLast, _, cx| {
                            let last = this.matches.len().saturating_sub(1);
                            this.select(last, cx);
                        }))
                        .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                            this.confirm(window, cx);
                        }))
                        .on_action(cx.listener(|this, _: &menu::Cancel, window, cx| {
                            this.dismiss(window, cx);
                        }))
                        // These three are bound application-wide and would other-
                        // wise reach the handlers that expect an editor window to
                        // prompt from. Answering them here keeps their keys working
                        // and their bindings on show.
                        .on_action(cx.listener(|this, _: &workspace::NewFile, window, cx| {
                            this.new_file(window, cx);
                        }))
                        .on_action(cx.listener(|this, _: &workspace::Open, window, cx| {
                            this.prompt_and_open(
                                PathPromptOptions {
                                    files: false,
                                    directories: true,
                                    multiple: true,
                                    prompt: None,
                                },
                                window,
                                cx,
                            );
                        }))
                        .on_action(cx.listener(|this, _: &workspace::OpenFiles, window, cx| {
                            this.prompt_and_open(
                                PathPromptOptions {
                                    files: true,
                                    directories: false,
                                    multiple: true,
                                    prompt: None,
                                },
                                window,
                                cx,
                            );
                        }))
                        .child(Label::new("Zed").size(LabelSize::Large))
                        .child(body)
                        .when(!has_no_history, |content| {
                            content.child(
                                v_flex()
                                    .gap_2()
                                    .pt_2()
                                    .border_t_1()
                                    .border_color(border_variant)
                                    .child(self.render_ways_in(false, cx)),
                            )
                        }),
                ),
            window,
            cx,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gpui::{TestAppContext, UpdateGlobal as _, VisualTestContext};
    use serde_json::json;
    use settings::SettingsStore;
    use workspace::{MultiWorkspace, RestoreOnStartupBehavior, WorkspaceId};

    use super::*;

    fn display(uuid: u128, visible: Bounds<Pixels>) -> DisplayShape {
        DisplayShape {
            uuid: Some(Uuid::from_u128(uuid)),
            visible,
        }
    }

    fn visible(width: f32, height: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(width), px(height)),
        }
    }

    #[test]
    fn test_size_follows_the_display_within_limits() {
        let sized = size_for_display(visible(2560., 1440.).size);
        assert_eq!(sized.width, px(1280.).min(MAX_WIDTH));
        assert_eq!(sized.height, px(1440. * HEIGHT_OF_DISPLAY).min(MAX_HEIGHT));

        // A middling display gets the proportion itself, untouched by either limit.
        let sized = size_for_display(visible(1600., 1000.).size);
        assert_eq!(sized.width, px(800.));
        assert_eq!(sized.height, px(550.));
    }

    #[test]
    fn test_size_stays_inside_a_small_display() {
        for (width, height) in [(800., 600.), (640., 480.), (320., 200.)] {
            let display = visible(width, height);
            let sized = size_for_display(display.size);
            assert!(
                sized.width <= display.size.width && sized.height <= display.size.height,
                "{width}x{height} produced {sized:?}, which does not fit the display"
            );
            assert!(sized.width <= MAX_WIDTH && sized.height <= MAX_HEIGHT);
        }
    }

    #[test]
    fn test_size_stays_inside_the_limits_on_an_ultrawide_display() {
        let sized = size_for_display(visible(5120., 1440.).size);
        assert_eq!(sized.width, MAX_WIDTH);
        assert!(sized.height <= MAX_HEIGHT);
    }

    #[test]
    fn test_opens_centred_on_the_primary_display_with_nothing_stored() {
        let displays = [display(1, visible(1920., 1080.))];
        let bounds = opening_bounds(None, &displays).expect("a display was offered");
        assert!(fits_inside(bounds, displays[0].visible));
        assert_eq!(bounds.center(), displays[0].visible.center());
    }

    #[test]
    fn test_reopens_where_the_reader_left_it() {
        let displays = [display(1, visible(1920., 1080.))];
        let left_at = Bounds {
            origin: point(px(120.), px(90.)),
            size: size(px(700.), px(500.)),
        };
        let stored = StoredBounds::new(Uuid::from_u128(1), left_at);
        assert_eq!(opening_bounds(Some(stored), &displays), Some(left_at));
    }

    #[test]
    fn test_falls_back_when_the_stored_display_is_gone() {
        let displays = [display(2, visible(1440., 900.))];
        // A place that would fit the attached display perfectly well: only the
        // display it was stored against being gone can rule it out, so the
        // identity check is the one thing this test can fail on.
        let stored = StoredBounds::new(
            Uuid::from_u128(1),
            Bounds {
                origin: point(px(100.), px(90.)),
                size: size(px(700.), px(500.)),
            },
        );
        let bounds = opening_bounds(Some(stored), &displays).expect("a display was offered");
        assert_ne!(
            bounds,
            stored.bounds(),
            "a place remembered on a display that is gone should not be reused"
        );
        assert!(
            fits_inside(bounds, displays[0].visible),
            "{bounds:?} is off the only attached display"
        );
    }

    #[test]
    fn test_falls_back_when_the_stored_place_no_longer_fits() {
        let displays = [display(1, visible(1280., 800.))];
        let stored = StoredBounds::new(
            Uuid::from_u128(1),
            Bounds {
                origin: point(px(1000.), px(600.)),
                size: size(px(700.), px(500.)),
            },
        );
        let bounds = opening_bounds(Some(stored), &displays).expect("a display was offered");
        assert!(fits_inside(bounds, displays[0].visible));
        assert_ne!(bounds, stored.bounds());
    }

    #[test]
    fn test_no_display_leaves_the_placement_to_the_platform() {
        assert_eq!(opening_bounds(None, &[]), None);
    }

    #[test]
    fn test_a_display_too_small_for_the_window_leaves_the_placement_to_the_platform() {
        for (width, height) in [(0., 0.), (300., 200.), (1920., 100.)] {
            let displays = [display(1, visible(width, height))];
            assert_eq!(
                opening_bounds(None, &displays),
                None,
                "a {width}x{height} display was given bounds under the window's own minimum"
            );
        }
    }

    #[test]
    fn test_a_remembered_place_that_is_no_place_at_all_is_refused() {
        let displays = [display(1, visible(1920., 1080.))];
        for wrong in [
            size(px(10.), px(10.)),
            size(px(-700.), px(500.)),
            size(px(f32::NAN), px(500.)),
        ] {
            let stored = StoredBounds::new(
                Uuid::from_u128(1),
                Bounds {
                    origin: point(px(120.), px(90.)),
                    size: wrong,
                },
            );
            let bounds = opening_bounds(Some(stored), &displays).expect("a display was offered");
            // Not `assert_ne!` against the stored place: a NaN never equals
            // itself, so that comparison would pass however wrong the answer is.
            assert!(
                is_openable(bounds) && fits_inside(bounds, displays[0].visible),
                "a remembered size of {wrong:?} produced {bounds:?}"
            );
        }
    }

    #[test]
    fn test_stored_bounds_survive_a_round_trip() {
        let stored = StoredBounds::new(
            Uuid::from_u128(7),
            Bounds {
                origin: point(px(12.), px(34.)),
                size: size(px(700.), px(500.)),
            },
        );
        let json = serde_json::to_string(&stored).expect("serialized");
        let read: StoredBounds = serde_json::from_str(&json).expect("deserialized");
        assert_eq!(read, stored);
    }

    fn init_launchpad_test(cx: &mut TestAppContext) -> Arc<AppState> {
        let app_state = crate::zed::tests::init_test(cx);
        cx.update(|cx| {
            // A database of this test's own, so recent projects seeded here are
            // not seen by every other test in the process.
            cx.set_global(db::AppDatabase::test_new());
            crate::zed::load_default_keymap(cx);
        });
        app_state
    }

    fn ask_for_the_launchpad(cx: &mut TestAppContext) {
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.workspace.restore_on_startup =
                        Some(RestoreOnStartupBehavior::Launchpad);
                });
            });
        });
    }

    /// Marks the installation as one that has been launched before, so a launch
    /// with nothing to restore reaches the launchpad rather than onboarding.
    async fn already_launched_once(cx: &mut TestAppContext) {
        let kvp = cx.update(|cx| KeyValueStore::global(cx));
        kvp.write_kvp(onboarding::FIRST_OPEN.to_string(), "false".to_string())
            .await
            .expect("recording a previous launch");
    }

    fn recent(name: &str, path: &str, id: i64) -> RecentProjectEntry {
        RecentProjectEntry {
            name: name.to_string().into(),
            full_path: path.to_string().into(),
            paths: vec![PathBuf::from(path)],
            workspace_id: WorkspaceId::from_i64(id),
            timestamp: chrono::Utc::now() - chrono::Duration::seconds(id),
        }
    }

    fn open_launchpad_with(
        recents: Vec<RecentProjectEntry>,
        app_state: Arc<AppState>,
        cx: &mut TestAppContext,
    ) -> WindowHandle<Launchpad> {
        cx.update(|cx| Launchpad::open_window(recents, app_state, cx))
            .expect("the launchpad window opened")
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.draw(cx).clear());
    }

    fn launchpad_windows(cx: &mut TestAppContext) -> Vec<WindowHandle<Launchpad>> {
        cx.update(|cx| {
            cx.windows()
                .into_iter()
                .filter_map(|window| window.downcast::<Launchpad>())
                .collect()
        })
    }

    fn project_roots(cx: &mut TestAppContext) -> Vec<PathBuf> {
        cx.update(|cx| {
            cx.windows()
                .into_iter()
                .filter_map(|window| window.downcast::<MultiWorkspace>())
                .flat_map(|window| {
                    window
                        .read_with(cx, |multi_workspace, cx| {
                            multi_workspace
                                .workspace()
                                .read(cx)
                                .project()
                                .read(cx)
                                .visible_worktrees(cx)
                                .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
    }

    fn drawn_names(window: WindowHandle<Launchpad>, cx: &mut TestAppContext) -> Vec<String> {
        cx.update(|cx| {
            window
                .read(cx)
                .map(|launchpad| {
                    launchpad
                        .matches
                        .iter()
                        .filter_map(|index| launchpad.projects.get(*index))
                        .map(|project| project.entry.name.to_string())
                        .collect()
                })
                .expect("the launchpad is still open")
        })
    }

    fn selected_name(window: WindowHandle<Launchpad>, cx: &mut TestAppContext) -> Option<String> {
        cx.update(|cx| {
            window
                .read(cx)
                .expect("the launchpad is still open")
                .selected_project()
                .map(|project| project.entry.name.to_string())
        })
    }

    fn is_closed(window: WindowHandle<Launchpad>, cx: &mut TestAppContext) -> bool {
        cx.update(|cx| window.read(cx).is_err())
    }

    /// Records whether the application was ever left with no window at all, which
    /// is what `bind_on_window_closed` reads as its last window closing and quits
    /// on. `TestPlatform::quit` does nothing, so the subscription the application
    /// installs is mirrored here rather than observed.
    fn watch_for_no_windows_left(cx: &mut TestAppContext) -> Rc<Cell<bool>> {
        let left_with_nothing = Rc::new(Cell::new(false));
        cx.update(|cx| {
            let left_with_nothing = left_with_nothing.clone();
            cx.on_window_closed(move |cx, _| {
                if cx.windows().is_empty() {
                    left_with_nothing.set(true);
                }
            })
            .detach();
        });
        left_with_nothing
    }

    #[gpui::test]
    async fn test_launchpad_opens_when_a_launch_has_nothing_to_restore(cx: &mut TestAppContext) {
        let app_state = init_launchpad_test(cx);
        ask_for_the_launchpad(cx);
        already_launched_once(cx).await;

        crate::restore_or_create_workspace(app_state, &mut cx.to_async())
            .await
            .expect("the launch completed");
        cx.run_until_parked();

        assert_eq!(launchpad_windows(cx).len(), 1);
        assert_eq!(
            cx.update(|cx| cx.windows().len()),
            1,
            "the launchpad should be the only window, with no editor behind it"
        );
    }

    #[gpui::test]
    async fn test_launchpad_stays_away_when_a_path_was_opened(cx: &mut TestAppContext) {
        let app_state = init_launchpad_test(cx);
        ask_for_the_launchpad(cx);
        already_launched_once(cx).await;

        app_state
            .fs
            .as_fake()
            .insert_tree("/from-the-command-line", json!({ "a.txt": "" }))
            .await;
        cx.update(|cx| {
            open_paths(
                &[PathBuf::from("/from-the-command-line")],
                app_state.clone(),
                OpenOptions::default(),
                cx,
            )
        })
        .await
        .expect("the project opened");
        cx.run_until_parked();

        crate::restore_or_create_workspace(app_state, &mut cx.to_async())
            .await
            .expect("the launch completed");
        cx.run_until_parked();

        assert!(
            launchpad_windows(cx).is_empty(),
            "a launch that already opened a path has nothing for the launchpad to do"
        );
        assert_eq!(
            cx.update(|cx| cx.windows().len()),
            1,
            "the launch added a window to the one that had already answered it"
        );
    }

    #[gpui::test]
    async fn test_the_launchpad_takes_its_appearance_from_the_window(cx: &mut TestAppContext) {
        let app_state = init_launchpad_test(cx);
        // The application-wide guess left disagreeing with what a window reports,
        // which is the state an editor window corrects for itself on open.
        let reported = cx.update(|cx| theme::Appearance::from(cx.window_appearance()));
        let disagreeing = match reported {
            theme::Appearance::Light => theme::Appearance::Dark,
            theme::Appearance::Dark => theme::Appearance::Light,
        };
        cx.update(|cx| *SystemAppearance::global_mut(cx) = SystemAppearance(disagreeing));

        open_launchpad_with(vec![recent("zed", "/projects/zed", 1)], app_state, cx);
        cx.run_until_parked();

        assert_eq!(
            cx.update(|cx| SystemAppearance::global(cx).0),
            reported,
            "the launchpad kept a guess its own window disagrees with"
        );
    }

    #[gpui::test]
    async fn test_the_launchpad_opens_no_window_when_one_is_already_up(cx: &mut TestAppContext) {
        let app_state = init_launchpad_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree("/answered-the-launch", json!({ "a.txt": "" }))
            .await;
        cx.update(|cx| {
            open_paths(
                &[PathBuf::from("/answered-the-launch")],
                app_state.clone(),
                OpenOptions::default(),
                cx,
            )
        })
        .await
        .expect("the project opened");
        cx.run_until_parked();

        let opened = cx
            .update(|cx| Launchpad::open(app_state, cx))
            .await
            .expect("the launch completed");
        cx.run_until_parked();

        assert!(
            opened.is_none(),
            "reading the history is long enough for a window to appear, and one had"
        );
        assert!(launchpad_windows(cx).is_empty());
    }

    #[gpui::test]
    async fn test_launchpad_body_paints_real_area_inside_the_window(cx: &mut TestAppContext) {
        let app_state = init_launchpad_test(cx);
        let window = open_launchpad_with(
            vec![
                recent("zed", "/projects/zed", 1),
                recent("shell", "/projects/shell", 2),
            ],
            app_state,
            cx,
        );
        cx.run_until_parked();

        let mut visual = VisualTestContext::from_window(window.into(), cx);
        draw(&mut visual);
        let viewport = visual.update(|window, _| window.viewport_size());
        let list = visual
            .debug_bounds("launchpad-projects")
            .expect("the project list was painted");
        let root = visual
            .debug_bounds("launchpad")
            .expect("the launchpad was painted");

        assert!(
            list.size.width > px(0.) && list.size.height > px(0.),
            "the project list painted {:?}, which is no area at all",
            list.size
        );
        assert!(
            fits_inside(list, root),
            "the project list painted {list:?}, outside the {root:?} launchpad"
        );
        assert!(
            root.size.width <= viewport.width && root.size.height <= viewport.height,
            "the launchpad painted {:?}, larger than its {viewport:?} window",
            root.size
        );
        assert!(
            viewport.width <= MAX_WIDTH && viewport.height <= MAX_HEIGHT,
            "the launchpad window is {viewport:?}, past the limits it should keep to"
        );
    }

    #[gpui::test]
    async fn test_recent_projects_are_listed_newest_first(cx: &mut TestAppContext) {
        let app_state = init_launchpad_test(cx);
        // `get_recent_projects` reads them newest first; the launchpad must keep
        // the order it was handed.
        let window = open_launchpad_with(
            vec![
                recent("newest", "/projects/newest", 1),
                recent("middle", "/projects/middle", 2),
                recent("oldest", "/projects/oldest", 3),
            ],
            app_state,
            cx,
        );
        cx.run_until_parked();

        assert_eq!(drawn_names(window, cx), ["newest", "middle", "oldest"]);
    }

    #[gpui::test]
    async fn test_recent_projects_come_from_the_database(cx: &mut TestAppContext) {
        let app_state = init_launchpad_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree("/projects/remembered", json!({ "a.txt": "" }))
            .await;

        // Recent projects are whatever the database was told about, so the only
        // honest way to seed one is to open it and let it serialize itself.
        cx.update(|cx| {
            open_paths(
                &[PathBuf::from("/projects/remembered")],
                app_state.clone(),
                OpenOptions::default(),
                cx,
            )
        })
        .await
        .expect("the project opened");
        cx.run_until_parked();

        let editor_window = cx
            .update(|cx| cx.windows().into_iter().next())
            .and_then(|window| window.downcast::<MultiWorkspace>())
            .expect("an editor window");
        crate::zed::tests::flush_workspace_serialization(&editor_window, cx).await;
        editor_window
            .update(cx, |_, window, _| window.remove_window())
            .expect("closing the editor window");
        cx.run_until_parked();

        let window = cx
            .update(|cx| Launchpad::open(app_state, cx))
            .await
            .expect("the launchpad opened")
            .expect("a window was opened");
        cx.run_until_parked();

        assert_eq!(drawn_names(window, cx), ["remembered"]);
    }

    #[gpui::test]
    async fn test_confirming_a_recent_project_opens_it_and_closes_the_launchpad(
        cx: &mut TestAppContext,
    ) {
        let app_state = init_launchpad_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree("/projects/chosen", json!({ "a.txt": "" }))
            .await;
        let left_with_nothing = watch_for_no_windows_left(cx);

        let window =
            open_launchpad_with(vec![recent("chosen", "/projects/chosen", 1)], app_state, cx);
        cx.run_until_parked();

        let mut visual = VisualTestContext::from_window(window.into(), cx);
        draw(&mut visual);
        visual.simulate_keystrokes("enter");
        cx.run_until_parked();

        assert_eq!(project_roots(cx), [PathBuf::from("/projects/chosen")]);
        assert!(
            is_closed(window, cx),
            "the launchpad should be gone once the project is open"
        );
        assert!(
            !left_with_nothing.get(),
            "the launchpad closed before the project window existed, which quits the application"
        );
    }

    #[gpui::test]
    async fn test_first_run_offers_the_three_ways_in_and_draws_no_list(cx: &mut TestAppContext) {
        let app_state = init_launchpad_test(cx);
        let window = open_launchpad_with(Vec::new(), app_state, cx);
        cx.run_until_parked();

        let mut visual = VisualTestContext::from_window(window.into(), cx);
        draw(&mut visual);

        let ways_in = visual
            .debug_bounds("launchpad-ways-in")
            .expect("the three ways in were painted");
        assert!(ways_in.size.width > px(0.) && ways_in.size.height > px(0.));
        assert!(
            visual.debug_bounds("launchpad-projects").is_none(),
            "a first run has no projects, so it should draw no list to scroll"
        );
    }

    #[gpui::test]
    async fn test_arrows_and_enter_move_through_the_list_as_real_input(cx: &mut TestAppContext) {
        let app_state = init_launchpad_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree("/projects/second", json!({ "a.txt": "" }))
            .await;

        let window = open_launchpad_with(
            vec![
                recent("first", "/projects/first", 1),
                recent("second", "/projects/second", 2),
                recent("third", "/projects/third", 3),
            ],
            app_state,
            cx,
        );
        cx.run_until_parked();
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        draw(&mut visual);

        assert_eq!(selected_name(window, cx).as_deref(), Some("first"));
        visual.simulate_keystrokes("down down");
        cx.run_until_parked();
        assert_eq!(selected_name(window, cx).as_deref(), Some("third"));
        visual.simulate_keystrokes("up");
        cx.run_until_parked();
        assert_eq!(selected_name(window, cx).as_deref(), Some("second"));

        visual.simulate_keystrokes("enter");
        cx.run_until_parked();

        assert_eq!(
            project_roots(cx),
            [PathBuf::from("/projects/second")],
            "enter should open whichever project the arrows left highlighted"
        );
    }

    #[gpui::test]
    async fn test_escape_closes_the_launchpad_and_leaves_no_window(cx: &mut TestAppContext) {
        let app_state = init_launchpad_test(cx);
        let left_with_nothing = watch_for_no_windows_left(cx);
        let window = open_launchpad_with(vec![recent("zed", "/projects/zed", 1)], app_state, cx);
        cx.run_until_parked();

        let mut visual = VisualTestContext::from_window(window.into(), cx);
        draw(&mut visual);
        visual.simulate_keystrokes("escape");
        cx.run_until_parked();

        assert!(is_closed(window, cx), "escape should close the launchpad");
        assert!(
            cx.update(|cx| cx.windows().is_empty()),
            "dismissing the launchpad should leave no window behind"
        );
        assert!(
            left_with_nothing.get(),
            "the application must see its last window close so it can quit"
        );
    }

    #[gpui::test]
    async fn test_filtering_narrows_the_list(cx: &mut TestAppContext) {
        let app_state = init_launchpad_test(cx);
        let window = open_launchpad_with(
            vec![
                recent("zed", "/projects/zed", 1),
                recent("shell", "/work/shell", 2),
            ],
            app_state,
            cx,
        );
        cx.run_until_parked();
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        draw(&mut visual);

        let both = visual
            .debug_bounds("launchpad-projects")
            .expect("the project list was painted");
        assert_eq!(both.size.height, PROJECT_ROW_HEIGHT * 2.);

        visual.simulate_input("shell");
        cx.run_until_parked();
        assert_eq!(drawn_names(window, cx), ["shell"]);

        // The list is one row shorter on screen, not merely one entry shorter in
        // the state behind it.
        draw(&mut visual);
        let narrowed = visual
            .debug_bounds("launchpad-projects")
            .expect("the project list was painted");
        assert_eq!(narrowed.size.height, PROJECT_ROW_HEIGHT);

        // The path is part of what the filter reads, not only the folder name.
        window
            .update(cx, |launchpad, window, cx| {
                launchpad
                    .filter
                    .update(cx, |filter, cx| filter.set_text("work", window, cx));
            })
            .expect("the launchpad is still open");
        cx.run_until_parked();
        assert_eq!(drawn_names(window, cx), ["shell"]);
    }
}
