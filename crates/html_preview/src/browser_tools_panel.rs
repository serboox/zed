use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use editor::Editor;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task, WeakEntity,
    Window, actions,
};
use serde::Deserialize;
use ui::prelude::*;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

use crate::html_preview_view::HtmlPreviewView;

actions!(
    browser_tools,
    [
        /// Shows or hides the developer tools for the page being read.
        ToggleFocus
    ]
);

/// How often the page is asked what its scripts have said and what it fetched.
/// Only while the panel is showing, and only for the tab that is open.
const ASK_AGAIN: Duration = Duration::from_millis(400);

const PANEL_KEY: &str = "BrowserToolsPanel";

/// Which of the three the reader is looking at.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tools {
    Elements,
    Console,
    Network,
}

impl Tools {
    const ALL: [Tools; 3] = [Self::Elements, Self::Console, Self::Network];

    fn label(self) -> &'static str {
        match self {
            Self::Elements => "Elements",
            Self::Console => "Console",
            Self::Network => "Network",
        }
    }
}

#[derive(Deserialize)]
struct Row {
    at: usize,
    depth: usize,
    text: String,
    children: usize,
}

#[derive(Deserialize)]
struct Said {
    level: String,
    text: String,
}

#[derive(Deserialize)]
struct Fetched {
    name: String,
    kind: String,
    ms: u64,
    size: u64,
}

/// What the page has answered, left here because the answers arrive on the
/// engine's own turn, where there is no context to hand.
#[derive(Default)]
struct Answers {
    tree: Option<String>,
    said: Option<String>,
    fetched: Option<String>,
    about: Option<String>,
    ran: Option<String>,
}

/// The developer's tools for a live page, in the dock beside the terminal.
///
/// It asks the page about itself rather than speaking a debugging protocol: the
/// page already carries a little script of ours, and everything here -- the tree
/// it is made of, what its scripts have said, what it fetched, what one element
/// is -- is something the page can be asked directly. Nothing listens on a port.
pub struct BrowserToolsPanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    position: DockPosition,
    showing: Tools,
    /// Where the reader types something for the page to run.
    console: Entity<Editor>,
    said: Vec<Said>,
    rows: Vec<Row>,
    fetched: Vec<Fetched>,
    /// The element the reader has picked, and what the page says about it.
    picked: Option<usize>,
    about: Option<String>,
    answers: Rc<RefCell<Answers>>,
    _collector: Task<()>,
    _console_events: Subscription,
}

impl BrowserToolsPanel {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let console = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text("Run something in the page", window, cx);
                editor
            });
            let console_events =
                cx.subscribe(&console, |_: &mut Self, _, _: &editor::EditorEvent, cx| {
                    cx.notify();
                });
            let answers: Rc<RefCell<Answers>> = Rc::default();
            // The page answers on a turn of the engine's own, so the answers are
            // left in a box and picked up from here.
            let collector = cx.spawn({
                let answers = answers.clone();
                async move |panel, cx| {
                    loop {
                        cx.background_executor().timer(ASK_AGAIN).await;
                        let anything = {
                            let answers = answers.borrow();
                            answers.tree.is_some()
                                || answers.said.is_some()
                                || answers.fetched.is_some()
                                || answers.about.is_some()
                                || answers.ran.is_some()
                        };
                        let carried = panel
                            .update(cx, |panel, cx| {
                                if anything {
                                    panel.take_answers(cx);
                                }
                                panel.ask_the_page(cx);
                            })
                            .is_ok();
                        if !carried {
                            return;
                        }
                    }
                }
            });
            Self {
                focus_handle: cx.focus_handle(),
                workspace,
                position: DockPosition::Bottom,
                showing: Tools::Console,
                console,
                said: Vec::new(),
                rows: Vec::new(),
                fetched: Vec::new(),
                picked: None,
                about: None,
                answers,
                _collector: collector,
                _console_events: console_events,
            }
        })
    }

    /// The page being read, if the reader is reading one.
    fn page(&self, cx: &App) -> Option<Entity<HtmlPreviewView>> {
        let workspace = self.workspace.upgrade()?;
        let workspace = workspace.read(cx);
        workspace
            .active_item(cx)
            .and_then(|item| item.downcast::<HtmlPreviewView>())
    }

    /// Asks the page whatever the open tab needs. Every answer is a script the
    /// page runs, so only the tab in front of the reader asks for anything.
    fn ask_the_page(&mut self, cx: &mut Context<Self>) {
        let Some(view) = self.page(cx) else {
            return;
        };
        let answers = self.answers.clone();
        view.update(cx, |view, _| {
            let Some(page) = view.page() else {
                return;
            };
            match self.showing {
                Tools::Console => {
                    let answers = answers.clone();
                    page.ask_tools("said()", move |said| answers.borrow_mut().said = Some(said));
                }
                Tools::Elements => {
                    page.ask_tools("tree(12)", {
                        let answers = answers.clone();
                        move |tree| answers.borrow_mut().tree = Some(tree)
                    });
                    if let Some(at) = self.picked {
                        page.ask_tools(&format!("about({at})"), {
                            let answers = answers.clone();
                            move |about| answers.borrow_mut().about = Some(about)
                        });
                    }
                }
                Tools::Network => {
                    let answers = answers.clone();
                    page.ask_tools("fetched()", move |fetched| {
                        answers.borrow_mut().fetched = Some(fetched)
                    });
                }
            }
        });
    }

    /// Takes whatever the page has answered since last time.
    fn take_answers(&mut self, cx: &mut Context<Self>) {
        let mut answers = self.answers.borrow_mut();
        if let Some(said) = answers.said.take()
            && let Ok(mut fresh) = serde_json::from_str::<Vec<Said>>(&said)
        {
            self.said.append(&mut fresh);
            // A page that talks for ever must not take the editor's memory with
            // it.
            let too_many = self.said.len().saturating_sub(500);
            self.said.drain(..too_many);
        }
        if let Some(tree) = answers.tree.take()
            && let Ok(rows) = serde_json::from_str::<Vec<Row>>(&tree)
        {
            self.rows = rows;
        }
        if let Some(fetched) = answers.fetched.take()
            && let Ok(fetched) = serde_json::from_str::<Vec<Fetched>>(&fetched)
        {
            self.fetched = fetched;
        }
        if let Some(about) = answers.about.take() {
            self.about = Some(about);
        }
        if let Some(ran) = answers.ran.take() {
            self.said.push(Said {
                level: "answer".into(),
                text: ran,
            });
        }
        drop(answers);
        cx.notify();
    }

    /// Runs what the reader typed, in the page.
    fn run_it(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let script = self.console.read(cx).text(cx);
        if script.trim().is_empty() {
            return;
        }
        self.said.push(Said {
            level: "asked".into(),
            text: script.clone(),
        });
        self.console.update(cx, |console, cx| {
            console.set_text("", window, cx);
        });
        let Some(view) = self.page(cx) else {
            self.said.push(Said {
                level: "error".into(),
                text: "There is no page in front of the reader to run this in.".into(),
            });
            cx.notify();
            return;
        };
        let answers = self.answers.clone();
        view.update(cx, |view, _| {
            if let Some(page) = view.page() {
                page.evaluate(&script, move |answer| {
                    answers.borrow_mut().ran = Some(answer)
                });
            }
        });
        cx.notify();
    }

    fn show(&mut self, tools: Tools, cx: &mut Context<Self>) {
        if self.showing == tools {
            return;
        }
        self.showing = tools;
        self.ask_the_page(cx);
        cx.notify();
    }

    fn pick(&mut self, at: usize, cx: &mut Context<Self>) {
        self.picked = Some(at);
        self.about = None;
        if let Some(view) = self.page(cx) {
            view.update(cx, |view, _| {
                if let Some(page) = view.page() {
                    page.ask_tools(&format!("highlight({at})"), |_| {});
                }
            });
        }
        self.ask_the_page(cx);
        cx.notify();
    }

    fn render_elements(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.rows.is_empty() {
            return Label::new("Nothing to show: open a Browser Page and this fills in.")
                .color(Color::Muted)
                .into_any_element();
        }
        h_flex()
            .size_full()
            .items_start()
            .child(
                v_flex()
                    .id("browser-tools-tree")
                    .h_full()
                    .w_2_3()
                    .overflow_y_scroll()
                    .children(self.rows.iter().map(|row| {
                        let at = row.at;
                        let picked = self.picked == Some(at);
                        h_flex()
                            .id(("row", at))
                            .w_full()
                            .px_2()
                            .pl(px(8. + row.depth as f32 * 12.))
                            .when(picked, |this| this.bg(cx.theme().colors().element_selected))
                            .hover(|style| style.bg(cx.theme().colors().element_hover))
                            .child(
                                Label::new(format!(
                                    "{}{}",
                                    row.text,
                                    match row.children {
                                        0 => String::new(),
                                        many => format!("  ({many})"),
                                    }
                                ))
                                .size(LabelSize::Small)
                                .color(if picked {
                                    Color::Default
                                } else {
                                    Color::Muted
                                }),
                            )
                            .on_click(cx.listener(move |panel, _, _, cx| panel.pick(at, cx)))
                    })),
            )
            .child(
                v_flex()
                    .id("browser-tools-about")
                    .h_full()
                    .w_1_3()
                    .p_2()
                    .gap_1()
                    .overflow_y_scroll()
                    .border_l_1()
                    .border_color(cx.theme().colors().border)
                    .children(self.about.as_deref().map(|about| {
                        Label::new(pretty(about))
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                    })),
            )
            .into_any_element()
    }

    fn render_console(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .size_full()
            .child(
                v_flex()
                    .id("browser-tools-said")
                    .flex_1()
                    .min_h_0()
                    .p_2()
                    .gap_0p5()
                    .overflow_y_scroll()
                    .children(self.said.iter().map(|said| {
                        Label::new(format!("{}  {}", mark(&said.level), said.text))
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .color(match said.level.as_str() {
                                "error" => Color::Error,
                                "warn" => Color::Warning,
                                "asked" => Color::Muted,
                                _ => Color::Default,
                            })
                    })),
            )
            .child(
                h_flex()
                    .w_full()
                    .flex_none()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .key_context("BrowserToolsConsole")
                    .on_action(cx.listener(|panel, _: &menu::Confirm, window, cx| {
                        panel.run_it(window, cx);
                    }))
                    .child(self.console.clone()),
            )
            .into_any_element()
    }

    fn render_network(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.fetched.is_empty() {
            return Label::new("Nothing fetched yet.")
                .color(Color::Muted)
                .into_any_element();
        }
        v_flex()
            .id("browser-tools-network")
            .size_full()
            .p_2()
            .gap_0p5()
            .overflow_y_scroll()
            .children(self.fetched.iter().enumerate().map(|(at, entry)| {
                h_flex()
                    .id(("fetched", at))
                    .w_full()
                    .gap_2()
                    .child(
                        Label::new(shorten(&entry.name))
                            .size(LabelSize::Small)
                            .buffer_font(cx),
                    )
                    .child(
                        Label::new(entry.kind.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(format!("{} ms", entry.ms))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(match entry.size {
                            0 => "cached".to_string(),
                            size => format!("{} kB", size / 1024),
                        })
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
            }))
            .into_any_element()
    }
}

/// The two-letter mark that says where a line came from.
fn mark(level: &str) -> &'static str {
    match level {
        "error" => "!!",
        "warn" => " !",
        "asked" => " >",
        "answer" => " <",
        _ => "  ",
    }
}

/// The page answers in JSON, which is not for reading. This puts each field on a
/// line of its own without pulling in a formatter.
fn pretty(json: &str) -> String {
    let mut out = String::with_capacity(json.len() + 32);
    let mut depth = 0_usize;
    let mut inside_text = false;
    for character in json.chars() {
        match character {
            '"' => {
                inside_text = !inside_text;
                out.push(character);
            }
            '{' | '[' if !inside_text => {
                depth += 1;
                out.push(character);
                out.push('\n');
                out.push_str(&"  ".repeat(depth));
            }
            '}' | ']' if !inside_text => {
                depth = depth.saturating_sub(1);
                out.push('\n');
                out.push_str(&"  ".repeat(depth));
                out.push(character);
            }
            ',' if !inside_text => {
                out.push(character);
                out.push('\n');
                out.push_str(&"  ".repeat(depth));
            }
            other => out.push(other),
        }
    }
    out
}

/// An address as much of it as is worth showing in a row.
fn shorten(name: &str) -> String {
    let tail = name.rsplit('/').next().unwrap_or(name);
    match tail.len() {
        0 => name.chars().take(60).collect(),
        _ => tail.chars().take(60).collect(),
    }
}

impl Focusable for BrowserToolsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for BrowserToolsPanel {}

impl Render for BrowserToolsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let showing = self.showing;
        v_flex()
            .key_context("BrowserTools")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .w_full()
                    .flex_none()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .children(Tools::ALL.map(|tools| {
                        Button::new(("tools", tools as usize), tools.label())
                            .label_size(LabelSize::Small)
                            .toggle_state(tools == showing)
                            .on_click(cx.listener(move |panel, _, _, cx| panel.show(tools, cx)))
                    })),
            )
            .child(div().flex_1().min_h_0().child(match showing {
                Tools::Elements => self.render_elements(cx),
                Tools::Console => self.render_console(cx),
                Tools::Network => self.render_network(cx),
            }))
    }
}

impl Panel for BrowserToolsPanel {
    fn persistent_name() -> &'static str {
        "BrowserToolsPanel"
    }

    fn panel_key() -> &'static str {
        PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, _position: DockPosition) -> bool {
        true
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(320.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::Code)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Browser Tools")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        9
    }
}

impl BrowserToolsPanel {
    /// Opens the panel, the way every other panel is opened.
    pub fn load(
        workspace: WeakEntity<Workspace>,
        cx: gpui::AsyncWindowContext,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        cx.spawn(async move |cx| cx.update(|window, cx| Self::new(workspace, window, cx)))
    }
}
