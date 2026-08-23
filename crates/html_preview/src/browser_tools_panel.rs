use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use editor::Editor;
use gpui::{
    App, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, SharedString, Size,
    Subscription, Task, WeakEntity, Window,
};
use serde::Deserialize;
use ui::Tooltip;
use ui::prelude::*;
use workspace::Workspace;
use workspace::dock::{DockPosition, Panel, PanelEvent};

use crate::ToggleFocus;
use crate::html_preview_view::HtmlPreviewView;

/// How often the page is asked what the open tab shows. Only while the panel is
/// showing, and only for that tab: every answer is a script the page runs, and a
/// panel that asks for all of it at once is work the page pays for.
const ASK_AGAIN: Duration = Duration::from_millis(400);

/// How deep the tree is read. Deeper than a reader scrolls, shallower than a
/// page built by a framework can nest without end.
const HOW_DEEP: usize = 30;

/// How many lines of what a page said are kept. A page that logs in a loop must
/// not take the editor's memory with it.
const MOST_SAID: usize = 1000;

const PANEL_KEY: &str = "BrowserToolsPanel";

/// Which of the tools the reader is looking at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tools {
    Elements,
    Console,
    Network,
    Style,
    Storage,
    Performance,
    Accessibility,
    Device,
}

impl Tools {
    const ALL: [Tools; 8] = [
        Self::Elements,
        Self::Console,
        Self::Network,
        Self::Style,
        Self::Storage,
        Self::Performance,
        Self::Accessibility,
        Self::Device,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Elements => "Elements",
            Self::Console => "Console",
            Self::Network => "Network",
            Self::Style => "Style",
            Self::Storage => "Storage",
            Self::Performance => "Performance",
            Self::Accessibility => "Accessibility",
            Self::Device => "Device",
        }
    }

    /// Whether what this tab shows changes on its own, and so is worth asking
    /// for again and again. Reading the page for what stands in a reader's way
    /// does not: it walks the whole page, so it is asked for once and again only
    /// when the reader asks. Nor does the size the page is shown at, which is
    /// the editor's own doing rather than the page's.
    fn keeps_changing(self) -> bool {
        !matches!(self, Self::Accessibility | Self::Device)
    }
}

/// Which side of one element the reader is reading.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    Rules,
    Computed,
    Layout,
    Fonts,
    Events,
}

impl Side {
    const ALL: [Side; 5] = [
        Self::Rules,
        Self::Computed,
        Self::Layout,
        Self::Fonts,
        Self::Events,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Rules => "Rules",
            Self::Computed => "Computed",
            Self::Layout => "Layout",
            Self::Fonts => "Fonts",
            Self::Events => "Events",
        }
    }
}

/// What a request is, for the chips that narrow the list. Both what asked for it
/// and what came back are used: a stylesheet the engine fetched itself has no
/// content type here, and an address alone does not say what a thing is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Kind {
    Document,
    Style,
    Script,
    Image,
    Font,
    Media,
    Asked,
    Other,
}

impl Kind {
    const ALL: [Kind; 8] = [
        Self::Document,
        Self::Style,
        Self::Script,
        Self::Image,
        Self::Font,
        Self::Media,
        Self::Asked,
        Self::Other,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Document => "HTML",
            Self::Style => "CSS",
            Self::Script => "JS",
            Self::Image => "Images",
            Self::Font => "Fonts",
            Self::Media => "Media",
            Self::Asked => "XHR",
            Self::Other => "Other",
        }
    }
}

/// What a request was: what the page asked for it as, what came back, and -- when
/// neither says -- what its address ends in.
///
/// The order matters. A request the page made itself is that kind of request
/// whatever comes back. Otherwise what came back is the best answer there is.
/// And last the address, because a font fetched by a stylesheet is a font: the
/// engine says only who asked for it, and calling the whole of a page's type
/// foundry "CSS" tells the reader nothing.
fn kind_of(how: &str, mime: &str, url: &str) -> Kind {
    if matches!(how, "xhr" | "fetch" | "beacon") {
        return Kind::Asked;
    }
    let mime = mime.to_ascii_lowercase();
    if !mime.is_empty() {
        if mime.contains("html") {
            return Kind::Document;
        } else if mime.contains("css") {
            return Kind::Style;
        } else if mime.contains("javascript")
            || mime.contains("ecmascript")
            || mime.contains("json")
        {
            return Kind::Script;
        } else if mime.starts_with("image/") {
            return Kind::Image;
        } else if mime.starts_with("font/") || mime.contains("woff") {
            return Kind::Font;
        } else if mime.starts_with("audio/") || mime.starts_with("video/") {
            return Kind::Media;
        }
    }
    let address = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    let ends_in = |endings: &[&str]| endings.iter().any(|ending| address.ends_with(ending));
    if ends_in(&[".woff2", ".woff", ".ttf", ".otf", ".eot"]) {
        return Kind::Font;
    }
    if ends_in(&[".css"]) {
        return Kind::Style;
    }
    if ends_in(&[".js", ".mjs", ".json"]) {
        return Kind::Script;
    }
    if ends_in(&[
        ".png", ".jpg", ".jpeg", ".gif", ".svg", ".webp", ".avif", ".ico",
    ]) {
        return Kind::Image;
    }
    if ends_in(&[".mp4", ".webm", ".mp3", ".ogg", ".wav", ".m4a"]) {
        return Kind::Media;
    }
    if ends_in(&[".html", ".htm"]) {
        return Kind::Document;
    }
    match how {
        "css" | "link" => Kind::Style,
        "script" => Kind::Script,
        "img" | "image" | "imageset" => Kind::Image,
        "font" => Kind::Font,
        "audio" | "video" | "track" => Kind::Media,
        "navigation" | "iframe" | "frame" => Kind::Document,
        _ => Kind::Other,
    }
}

/// Which of the questions an answer is to. The page answers on a turn of the
/// engine's own, long after the asking, so every answer carries its question.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ask {
    Said,
    Tree,
    Path,
    Rules,
    Computed,
    Layout,
    Fonts,
    Events,
    Selector,
    Html,
    Edited,
    Installed,
    Network,
    Request,
    Sheets,
    SheetText,
    Storage,
    Cost,
    Findings,
    Picked,
    Ran,
    /// Which document is being read, so that what was picked in one page is not
    /// acted on in the next.
    Who,
    /// Something the page was told to do rather than asked about -- turn a
    /// stylesheet off, forget a key, draw the tab order. There is nothing to
    /// show for it.
    Nothing,
}

#[derive(Deserialize)]
struct Row {
    at: usize,
    depth: usize,
    text: String,
    children: usize,
    #[serde(default)]
    preview: String,
    #[serde(default)]
    listens: usize,
}

#[derive(Deserialize)]
struct Said {
    level: String,
    text: String,
    #[serde(default)]
    from: String,
    #[serde(default = "once")]
    times: usize,
}

fn once() -> usize {
    1
}

#[derive(Deserialize)]
struct Crumb {
    at: i64,
    text: String,
}

#[derive(Deserialize)]
struct Rule {
    sheet: String,
    selector: String,
    #[serde(default)]
    media: String,
    declarations: Vec<(String, String)>,
}

#[derive(Deserialize)]
struct Frame {
    left: f32,
    top: f32,
    width: f32,
    height: f32,
}

#[derive(Deserialize)]
struct Room {
    width: f32,
    height: f32,
}

#[derive(Deserialize)]
struct Flex {
    direction: String,
    wrap: String,
    justify: String,
    align: String,
    gap: String,
}

#[derive(Deserialize)]
struct Grid {
    columns: String,
    rows: String,
    gap: String,
    #[serde(default)]
    areas: String,
}

#[derive(Deserialize)]
struct Layout {
    #[serde(rename = "box")]
    frame: Frame,
    margin: [f32; 4],
    border: [f32; 4],
    padding: [f32; 4],
    content: Room,
    display: String,
    position: String,
    #[serde(rename = "boxSizing")]
    box_sizing: String,
    #[serde(rename = "zIndex")]
    z_index: String,
    overflow: String,
    flex: Option<Flex>,
    grid: Option<Grid>,
}

#[derive(Deserialize)]
struct ElementFont {
    family: String,
    size: String,
    weight: String,
    style: String,
    height: String,
    spacing: String,
}

#[derive(Deserialize)]
struct Face {
    family: String,
    weight: String,
    style: String,
    status: String,
}

#[derive(Deserialize)]
struct Fonts {
    element: Option<ElementFont>,
    #[serde(default)]
    faces: Vec<Face>,
}

#[derive(Default, Deserialize)]
struct Installed {
    #[serde(default)]
    manifest: String,
    #[serde(default)]
    workers: Vec<String>,
    #[serde(default)]
    supported: bool,
}

#[derive(Deserialize)]
struct Wire {
    id: i64,
    method: String,
    url: String,
    kind: String,
    status: u16,
    size: u64,
    ms: u64,
    start: u64,
    #[serde(rename = "type", default)]
    mime: String,
}

#[derive(Deserialize)]
struct Detail {
    url: String,
    method: String,
    status: u16,
    #[serde(rename = "statusText", default)]
    status_text: String,
    #[serde(rename = "type", default)]
    mime: String,
    size: u64,
    ms: u64,
    #[serde(rename = "reqHeaders", default)]
    asked: Vec<(String, String)>,
    #[serde(rename = "resHeaders", default)]
    answered: Vec<(String, String)>,
    #[serde(default)]
    phases: Vec<(String, f32)>,
    #[serde(default)]
    body: String,
}

#[derive(Deserialize)]
struct Sheet {
    id: usize,
    name: String,
    rules: usize,
    disabled: bool,
    #[serde(default)]
    media: String,
    readable: bool,
}

#[derive(Deserialize)]
struct SheetText {
    name: String,
    #[serde(default)]
    text: String,
    rules: usize,
}

#[derive(Default, Deserialize)]
struct Stores {
    #[serde(default)]
    cookies: Vec<(String, String)>,
    #[serde(default)]
    local: Vec<(String, String)>,
    #[serde(default)]
    session: Vec<(String, String)>,
    #[serde(default)]
    databases: Vec<String>,
    #[serde(default)]
    caches: Vec<String>,
}

#[derive(Default, Deserialize)]
struct Counts {
    #[serde(default)]
    elements: u64,
    #[serde(default)]
    text: u64,
    #[serde(default)]
    images: u64,
    #[serde(default)]
    scripts: u64,
    #[serde(default)]
    stylesheets: u64,
    #[serde(default)]
    rules: u64,
    #[serde(default)]
    listeners: u64,
    #[serde(default)]
    requests: u64,
    #[serde(default)]
    transferred: u64,
}

#[derive(Deserialize)]
struct Memory {
    used: u64,
    total: u64,
}

#[derive(Default, Deserialize)]
struct Cost {
    #[serde(default)]
    phases: Vec<(String, f32)>,
    #[serde(default)]
    paints: Vec<(String, f32)>,
    #[serde(default)]
    counts: Counts,
    #[serde(default)]
    memory: Option<Memory>,
}

#[derive(Deserialize)]
struct Finding {
    level: String,
    rule: String,
    text: String,
    at: i64,
    #[serde(default)]
    selector: String,
}

#[derive(Deserialize)]
struct Chosen {
    at: usize,
    #[serde(default)]
    selector: String,
}

/// What the page has answered, left in a box because the answers arrive on the
/// engine's own turn, where there is no context to hand.
#[derive(Default)]
struct Answers {
    got: Vec<(Ask, String)>,
}

/// One of the page's stores, for the rows the storage tab shows and the delete
/// beside each of them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Store {
    Cookie,
    Local,
    Session,
}

impl Store {
    fn word(self) -> &'static str {
        match self {
            Self::Cookie => "cookie",
            Self::Local => "local",
            Self::Session => "session",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Cookie => "Cookies",
            Self::Local => "Local storage",
            Self::Session => "Session storage",
        }
    }
}

/// The developer's tools for a live page, in the dock beside the terminal.
///
/// It asks the page about itself rather than speaking a debugging protocol: the
/// page already carries a little script of ours, and everything here -- the tree
/// it is made of, which rules reach one element, what its scripts have said,
/// what it fetched, what it keeps, what stands in a reader's way -- is something
/// the page can be asked directly. Nothing listens on a port.
pub struct BrowserToolsPanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    position: DockPosition,
    showing: Tools,
    side: Side,

    /// Where the reader types something for the page to run, and what has been
    /// typed there before.
    console: Entity<Editor>,
    history: Vec<String>,
    history_at: Option<usize>,
    said: Vec<Said>,
    console_filter: Entity<Editor>,
    quiet: HashSet<String>,

    rows: Vec<Row>,
    folded: HashSet<usize>,
    tree_filter: Entity<Editor>,
    picked: Option<usize>,
    picked_selector: String,
    picked_html: String,
    /// Where the reader rewrites the markup of the element they picked, and
    /// whether they are doing so.
    html_editor: Entity<Editor>,
    editing_html: bool,
    crumbs: Vec<Crumb>,
    rules: Vec<Rule>,
    computed: Vec<(String, String)>,
    layout: Option<Layout>,
    fonts: Option<Fonts>,
    events: Vec<(String, usize, String)>,

    wires: Vec<Wire>,
    wire_filter: Entity<Editor>,
    hidden_kinds: HashSet<Kind>,
    chosen_wire: Option<i64>,
    wire: Option<Detail>,

    sheets: Vec<Sheet>,
    chosen_sheet: Option<usize>,
    sheet: Option<SheetText>,

    /// The page the answers above are about. A number that names an element is
    /// the page's own numbering, and it means nothing in another page.
    document: String,
    /// What the last few seconds of frames cost the engine, and the size the page
    /// is being laid out at. Read when the page is asked its questions rather
    /// than while the panel is drawn: finding the page means reading the
    /// workspace, and the workspace is already being read to draw this.
    frames: Option<crate::html_preview_view::Frames>,
    device: Option<Size<Pixels>>,
    stores: Stores,
    installed: Installed,
    cost: Cost,
    findings: Vec<Finding>,

    /// Whether the dock says this panel is the one on screen. Nothing is asked
    /// of the page while it is not: every answer is a script the page runs, and
    /// a panel behind another one is worth nothing to the reader. `None` means
    /// the dock has not said either way, which is read as "on screen" so that a
    /// panel that is never told does not sit there empty.
    told_active: Option<bool>,

    /// The tools that are drawn on the page itself rather than here.
    picking: bool,
    ruled: bool,
    measuring: bool,
    numbering: bool,

    answers: Rc<RefCell<Answers>>,
    _collector: Task<()>,
    _typing: Vec<Subscription>,
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
            let tree_filter = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text("Find an element or a property", window, cx);
                editor
            });
            let console_filter = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text("Filter", window, cx);
                editor
            });
            let wire_filter = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text("Filter by address", window, cx);
                editor
            });
            let html_editor = cx.new(|cx| {
                let mut editor = Editor::multi_line(window, cx);
                editor.set_placeholder_text("The markup to put in its place", window, cx);
                editor
            });
            let typing = [
                &console,
                &tree_filter,
                &console_filter,
                &wire_filter,
                &html_editor,
            ]
            .into_iter()
            .map(|editor| {
                cx.subscribe(editor, |_: &mut Self, _, _: &editor::EditorEvent, cx| {
                    cx.notify();
                })
            })
            .collect();
            let answers: Rc<RefCell<Answers>> = Rc::default();
            // The page answers on a turn of the engine's own, so the answers are
            // left in a box and picked up from here.
            let collector = cx.spawn({
                let answers = answers.clone();
                async move |panel, cx| {
                    loop {
                        cx.background_executor().timer(ASK_AGAIN).await;
                        let anything = !answers.borrow().got.is_empty();
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
                showing: Tools::Elements,
                side: Side::Rules,
                console,
                history: Vec::new(),
                history_at: None,
                said: Vec::new(),
                console_filter,
                quiet: HashSet::new(),
                rows: Vec::new(),
                folded: HashSet::new(),
                tree_filter,
                picked: None,
                picked_selector: String::new(),
                picked_html: String::new(),
                html_editor,
                editing_html: false,
                crumbs: Vec::new(),
                rules: Vec::new(),
                computed: Vec::new(),
                layout: None,
                fonts: None,
                events: Vec::new(),
                wires: Vec::new(),
                wire_filter,
                hidden_kinds: HashSet::new(),
                chosen_wire: None,
                wire: None,
                sheets: Vec::new(),
                chosen_sheet: None,
                sheet: None,
                document: String::new(),
                frames: None,
                device: None,
                stores: Stores::default(),
                installed: Installed::default(),
                cost: Cost::default(),
                findings: Vec::new(),
                told_active: None,
                picking: false,
                ruled: false,
                measuring: false,
                numbering: false,
                answers,
                _collector: collector,
                _typing: typing,
            }
        })
    }

    /// The page being read, if the reader is reading one.
    fn page(&self, cx: &App) -> Option<Entity<HtmlPreviewView>> {
        let workspace = self.workspace.upgrade()?;
        let workspace = workspace.read(cx);
        // Asked for as "whatever stands in for a page", not as the item itself: a
        // page read beside its source is inside another item, and that item
        // answers for it.
        workspace
            .active_item(cx)
            .and_then(|item| item.act_as::<HtmlPreviewView>(cx))
    }

    /// Asks the page one question, and files the answer under it.
    fn put(&self, ask: Ask, question: String, cx: &mut Context<Self>) {
        let Some(view) = self.page(cx) else {
            return;
        };
        let answers = self.answers.clone();
        view.update(cx, |view, _| {
            if let Some(page) = view.page() {
                page.ask_tools(&question, move |answer| {
                    answers.borrow_mut().got.push((ask, answer))
                });
            }
        });
    }

    /// Asks the page something with words of the reader's own in it -- a key to
    /// forget, a store to clear. The words are escaped by the engine rather than
    /// pasted into the question.
    fn put_about(&self, ask: Ask, question: &str, words: &[&str], cx: &mut Context<Self>) {
        let Some(view) = self.page(cx) else {
            return;
        };
        let answers = self.answers.clone();
        view.update(cx, |view, _| {
            if let Some(page) = view.page() {
                page.ask_tools_about(question, words, move |answer| {
                    answers.borrow_mut().got.push((ask, answer))
                });
            }
        });
    }

    /// Whether the reader can see this panel at all.
    fn being_looked_at(&self) -> bool {
        self.told_active.unwrap_or(true)
    }

    /// Asks the page whatever the open tab needs, again. Every answer is a
    /// script the page runs, so only the tab in front of the reader asks for
    /// anything -- and a tab that reads the whole page does not ask twice.
    fn ask_the_page(&mut self, cx: &mut Context<Self>) {
        if !self.being_looked_at() {
            return;
        }
        if self.picking {
            self.put(Ask::Picked, "picked()".into(), cx);
        }
        // Whichever tab is open: the page keeps only its last few hundred lines,
        // so a reader who opens the console after something went wrong would
        // otherwise find the beginning of it dropped. And which page it is,
        // because everything else here is about one page in particular.
        self.put(Ask::Said, "said()".into(), cx);
        self.put(Ask::Who, "who()".into(), cx);
        // Both of these are the preview's own answers rather than the page's, and
        // this is the one moment it is safe to ask for them.
        if let Some(view) = self.page(cx) {
            let view = view.read(cx);
            self.frames = view.how_the_frames_go();
            self.device = view.shown_at();
        }
        if !self.showing.keeps_changing() {
            return;
        }
        match self.showing {
            Tools::Elements => {
                // Only when it has changed: the answer is the whole page, and
                // a page can be thousands of elements.
                self.put(Ask::Tree, format!("treeIfChanged({HOW_DEEP})"), cx);
                if let Some(at) = self.picked {
                    self.put(Ask::Path, format!("path({at})"), cx);
                    if worth_reading_again(self.side) {
                        self.ask_about_the_element(at, cx);
                    }
                }
            }
            Tools::Console => {}
            Tools::Network => {
                self.put(Ask::Network, "network()".into(), cx);
                if let Some(id) = self.chosen_wire {
                    self.put(Ask::Request, format!("request({id})"), cx);
                }
            }
            Tools::Style => {
                self.put(Ask::Sheets, "sheets()".into(), cx);
                if let Some(id) = self.chosen_sheet {
                    self.put(Ask::SheetText, format!("sheet({id})"), cx);
                }
            }
            Tools::Storage => {
                self.put(Ask::Storage, "storage()".into(), cx);
                self.put(Ask::Installed, "installed()".into(), cx);
            }
            Tools::Performance => self.put(Ask::Cost, "timings()".into(), cx),
            Tools::Accessibility | Tools::Device => {}
        }
    }

    /// What the open side of the picked element needs, and only that side:
    /// reading every rule and every property for a side nobody is looking at is
    /// work the page pays for. Asked whenever the reader picks something or
    /// turns to another side; see `worth_reading_again` for what is asked over
    /// and over as well.
    fn ask_about_the_element(&self, at: usize, cx: &mut Context<Self>) {
        match self.side {
            Side::Rules => self.put(Ask::Rules, format!("rules({at})"), cx),
            Side::Computed => self.put(Ask::Computed, format!("computed({at})"), cx),
            Side::Layout => self.put(Ask::Layout, format!("layout({at})"), cx),
            Side::Fonts => self.put(Ask::Fonts, format!("fonts({at})"), cx),
            Side::Events => self.put(Ask::Events, format!("listening({at})"), cx),
        }
    }

    /// Takes whatever the page has answered since last time.
    fn take_answers(&mut self, cx: &mut Context<Self>) {
        let answered: Vec<(Ask, String)> = std::mem::take(&mut self.answers.borrow_mut().got);
        for (ask, answer) in answered {
            match ask {
                Ask::Said => {
                    if let Ok(fresh) = serde_json::from_str::<Vec<Said>>(&answer) {
                        for line in fresh {
                            self.remember(line);
                        }
                        let too_many = self.said.len().saturating_sub(MOST_SAID);
                        self.said.drain(..too_many);
                    }
                }
                Ask::Tree => {
                    if let Ok(rows) = serde_json::from_str::<Vec<Row>>(&answer) {
                        self.rows = rows;
                    }
                }
                Ask::Path => {
                    if let Ok(crumbs) = serde_json::from_str::<Vec<Crumb>>(&answer) {
                        self.crumbs = crumbs;
                    }
                }
                Ask::Rules => {
                    if let Ok(rules) = serde_json::from_str::<Vec<Rule>>(&answer) {
                        self.rules = rules;
                    }
                }
                Ask::Computed => {
                    if let Ok(properties) = serde_json::from_str::<Vec<(String, String)>>(&answer) {
                        self.computed = properties;
                    }
                }
                Ask::Layout => {
                    if let Ok(layout) = serde_json::from_str::<Layout>(&answer) {
                        self.layout = Some(layout);
                    }
                }
                Ask::Fonts => {
                    if let Ok(fonts) = serde_json::from_str::<Fonts>(&answer) {
                        self.fonts = Some(fonts);
                    }
                }
                Ask::Installed => {
                    if let Ok(installed) = serde_json::from_str::<Installed>(&answer) {
                        self.installed = installed;
                    }
                }
                Ask::Events => {
                    if let Ok(events) =
                        serde_json::from_str::<Vec<(String, usize, String)>>(&answer)
                    {
                        self.events = events;
                    }
                }
                Ask::Selector => self.picked_selector = answer,
                Ask::Html => self.picked_html = answer,
                Ask::Edited => {
                    // Nothing back is the page saying it did it; the result is
                    // to be seen in the page rather than said here.
                    if !answer.trim().is_empty() {
                        self.remember(Said {
                            level: "error".into(),
                            text: answer,
                            from: String::new(),
                            times: 1,
                        });
                    }
                }
                Ask::Network => {
                    if let Ok(wires) = serde_json::from_str::<Vec<Wire>>(&answer) {
                        self.wires = wires;
                    }
                }
                Ask::Request => {
                    if let Ok(detail) = serde_json::from_str::<Detail>(&answer) {
                        self.wire = Some(detail);
                    }
                }
                Ask::Sheets => {
                    if let Ok(sheets) = serde_json::from_str::<Vec<Sheet>>(&answer) {
                        self.sheets = sheets;
                    }
                }
                Ask::SheetText => {
                    if let Ok(sheet) = serde_json::from_str::<SheetText>(&answer) {
                        self.sheet = Some(sheet);
                    }
                }
                Ask::Storage => {
                    if let Ok(stores) = serde_json::from_str::<Stores>(&answer) {
                        self.stores = stores;
                    }
                }
                Ask::Cost => {
                    if let Ok(cost) = serde_json::from_str::<Cost>(&answer) {
                        self.cost = cost;
                    }
                }
                Ask::Findings => {
                    if let Ok(mut findings) = serde_json::from_str::<Vec<Finding>>(&answer) {
                        worst_first(&mut findings);
                        self.findings = findings;
                    }
                }
                Ask::Picked => {
                    if let Ok(chosen) = serde_json::from_str::<Chosen>(&answer) {
                        self.picking = false;
                        self.showing = Tools::Elements;
                        self.pick(chosen.at, cx);
                        self.picked_selector = chosen.selector;
                    }
                }
                Ask::Ran => self.remember(said_by_the_page(answer)),
                Ask::Who => {
                    if !answer.is_empty() && answer != self.document {
                        let first = self.document.is_empty();
                        self.document = answer;
                        // A page of its own: what the panel is holding was read
                        // out of another one, and a number that named an element
                        // there names something else here. What the pages have
                        // said is kept on purpose -- it is often the reason the
                        // reader went to the next one.
                        if !first {
                            self.forget_the_page();
                        }
                    }
                }
                Ask::Nothing => {}
            }
        }
        cx.notify();
    }

    /// Lets go of everything that was read out of a page, once the reader is
    /// looking at another one.
    fn forget_the_page(&mut self) {
        self.rows.clear();
        self.folded.clear();
        self.picked = None;
        self.picked_selector.clear();
        self.picked_html.clear();
        self.editing_html = false;
        self.crumbs.clear();
        self.rules.clear();
        self.computed.clear();
        self.layout = None;
        self.fonts = None;
        self.events.clear();
        self.wires.clear();
        self.chosen_wire = None;
        self.wire = None;
        self.sheets.clear();
        self.chosen_sheet = None;
        self.sheet = None;
        self.stores = Stores::default();
        self.installed = Installed::default();
        self.cost = Cost::default();
        self.findings.clear();
    }

    /// Takes what the tools drew on the page off it again. A page left with the
    /// measuring drag armed holds back every press the reader makes, which reads
    /// as a page that has stopped working -- so nothing of ours is left on a page
    /// the reader can no longer see the panel for.
    fn put_the_tools_away(&mut self, cx: &mut Context<Self>) {
        for (on, question) in [
            (self.picking, "pick(0)"),
            (self.measuring, "measure(0)"),
            (self.ruled, "rulers(0)"),
            (self.numbering, "tabOrder(0)"),
        ] {
            if on {
                self.put(Ask::Nothing, question.to_string(), cx);
            }
        }
        if self.picking || self.measuring || self.ruled || self.numbering {
            self.outline(None, cx);
        }
        self.picking = false;
        self.measuring = false;
        self.ruled = false;
        self.numbering = false;
    }

    /// Keeps one line the page said, or counts it again if it is the same line
    /// twice. A page that logs in a loop fills a panel otherwise.
    fn remember(&mut self, line: Said) {
        if let Some(last) = self.said.last_mut()
            && last.level == line.level
            && last.text == line.text
        {
            last.times += line.times;
            return;
        }
        self.said.push(line);
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
            from: String::new(),
            times: 1,
        });
        self.history.push(script.clone());
        self.history_at = None;
        self.console.update(cx, |console, cx| {
            console.set_text("", window, cx);
        });
        let Some(view) = self.page(cx) else {
            self.said.push(Said {
                level: "error".into(),
                text: "There is no page in front of the reader to run this in.".into(),
                from: String::new(),
                times: 1,
            });
            cx.notify();
            return;
        };
        let answers = self.answers.clone();
        view.update(cx, |view, _| {
            if let Some(page) = view.page() {
                page.run_in_page(&script, move |answer| {
                    answers.borrow_mut().got.push((Ask::Ran, answer))
                });
            }
        });
        cx.notify();
    }

    /// Back and forward through what has been typed here, the way a shell walks
    /// its history.
    fn walk_history(&mut self, back: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.history.is_empty() {
            return;
        }
        let last = self.history.len() - 1;
        let at = match (self.history_at, back) {
            (None, true) => Some(last),
            (None, false) => None,
            (Some(at), true) => Some(at.saturating_sub(1)),
            (Some(at), false) if at >= last => None,
            (Some(at), false) => Some(at + 1),
        };
        self.history_at = at;
        let text = at
            .and_then(|at| self.history.get(at).cloned())
            .unwrap_or_default();
        self.console.update(cx, |console, cx| {
            console.set_text(text, window, cx);
        });
        cx.notify();
    }

    fn show(&mut self, tools: Tools, cx: &mut Context<Self>) {
        if self.showing == tools {
            return;
        }
        self.showing = tools;
        if tools == Tools::Accessibility && self.findings.is_empty() {
            self.read_the_page(cx);
        }
        self.ask_the_page(cx);
        cx.notify();
    }

    fn show_side(&mut self, side: Side, cx: &mut Context<Self>) {
        if self.side == side {
            return;
        }
        self.side = side;
        if let Some(at) = self.picked {
            self.ask_about_the_element(at, cx);
        }
        cx.notify();
    }

    fn pick(&mut self, at: usize, cx: &mut Context<Self>) {
        self.picked = Some(at);
        self.rules.clear();
        self.computed.clear();
        self.layout = None;
        self.fonts = None;
        self.events.clear();
        self.put(Ask::Selector, format!("selector({at})"), cx);
        self.put(Ask::Html, format!("html({at})"), cx);
        self.put(Ask::Path, format!("path({at})"), cx);
        // So that `$0` in the console is the element in front of the reader.
        self.put(Ask::Nothing, format!("chose({at})"), cx);
        self.outline(Some(at), cx);
        self.ask_about_the_element(at, cx);
        cx.notify();
    }

    /// Draws a frame around one element on the page itself, or takes it away.
    fn outline(&self, at: Option<usize>, cx: &mut Context<Self>) {
        let at = at.map_or(-1, |at| at as i64);
        self.put(Ask::Nothing, format!("highlight({at})"), cx);
    }

    /// Opens the markup of the picked element for the reader to rewrite.
    fn edit_the_html(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.picked.is_none() {
            return;
        }
        let markup = self.picked_html.clone();
        self.html_editor.update(cx, |editor, cx| {
            editor.set_text(markup, window, cx);
        });
        self.editing_html = true;
        cx.notify();
    }

    /// Puts what the reader wrote in place of the element they picked.
    fn replace_the_element(&mut self, cx: &mut Context<Self>) {
        let Some(at) = self.picked else {
            return;
        };
        let index = at.to_string();
        let markup = self.html_editor.read(cx).text(cx);
        self.put_about(
            Ask::Edited,
            "setHtml",
            &[index.as_str(), markup.as_str()],
            cx,
        );
        self.editing_html = false;
        // What replaced the element is a different element, and the numbering
        // the panel is holding is the page as it was, so nothing is picked
        // until the tree has been read again.
        self.picked = None;
        self.picked_html.clear();
        self.picked_selector.clear();
        cx.notify();
    }

    fn fold(&mut self, at: usize, cx: &mut Context<Self>) {
        if !self.folded.remove(&at) {
            self.folded.insert(at);
        }
        cx.notify();
    }

    /// Arms the picker: the reader's next click in the page picks whatever is
    /// under it rather than reaching the page.
    fn start_picking(&mut self, cx: &mut Context<Self>) {
        self.picking = !self.picking;
        let on = i32::from(self.picking);
        self.put(Ask::Nothing, format!("pick({on})"), cx);
        cx.notify();
    }

    fn toggle_rulers(&mut self, cx: &mut Context<Self>) {
        self.ruled = !self.ruled;
        let on = i32::from(self.ruled);
        self.put(Ask::Nothing, format!("rulers({on})"), cx);
        cx.notify();
    }

    fn toggle_measure(&mut self, cx: &mut Context<Self>) {
        self.measuring = !self.measuring;
        let on = i32::from(self.measuring);
        self.put(Ask::Nothing, format!("measure({on})"), cx);
        cx.notify();
    }

    fn toggle_numbering(&mut self, cx: &mut Context<Self>) {
        self.numbering = !self.numbering;
        let on = i32::from(self.numbering);
        self.put(Ask::Nothing, format!("tabOrder({on})"), cx);
        cx.notify();
    }

    /// Reads the page for what would stand in a reader's way. Asked for rather
    /// than polled: it walks the whole page and works out the contrast of every
    /// line of text on it.
    fn read_the_page(&mut self, cx: &mut Context<Self>) {
        self.put(Ask::Findings, "audit()".into(), cx);
    }

    /// Asks the preview to show the page at the size of a device, or at the
    /// size of the pane again.
    fn show_the_page_at(&mut self, device: Option<Size<Pixels>>, cx: &mut Context<Self>) {
        if let Some(view) = self.page(cx) {
            view.update(cx, |view, cx| view.show_at(device, cx));
        }
        self.device = device;
        cx.notify();
    }

    fn copy(&self, text: String, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    // ------------------------------------------------------------------ elements

    fn render_elements(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.rows.is_empty() {
            return nothing_yet(
                "Nothing to show yet. Open a page and what it is made of appears here.",
            );
        }
        let filter = self.tree_filter.read(cx).text(cx).to_lowercase();
        let showing = showing_rows(&self.rows, &self.folded, &filter);
        let crumbs = self.render_crumbs(cx);
        let tree: Vec<AnyElement> = showing
            .into_iter()
            .filter_map(|which| self.rows.get(which))
            .map(|row| self.render_tree_row(row, cx))
            .collect();
        let tabs = self.render_side_tabs(cx);
        let editing = self.editing_html.then(|| self.render_html_editor(cx));
        let side = match self.side {
            Side::Rules => self.render_rules(cx),
            Side::Computed => self.render_computed(cx),
            Side::Layout => self.render_layout(cx),
            Side::Fonts => self.render_fonts(cx),
            Side::Events => self.render_events(cx),
        };
        let edge = cx.theme().colors().border;
        v_flex()
            .size_full()
            .child(crumbs)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .child(
                        v_flex()
                            .id("browser-tools-tree")
                            .h_full()
                            .w_1_2()
                            .overflow_y_scroll()
                            .children(tree),
                    )
                    .child(
                        v_flex()
                            .h_full()
                            .w_1_2()
                            .min_w_0()
                            .border_l_1()
                            .border_color(edge)
                            .child(tabs)
                            .children(editing)
                            .child(div().flex_1().min_h_0().child(side)),
                    ),
            )
            .into_any_element()
    }

    fn render_tree_row(&self, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        let at = row.at;
        let picked = self.picked == Some(at);
        let folded = self.folded.contains(&at);
        let has_children = row.children > 0;
        h_flex()
            .id(("row", at))
            .debug_selector(move || format!("TREE-{at}"))
            .w_full()
            .gap_1()
            .px_1()
            .pl(px(4. + row.depth as f32 * 10.))
            .when(picked, |this| this.bg(cx.theme().colors().element_selected))
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .child(
                div()
                    .w(px(12.))
                    .flex_none()
                    .debug_selector(move || format!("INDENT-{at}"))
                    .when(has_children, |this| {
                        this.child(
                            div()
                                .debug_selector(move || format!("FOLD-{at}"))
                                .child(
                                    Icon::new(match folded {
                                        true => IconName::ChevronRight,
                                        false => IconName::ChevronDown,
                                    })
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                                )
                                .on_mouse_down(
                                    gpui::MouseButton::Left,
                                    cx.listener(move |panel, _, _, cx| panel.fold(at, cx)),
                                ),
                        )
                    }),
            )
            .child(
                Label::new(row.text.clone())
                    .size(LabelSize::Small)
                    .buffer_font(cx)
                    .color(if picked { Color::Default } else { Color::Muted }),
            )
            .when(has_children, |this| {
                this.child(
                    Label::new(format!("({})", row.children))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .when(row.listens > 0, |this| {
                this.child(
                    Label::new("event")
                        .size(LabelSize::XSmall)
                        .color(Color::Accent),
                )
            })
            .when(!row.preview.is_empty(), |this| {
                this.child(
                    Label::new(row.preview.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate(),
                )
            })
            .on_click(cx.listener(move |panel, _, _, cx| panel.pick(at, cx)))
            .on_hover(cx.listener(move |panel, over: &bool, _, cx| {
                if *over {
                    panel.outline(Some(at), cx);
                }
            }))
            .into_any_element()
    }

    fn render_crumbs(&self, cx: &mut Context<Self>) -> AnyElement {
        let edge = cx.theme().colors().border;
        h_flex()
            .id("browser-tools-crumbs")
            .w_full()
            .flex_none()
            .gap_1()
            .px_1()
            .py_0p5()
            .overflow_x_scroll()
            .border_b_1()
            .border_color(edge)
            .children(self.crumbs.iter().enumerate().map(|(which, crumb)| {
                let at = crumb.at;
                Button::new(("crumb", which), crumb.text.clone())
                    .label_size(LabelSize::XSmall)
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        if at >= 0 {
                            panel.pick(at as usize, cx);
                        }
                    }))
            }))
            .when(self.crumbs.is_empty(), |this| {
                this.child(
                    Label::new("Pick an element to see where it sits.")
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
            })
            .into_any_element()
    }

    fn render_side_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let side = self.side;
        let picked = self.picked;
        let selector = self.picked_selector.clone();
        let html = self.picked_html.clone();
        let edge = cx.theme().colors().border;
        h_flex()
            .w_full()
            .flex_none()
            .gap_1()
            .px_1()
            .py_0p5()
            .border_b_1()
            .border_color(edge)
            .children(Side::ALL.map(|one| {
                div()
                    .id(("side-hitbox", one as usize))
                    .debug_selector(move || format!("SIDE-{}", one.label()))
                    .child(
                        Button::new(("side", one as usize), one.label())
                            .label_size(LabelSize::XSmall)
                            .toggle_state(one == side)
                            .on_click(cx.listener(move |panel, _, _, cx| panel.show_side(one, cx))),
                    )
            }))
            .when_some(picked, |this, at| {
                this.child(div().flex_1())
                    .child(
                        IconButton::new("copy-selector", IconName::Copy)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Copy this element's selector"))
                            .on_click(
                                cx.listener(move |panel, _, _, cx| {
                                    panel.copy(selector.clone(), cx)
                                }),
                            ),
                    )
                    .child(
                        Button::new("copy-html", "HTML")
                            .label_size(LabelSize::XSmall)
                            .tooltip(Tooltip::text("Copy this element's HTML"))
                            .on_click(
                                cx.listener(move |panel, _, _, cx| panel.copy(html.clone(), cx)),
                            ),
                    )
                    .child(
                        div()
                            .id("edit-html-hitbox")
                            .debug_selector(|| "EDIT-OPEN".to_string())
                            .child(
                                Button::new("edit-html", "Edit")
                                    .label_size(LabelSize::XSmall)
                                    .tooltip(Tooltip::text("Rewrite this element's markup"))
                                    .on_click(cx.listener(|panel, _, window, cx| {
                                        panel.edit_the_html(window, cx)
                                    })),
                            ),
                    )
                    .child(
                        Button::new("bring-into-view", "Show")
                            .label_size(LabelSize::XSmall)
                            .tooltip(Tooltip::text("Scroll the page to this element"))
                            .on_click(cx.listener(move |panel, _, _, cx| {
                                panel.put(Ask::Nothing, format!("bring({at})"), cx);
                            })),
                    )
                    .child(
                        IconButton::new("remove-element", IconName::Trash)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Take this element out of the page"))
                            .on_click(cx.listener(move |panel, _, _, cx| {
                                panel.put(Ask::Nothing, format!("remove({at})"), cx);
                                panel.picked = None;
                                cx.notify();
                            })),
                    )
            })
            .into_any_element()
    }

    fn render_html_editor(&self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .w_full()
            .flex_none()
            .p_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                // A definite height: a multi-line editor has none of its own and
                // is painted as a sliver without one.
                div()
                    .id("browser-tools-html")
                    .debug_selector(|| "EDIT-HTML".to_string())
                    .w_full()
                    .h(px(120.))
                    .child(self.html_editor.clone()),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        div()
                            .id("apply-html-hitbox")
                            .debug_selector(|| "EDIT-APPLY".to_string())
                            .child(
                                Button::new("apply-html", "Put it in the page")
                                    .label_size(LabelSize::XSmall)
                                    .on_click(
                                        cx.listener(|panel, _, _, cx| {
                                            panel.replace_the_element(cx)
                                        }),
                                    ),
                            ),
                    )
                    .child(
                        Button::new("cancel-html", "Leave it as it is")
                            .label_size(LabelSize::XSmall)
                            .on_click(cx.listener(|panel, _, _, cx| {
                                panel.editing_html = false;
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_rules(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.picked.is_none() {
            return nothing_yet("Pick an element to see which rules reach it.");
        }
        if self.rules.is_empty() {
            return nothing_yet("No stylesheet of this page reaches this element.");
        }
        v_flex()
            .id("browser-tools-rules")
            .size_full()
            .p_1()
            .gap_1()
            .overflow_y_scroll()
            // Last in the cascade first: what the reader is looking for is
            // whatever won, and that is the one at the end.
            .children(self.rules.iter().rev().enumerate().map(|(which, rule)| {
                v_flex()
                    .id(("rule", which))
                    .debug_selector(move || format!("RULE-{which}"))
                    .w_full()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .child(
                                Label::new(rule.selector.clone())
                                    .size(LabelSize::Small)
                                    .buffer_font(cx)
                                    .color(Color::Accent),
                            )
                            .child(
                                Label::new(rule.sheet.clone())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .when(!rule.media.is_empty(), |this| {
                                this.child(
                                    Label::new(format!("@ {}", rule.media))
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                            }),
                    )
                    .children(rule.declarations.iter().map(|(name, value)| {
                        h_flex()
                            .w_full()
                            .gap_1()
                            .pl_2()
                            .child(
                                Label::new(format!("{name}:"))
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(value.clone())
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx),
                            )
                    }))
            }))
            .into_any_element()
    }

    fn render_computed(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.computed.is_empty() {
            return nothing_yet("Pick an element to see everything it is painted with.");
        }
        let filter = self.tree_filter.read(cx).text(cx).to_lowercase();
        v_flex()
            .id("browser-tools-computed")
            .size_full()
            .p_1()
            .overflow_y_scroll()
            .children(
                self.computed
                    .iter()
                    .filter(|(name, value)| {
                        filter.is_empty()
                            || name.to_lowercase().contains(&filter)
                            || value.to_lowercase().contains(&filter)
                    })
                    .enumerate()
                    .map(|(which, (name, value))| {
                        h_flex()
                            .id(("computed", which))
                            .debug_selector(move || format!("COMPUTED-{which}"))
                            .w_full()
                            .gap_1()
                            .child(
                                Label::new(name.clone())
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(value.clone())
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx)
                                    .truncate(),
                            )
                    }),
            )
            .into_any_element()
    }

    fn render_layout(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(layout) = self.layout.as_ref() else {
            return nothing_yet("Pick an element to see the box it takes up.");
        };
        let ring = |name: &'static str, sides: [f32; 4], colour: Color, cx: &App| {
            h_flex()
                .w_full()
                .gap_1()
                .child(Label::new(name).size(LabelSize::XSmall).color(Color::Muted))
                .child(
                    Label::new(format!(
                        "{} · {} · {} · {}",
                        trim_number(sides[0]),
                        trim_number(sides[1]),
                        trim_number(sides[2]),
                        trim_number(sides[3])
                    ))
                    .size(LabelSize::XSmall)
                    .buffer_font(cx)
                    .color(colour),
                )
        };
        v_flex()
            .id("browser-tools-layout")
            .debug_selector(|| "LAYOUT".to_string())
            .size_full()
            .p_2()
            .gap_1()
            .overflow_y_scroll()
            .child(
                Label::new(format!(
                    "{} × {} at {}, {}",
                    trim_number(layout.frame.width),
                    trim_number(layout.frame.height),
                    trim_number(layout.frame.left),
                    trim_number(layout.frame.top)
                ))
                .size(LabelSize::Small)
                .buffer_font(cx),
            )
            .child(ring("margin", layout.margin, Color::Warning, cx))
            .child(ring("border", layout.border, Color::Muted, cx))
            .child(ring("padding", layout.padding, Color::Success, cx))
            .child(
                Label::new(format!(
                    "content  {} × {}",
                    trim_number(layout.content.width),
                    trim_number(layout.content.height)
                ))
                .size(LabelSize::XSmall)
                .buffer_font(cx),
            )
            .child(
                Label::new(format!(
                    "{} · {} · box-sizing {} · z {} · overflow {}",
                    layout.display,
                    layout.position,
                    layout.box_sizing,
                    layout.z_index,
                    layout.overflow
                ))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
            .children(layout.flex.as_ref().map(|flex| {
                Label::new(format!(
                    "flex  {} · wrap {} · justify {} · align {} · gap {}",
                    flex.direction, flex.wrap, flex.justify, flex.align, flex.gap
                ))
                .size(LabelSize::XSmall)
                .buffer_font(cx)
                .color(Color::Accent)
            }))
            .children(layout.grid.as_ref().map(|grid| {
                v_flex()
                    .child(
                        Label::new(format!("grid columns  {}", grid.columns))
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(Color::Accent),
                    )
                    .child(
                        Label::new(format!(
                            "grid rows  {} · gap {} · areas {}",
                            grid.rows, grid.gap, grid.areas
                        ))
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(Color::Accent),
                    )
            }))
            .into_any_element()
    }

    fn render_fonts(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(fonts) = self.fonts.as_ref() else {
            return nothing_yet("Pick an element to see the fonts its words are set in.");
        };
        v_flex()
            .id("browser-tools-fonts")
            .debug_selector(|| "FONTS".to_string())
            .size_full()
            .p_2()
            .gap_1()
            .overflow_y_scroll()
            .children(fonts.element.as_ref().map(|element| {
                v_flex()
                    .child(
                        Label::new(element.family.clone())
                            .size(LabelSize::Small)
                            .buffer_font(cx),
                    )
                    .child(
                        Label::new(format!(
                            "{} · weight {} · {} · line height {} · letter spacing {}",
                            element.size,
                            element.weight,
                            element.style,
                            element.height,
                            element.spacing
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                    )
            }))
            .when(!fonts.faces.is_empty(), |this| {
                this.child(heading("What the page has loaded")).children(
                    fonts.faces.iter().enumerate().map(|(which, face)| {
                        h_flex()
                            .id(("face", which))
                            .debug_selector(move || format!("FACE-{which}"))
                            .w_full()
                            .gap_1()
                            .child(
                                Label::new(face.family.clone())
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx),
                            )
                            .child(
                                Label::new(format!("{} {}", face.weight, face.style))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                // A face that was asked for and never arrived is
                                // why words turn up in the wrong font, so how it
                                // ended up is not said quietly.
                                Label::new(face.status.clone())
                                    .size(LabelSize::XSmall)
                                    .color(match face.status.as_str() {
                                        "loaded" => Color::Success,
                                        "error" => Color::Error,
                                        _ => Color::Warning,
                                    }),
                            )
                    }),
                )
            })
            .into_any_element()
    }

    fn render_events(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.picked.is_none() {
            return nothing_yet("Pick an element to see what it listens for.");
        }
        if self.events.is_empty() {
            return nothing_yet(
                "This element listens for nothing. Only what was asked for after the page was \
                 opened is counted, since an engine keeps its own listeners to itself.",
            );
        }
        v_flex()
            .id("browser-tools-events")
            .size_full()
            .p_2()
            .gap_0p5()
            .overflow_y_scroll()
            .children(self.events.iter().enumerate().map(|(which, event)| {
                h_flex()
                    .id(("event", which))
                    .debug_selector(move || format!("EVENT-{which}"))
                    .w_full()
                    .gap_1()
                    .child(
                        Label::new(event.0.clone())
                            .size(LabelSize::Small)
                            .buffer_font(cx),
                    )
                    .when(event.1 > 1, |this| {
                        this.child(
                            Label::new(format!("×{}", event.1))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .child(
                        Label::new(event.2.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
            }))
            .into_any_element()
    }

    // ------------------------------------------------------------------- console

    fn render_console(&self, cx: &mut Context<Self>) -> AnyElement {
        let filter = self.console_filter.read(cx).text(cx).to_lowercase();
        let showing: Vec<&Said> = self
            .said
            .iter()
            .filter(|line| wanted(line, &self.quiet, &filter))
            .collect();
        let counted = format!("{} of {}", showing.len(), self.said.len());
        let lines: Vec<AnyElement> = showing
            .into_iter()
            .enumerate()
            .map(|(which, said)| {
                h_flex()
                    .id(("said", which))
                    .debug_selector(move || format!("SAID-{which}"))
                    .w_full()
                    .gap_1()
                    .items_start()
                    .child(
                        Label::new(mark(&said.level))
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                            .color(colour_of(&said.level)),
                    )
                    .when(said.times > 1, |this| {
                        this.child(
                            Label::new(format!("×{}", said.times))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    })
                    .child(
                        Label::new(said.text.clone())
                            .size(LabelSize::Small)
                            .buffer_font(cx)
                            .color(colour_of(&said.level)),
                    )
                    .when(!said.from.is_empty(), |this| {
                        this.child(div().flex_1()).child(
                            Label::new(said.from.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    })
                    .into_any_element()
            })
            .collect();
        let edge = cx.theme().colors().border;
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .w_full()
                    .flex_none()
                    .gap_1()
                    .px_1()
                    .py_0p5()
                    .border_b_1()
                    .border_color(edge)
                    .children(["error", "warn", "log", "info", "debug"].map(|level| {
                        let quiet = self.quiet.contains(level);
                        Button::new(SharedString::from(format!("level-{level}")), level)
                            .label_size(LabelSize::XSmall)
                            .toggle_state(!quiet)
                            .on_click(cx.listener(move |panel, _, _, cx| {
                                if !panel.quiet.remove(level) {
                                    panel.quiet.insert(level.to_string());
                                }
                                cx.notify();
                            }))
                    }))
                    .child(div().w(px(140.)).child(self.console_filter.clone()))
                    .child(div().flex_1())
                    .child(
                        Label::new(counted)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        IconButton::new("clear-console", IconName::Eraser)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Clear what has been said"))
                            .on_click(cx.listener(|panel, _, _, cx| {
                                panel.said.clear();
                                cx.notify();
                            })),
                    ),
            )
            // Said plainly rather than left blank: a page that has logged nothing
            // is not a panel that is broken, and the reader cannot tell the two
            // apart from an empty box.
            .when(self.said.is_empty(), |console| {
                console.child(
                    div().p_2().child(
                        Label::new(
                            "This page has not said anything. Whatever it logs appears here; \
                             what you type below runs in the page, where $0 is the element you \
                             picked and $_ the last answer.",
                        )
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
                )
            })
            .child(
                v_flex()
                    .id("browser-tools-said")
                    .flex_1()
                    .min_h_0()
                    .p_1()
                    .gap_0p5()
                    .overflow_y_scroll()
                    .children(lines),
            )
            .child(
                h_flex()
                    .w_full()
                    .flex_none()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(edge)
                    .key_context("BrowserToolsConsole")
                    .on_action(cx.listener(|panel, _: &menu::Confirm, window, cx| {
                        panel.run_it(window, cx);
                    }))
                    .capture_action(cx.listener(
                        |panel, _: &zed_actions::editor::MoveUp, window, cx| {
                            panel.walk_history(true, window, cx);
                        },
                    ))
                    .capture_action(cx.listener(
                        |panel, _: &zed_actions::editor::MoveDown, window, cx| {
                            panel.walk_history(false, window, cx);
                        },
                    ))
                    .child(self.console.clone()),
            )
            .into_any_element()
    }

    // ------------------------------------------------------------------- network

    fn render_network(&self, cx: &mut Context<Self>) -> AnyElement {
        let filter = self.wire_filter.read(cx).text(cx).to_lowercase();
        let showing: Vec<&Wire> = self
            .wires
            .iter()
            .filter(|wire| on_the_list(wire, &self.hidden_kinds, &filter))
            .collect();
        let slowest = showing.iter().map(|wire| wire.ms).max().unwrap_or(1).max(1);
        let sent: u64 = showing.iter().map(|wire| wire.size).sum();
        let finished = showing
            .iter()
            .map(|wire| wire.start + wire.ms)
            .max()
            .unwrap_or(0);
        let how_many = self.wires.len();
        let rows: Vec<AnyElement> = showing
            .into_iter()
            .map(|wire| self.render_wire(wire, slowest, cx))
            .collect();
        let detail = self
            .wire
            .as_ref()
            .map(|detail| self.render_detail(detail, cx));
        let has_detail = detail.is_some();
        let body = if how_many == 0 {
            nothing_yet(
                "This page has not fetched anything. Every request it makes is listed here, \
                 with what it cost.",
            )
        } else {
            h_flex()
                .flex_1()
                .min_h_0()
                .items_start()
                .child(
                    v_flex()
                        .id("browser-tools-network")
                        .h_full()
                        .when(has_detail, |this| this.w_1_2())
                        .when(!has_detail, |this| this.w_full())
                        .overflow_y_scroll()
                        .children(rows),
                )
                .children(detail)
                .into_any_element()
        };
        let edge = cx.theme().colors().border;
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .w_full()
                    .flex_none()
                    .gap_1()
                    .px_1()
                    .py_0p5()
                    .border_b_1()
                    .border_color(edge)
                    .children(Kind::ALL.map(|kind| {
                        Button::new(("kind", kind as usize), kind.label())
                            .label_size(LabelSize::XSmall)
                            .toggle_state(!self.hidden_kinds.contains(&kind))
                            .on_click(cx.listener(move |panel, _, _, cx| {
                                if !panel.hidden_kinds.remove(&kind) {
                                    panel.hidden_kinds.insert(kind);
                                }
                                cx.notify();
                            }))
                    }))
                    .child(div().w(px(140.)).child(self.wire_filter.clone())),
            )
            .child(body)
            .child(
                h_flex()
                    .w_full()
                    .flex_none()
                    .gap_2()
                    .px_2()
                    .py_0p5()
                    .border_t_1()
                    .border_color(edge)
                    .child(
                        Label::new(format!("{how_many} requests"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(size_text(sent))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(format!("finished at {finished} ms"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .into_any_element()
    }

    fn render_wire(&self, wire: &Wire, slowest: u64, cx: &mut Context<Self>) -> AnyElement {
        let id = wire.id;
        let chosen = self.chosen_wire == Some(id);
        let room = 60.;
        let width = (wire.ms as f32 / slowest as f32 * room).max(2.);
        let bar = cx.theme().colors().text_accent;
        h_flex()
            .id(SharedString::from(format!("wire-{id}")))
            .debug_selector(move || format!("WIRE-{id}"))
            .w_full()
            .gap_1()
            .px_1()
            .when(chosen, |this| this.bg(cx.theme().colors().element_selected))
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .child(
                div().w(px(34.)).flex_none().child(
                    Label::new(wire.method.clone())
                        .size(LabelSize::XSmall)
                        .buffer_font(cx)
                        .color(Color::Muted),
                ),
            )
            .child(
                div().w(px(26.)).flex_none().child(
                    Label::new(match wire.status {
                        0 => "—".to_string(),
                        status => status.to_string(),
                    })
                    .size(LabelSize::XSmall)
                    .buffer_font(cx)
                    .color(match wire.status {
                        0 => Color::Muted,
                        status if status >= 400 => Color::Error,
                        status if status >= 300 => Color::Warning,
                        _ => Color::Success,
                    }),
                ),
            )
            .child(
                div().w(px(52.)).flex_none().child(
                    Label::new(kind_of(&wire.kind, &wire.mime, &wire.url).label())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            )
            .child(
                div().flex_1().min_w_0().child(
                    Label::new(shorten(&wire.url))
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                        .truncate(),
                ),
            )
            .child(
                div().w(px(56.)).flex_none().child(
                    Label::new(size_text(wire.size))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            )
            .child(
                div().w(px(52.)).flex_none().child(
                    Label::new(format!("{} ms", wire.ms))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                ),
            )
            .child(
                div()
                    .w(px(room))
                    .flex_none()
                    .child(div().h(px(4.)).w(px(width)).rounded_sm().bg(bar)),
            )
            .on_click(cx.listener(move |panel, _, _, cx| {
                panel.chosen_wire = Some(id);
                panel.wire = None;
                panel.put(Ask::Request, format!("request({id})"), cx);
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_detail(&self, detail: &Detail, cx: &mut Context<Self>) -> AnyElement {
        let slowest = detail
            .phases
            .iter()
            .map(|(_, ms)| *ms)
            .fold(1., f32::max)
            .max(1.);
        let curl = curl_of(detail);
        let bar = cx.theme().colors().text_accent;
        let edge = cx.theme().colors().border;
        v_flex()
            .id("browser-tools-request")
            .h_full()
            .w_1_2()
            .min_w_0()
            .p_2()
            .gap_1()
            .overflow_y_scroll()
            .border_l_1()
            .border_color(edge)
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        Label::new(format!("{} {}", detail.method, detail.status))
                            .size(LabelSize::Small)
                            .buffer_font(cx),
                    )
                    .child(
                        Label::new(detail.status_text.clone())
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("copy-curl", "Copy as cURL")
                            .label_size(LabelSize::XSmall)
                            .on_click(
                                cx.listener(move |panel, _, _, cx| panel.copy(curl.clone(), cx)),
                            ),
                    )
                    .child(
                        IconButton::new("close-request", IconName::Close)
                            .icon_size(IconSize::XSmall)
                            .on_click(cx.listener(|panel, _, _, cx| {
                                panel.chosen_wire = None;
                                panel.wire = None;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                Label::new(detail.url.clone())
                    .size(LabelSize::XSmall)
                    .buffer_font(cx)
                    .color(Color::Muted),
            )
            .child(
                Label::new(format!(
                    "{} · {} · {} ms",
                    match detail.mime.is_empty() {
                        true => "no content type".to_string(),
                        false => detail.mime.clone(),
                    },
                    size_text(detail.size),
                    detail.ms
                ))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
            .child(heading("What it cost"))
            .children(
                detail
                    .phases
                    .iter()
                    .filter(|(_, ms)| *ms > 0.)
                    .map(|(name, ms)| {
                        h_flex()
                            .w_full()
                            .gap_1()
                            .child(
                                div().w(px(56.)).flex_none().child(
                                    Label::new(name.clone())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                            )
                            .child(
                                div()
                                    .h(px(4.))
                                    .w(px((ms / slowest * 90.).max(2.)))
                                    .rounded_sm()
                                    .bg(bar),
                            )
                            .child(
                                Label::new(format!("{} ms", trim_number(*ms)))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                    }),
            )
            .when(!detail.asked.is_empty(), |this| {
                this.child(heading("What was asked")).children(
                    detail
                        .asked
                        .iter()
                        .map(|(name, value)| header_line(name, value, cx)),
                )
            })
            .when(!detail.answered.is_empty(), |this| {
                this.child(heading("What came back")).children(
                    detail
                        .answered
                        .iter()
                        .map(|(name, value)| header_line(name, value, cx)),
                )
            })
            .when(!detail.body.is_empty(), |this| {
                this.child(heading("The answer itself")).child(
                    Label::new(shape(&detail.body, &detail.mime))
                        .size(LabelSize::XSmall)
                        .buffer_font(cx),
                )
            })
            .into_any_element()
    }

    // --------------------------------------------------------------------- style

    fn render_style(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.sheets.is_empty() {
            return nothing_yet(
                "This page has no stylesheets of its own. Whatever it loads is listed here, and \
                 each one can be turned off.",
            );
        }
        let edge = cx.theme().colors().border;
        let rows: Vec<AnyElement> = self
            .sheets
            .iter()
            .map(|sheet| self.render_sheet_row(sheet, cx))
            .collect();
        h_flex()
            .size_full()
            .items_start()
            .child(
                v_flex()
                    .id("browser-tools-sheets")
                    .h_full()
                    .w_1_3()
                    .overflow_y_scroll()
                    .children(rows),
            )
            .child(
                v_flex()
                    .id("browser-tools-sheet")
                    .h_full()
                    .w_2_3()
                    .min_w_0()
                    .p_2()
                    .overflow_y_scroll()
                    .border_l_1()
                    .border_color(edge)
                    .children(self.sheet.as_ref().map(|sheet| {
                        v_flex()
                            .child(
                                Label::new(format!("{}  ({} rules)", sheet.name, sheet.rules))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new(sheet.text.clone())
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx),
                            )
                    }))
                    .when(self.sheet.is_none(), |this| {
                        this.child(
                            Label::new("Pick a stylesheet to read it.")
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_sheet_row(&self, sheet: &Sheet, cx: &mut Context<Self>) -> AnyElement {
        let id = sheet.id;
        let chosen = self.chosen_sheet == Some(id);
        let off = sheet.disabled;
        h_flex()
            .id(("sheet", id))
            .debug_selector(move || format!("SHEET-{id}"))
            .w_full()
            .gap_1()
            .px_1()
            .when(chosen, |this| this.bg(cx.theme().colors().element_selected))
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .child(
                IconButton::new(
                    ("sheet-eye", id),
                    match off {
                        true => IconName::EyeOff,
                        false => IconName::Eye,
                    },
                )
                .icon_size(IconSize::XSmall)
                .tooltip(Tooltip::text("Turn this stylesheet off"))
                .on_click(cx.listener(move |panel, _, _, cx| {
                    panel.put(Ask::Nothing, format!("toggleSheet({id})"), cx);
                })),
            )
            .child(
                Label::new(sheet.name.clone())
                    .size(LabelSize::Small)
                    .buffer_font(cx)
                    .color(match off {
                        true => Color::Muted,
                        false => Color::Default,
                    })
                    .truncate(),
            )
            .child(
                Label::new(format!("{} rules", sheet.rules))
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .when(!sheet.media.is_empty(), |this| {
                this.child(
                    Label::new(sheet.media.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Accent),
                )
            })
            .when(!sheet.readable, |this| {
                this.child(
                    Label::new("not readable")
                        .size(LabelSize::XSmall)
                        .color(Color::Warning),
                )
            })
            .on_click(cx.listener(move |panel, _, _, cx| {
                panel.chosen_sheet = Some(id);
                panel.sheet = None;
                panel.put(Ask::SheetText, format!("sheet({id})"), cx);
                cx.notify();
            }))
            .into_any_element()
    }

    // ------------------------------------------------------------------- storage

    fn render_storage(&self, cx: &mut Context<Self>) -> AnyElement {
        let empty = self.stores.cookies.is_empty()
            && self.stores.local.is_empty()
            && self.stores.session.is_empty()
            && self.stores.databases.is_empty()
            && self.stores.caches.is_empty()
            && self.installed.workers.is_empty()
            && self.installed.manifest.is_empty();
        if empty {
            return nothing_yet(
                "This page keeps nothing: no cookies, no local or session storage, no \
                 databases. Whatever it keeps appears here.",
            );
        }
        let cookies = self.render_store(Store::Cookie, &self.stores.cookies, cx);
        let local = self.render_store(Store::Local, &self.stores.local, cx);
        let session = self.render_store(Store::Session, &self.stores.session, cx);
        v_flex()
            .id("browser-tools-storage")
            .size_full()
            .p_1()
            .gap_1()
            .overflow_y_scroll()
            .child(cookies)
            .child(local)
            .child(session)
            .when(!self.stores.databases.is_empty(), |this| {
                this.child(heading("Databases"))
                    .children(self.stores.databases.iter().map(|name| {
                        Label::new(name.clone())
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                    }))
            })
            .when(!self.stores.caches.is_empty(), |this| {
                this.child(heading("Caches"))
                    .children(self.stores.caches.iter().map(|name| {
                        Label::new(name.clone())
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                    }))
            })
            .when(
                self.installed.workers.is_empty() && !self.installed.supported,
                |this| {
                    this.child(heading("Service workers")).child(
                        Label::new("This engine has none, so a page cannot install one.")
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                },
            )
            .when(!self.installed.workers.is_empty(), |this| {
                this.child(heading("Service workers")).children(
                    self.installed
                        .workers
                        .iter()
                        .enumerate()
                        .map(|(which, worker)| {
                            div()
                                .id(("worker", which))
                                .debug_selector(move || format!("WORKER-{which}"))
                                .child(
                                    Label::new(worker.clone())
                                        .size(LabelSize::XSmall)
                                        .buffer_font(cx),
                                )
                        }),
                )
            })
            .when(!self.installed.manifest.is_empty(), |this| {
                this.child(heading("Manifest")).child(
                    Label::new(self.installed.manifest.clone())
                        .size(LabelSize::XSmall)
                        .buffer_font(cx),
                )
            })
            .into_any_element()
    }

    fn render_store(
        &self,
        store: Store,
        rows: &[(String, String)],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let how_many = rows.len();
        v_flex()
            .w_full()
            .gap_0p5()
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(heading(store.title()))
                    .child(
                        Label::new(format!("{how_many}"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .when(how_many > 0, |this| {
                        this.child(
                            Button::new(
                                SharedString::from(format!("clear-store-{}", store.word())),
                                "Clear",
                            )
                            .label_size(LabelSize::XSmall)
                            .on_click(cx.listener(
                                move |panel, _, _, cx| {
                                    panel.put_about(
                                        Ask::Nothing,
                                        "clearStore",
                                        &[store.word()],
                                        cx,
                                    );
                                },
                            )),
                        )
                    }),
            )
            .children(rows.iter().enumerate().map(|(which, (key, value))| {
                let key = key.clone();
                h_flex()
                    .id((store.word(), which))
                    .debug_selector(move || format!("STORE-{}-{which}", store.word()))
                    .w_full()
                    .gap_1()
                    .child(
                        div().w(px(140.)).flex_none().child(
                            Label::new(key.clone())
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .truncate(),
                        ),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Label::new(value.clone())
                                .size(LabelSize::XSmall)
                                .buffer_font(cx)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                    )
                    .child(
                        IconButton::new((store.word(), which + 1000), IconName::Trash)
                            .icon_size(IconSize::XSmall)
                            .tooltip(Tooltip::text("Take this out of the page's store"))
                            .on_click(cx.listener(move |panel, _, _, cx| {
                                panel.put_about(
                                    Ask::Nothing,
                                    "forget",
                                    &[store.word(), key.as_str()],
                                    cx,
                                );
                            })),
                    )
            }))
            .into_any_element()
    }

    // --------------------------------------------------------------- performance

    fn render_performance(&self, cx: &mut Context<Self>) -> AnyElement {
        let counts = &self.cost.counts;
        let longest = self
            .cost
            .phases
            .iter()
            .map(|(_, ms)| *ms)
            .fold(1., f32::max)
            .max(1.);
        let frames = self.frames;
        let bar = cx.theme().colors().text_accent;
        v_flex()
            .id("browser-tools-performance")
            .size_full()
            .p_2()
            .gap_1()
            .overflow_y_scroll()
            .when(self.cost.phases.is_empty(), |this| {
                this.child(
                    Label::new(
                        "This page has not said how long it took to arrive. What it holds is \
                         still counted below.",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
            })
            .when(!self.cost.phases.is_empty(), |this| {
                this.child(heading("How it arrived"))
                    .children(self.cost.phases.iter().map(|(name, ms)| {
                        let named = name.clone();
                        h_flex()
                            .debug_selector(move || format!("PHASE-{named}"))
                            .w_full()
                            .gap_1()
                            .child(
                                div().w(px(80.)).flex_none().child(
                                    Label::new(name.clone())
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                            )
                            .child(
                                div()
                                    .h(px(5.))
                                    .w(px((ms / longest * 140.).max(2.)))
                                    .rounded_sm()
                                    .bg(bar),
                            )
                            .child(
                                Label::new(format!("{} ms", trim_number(*ms)))
                                    .size(LabelSize::XSmall)
                                    .buffer_font(cx),
                            )
                    }))
            })
            .when(!self.cost.paints.is_empty(), |this| {
                this.child(heading("When it first showed"))
                    .children(self.cost.paints.iter().map(|(name, ms)| {
                        Label::new(format!("{name}  {} ms", trim_number(*ms)))
                            .size(LabelSize::XSmall)
                            .buffer_font(cx)
                    }))
            })
            .children(frames.map(|frames| {
                v_flex()
                    .child(heading("What a frame costs the engine"))
                    .child(
                        Label::new(format!(
                            "{} frames at {}×{} · {} ms at the middle · {} ms at worst · \
                             {} turns of the engine each",
                            frames.frames,
                            frames.width,
                            frames.height,
                            trim_number(frames.middle_ms),
                            trim_number(frames.worst_ms),
                            frames.turns
                        ))
                        .size(LabelSize::XSmall)
                        .buffer_font(cx),
                    )
            }))
            .child(heading("What it holds"))
            .child(
                Label::new(format!(
                    "{} elements · {} characters of text · {} images · {} scripts",
                    counts.elements, counts.text, counts.images, counts.scripts
                ))
                .size(LabelSize::XSmall)
                .buffer_font(cx),
            )
            .child(
                Label::new(format!(
                    "{} stylesheets with {} rules · {} things it listens for · {} requests, \
                     {} in all",
                    counts.stylesheets,
                    counts.rules,
                    counts.listeners,
                    counts.requests,
                    size_text(counts.transferred)
                ))
                .size(LabelSize::XSmall)
                .buffer_font(cx),
            )
            .children(self.cost.memory.as_ref().map(|memory| {
                Label::new(format!(
                    "{} of script memory in use, {} taken",
                    size_text(memory.used),
                    size_text(memory.total)
                ))
                .size(LabelSize::XSmall)
                .buffer_font(cx)
            }))
            .into_any_element()
    }

    // ------------------------------------------------------------- accessibility

    fn render_accessibility(&self, cx: &mut Context<Self>) -> AnyElement {
        let to_fix = self
            .findings
            .iter()
            .filter(|one| one.level == "error")
            .count();
        let worth_a_look = self.findings.len() - to_fix;
        let rows: Vec<AnyElement> = self
            .findings
            .iter()
            .enumerate()
            .map(|(which, finding)| self.render_finding(which, finding, cx))
            .collect();
        let edge = cx.theme().colors().border;
        let body = if rows.is_empty() {
            nothing_yet(
                "Nothing found that would stand between this page and a reader who cannot see \
                 it or cannot use a mouse.",
            )
        } else {
            v_flex()
                .id("browser-tools-findings")
                .flex_1()
                .min_h_0()
                .p_1()
                .gap_0p5()
                .overflow_y_scroll()
                .children(rows)
                .into_any_element()
        };
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .w_full()
                    .flex_none()
                    .gap_1()
                    .px_1()
                    .py_0p5()
                    .border_b_1()
                    .border_color(edge)
                    .child(
                        Button::new("read-again", "Check the page")
                            .label_size(LabelSize::XSmall)
                            .on_click(cx.listener(|panel, _, _, cx| panel.read_the_page(cx))),
                    )
                    .child(
                        Button::new("tab-order", "Show the tab order")
                            .label_size(LabelSize::XSmall)
                            .toggle_state(self.numbering)
                            .on_click(cx.listener(|panel, _, _, cx| panel.toggle_numbering(cx))),
                    )
                    .child(div().flex_1())
                    .child(
                        Label::new(format!("{to_fix} to fix · {worth_a_look} worth a look"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(body)
            .into_any_element()
    }

    // -------------------------------------------------------------------- device

    fn render_device(&self, cx: &mut Context<Self>) -> AnyElement {
        let shown_at = self
            .device
            .map(|at| (f32::from(at.width), f32::from(at.height)));
        v_flex()
            .id("browser-tools-device")
            .size_full()
            .p_2()
            .gap_1()
            .overflow_y_scroll()
            .child(heading("Show the page as a device would"))
            .child(
                Label::new(
                    "The page is laid out again at the size below, so what is on screen is \
                     what a reader on that device would get rather than a picture of it made \
                     smaller.",
                )
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .flex_wrap()
                    .children(DEVICES.iter().enumerate().map(|(which, device)| {
                        let (name, wide, tall) = *device;
                        let chosen = shown_at == Some((wide, tall));
                        div()
                            .id(("device-hitbox", which))
                            .debug_selector(move || format!("DEVICE-{name}"))
                            .child(
                                Button::new(("device", which), format!("{name}  {wide}×{tall}"))
                                    .label_size(LabelSize::XSmall)
                                    .toggle_state(chosen)
                                    .on_click(cx.listener(move |panel, _, _, cx| {
                                        panel.show_the_page_at(
                                            Some(gpui::size(px(wide), px(tall))),
                                            cx,
                                        );
                                    })),
                            )
                    })),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        div()
                            .id("device-full-hitbox")
                            .debug_selector(|| "DEVICE-Full".to_string())
                            .child(
                                Button::new("device-full", "The pane's own size")
                                    .label_size(LabelSize::XSmall)
                                    .toggle_state(shown_at.is_none())
                                    .on_click(cx.listener(|panel, _, _, cx| {
                                        panel.show_the_page_at(None, cx)
                                    })),
                            ),
                    )
                    .when_some(shown_at, |this, (wide, tall)| {
                        this.child(
                            Button::new("device-turn", "Turn it")
                                .label_size(LabelSize::XSmall)
                                .tooltip(Tooltip::text("Stand the device the other way up"))
                                .on_click(cx.listener(move |panel, _, _, cx| {
                                    panel
                                        .show_the_page_at(Some(gpui::size(px(tall), px(wide))), cx);
                                })),
                        )
                    }),
            )
            .child(
                Label::new(match shown_at {
                    Some((wide, tall)) => {
                        format!("The page is being laid out at {wide}×{tall}.")
                    }
                    None => "The page is being laid out at whatever the pane comes to.".into(),
                })
                .size(LabelSize::XSmall)
                .buffer_font(cx),
            )
            .into_any_element()
    }

    fn render_finding(
        &self,
        which: usize,
        finding: &Finding,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let at = finding.at;
        h_flex()
            .id(("finding", which))
            .debug_selector(move || format!("FINDING-{which}"))
            .w_full()
            .gap_1()
            .items_start()
            .hover(|style| style.bg(cx.theme().colors().element_hover))
            .child(
                div().w(px(96.)).flex_none().child(
                    Label::new(finding.rule.clone())
                        .size(LabelSize::XSmall)
                        .color(match finding.level.as_str() {
                            "error" => Color::Error,
                            _ => Color::Warning,
                        }),
                ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Label::new(finding.text.clone()).size(LabelSize::XSmall)),
            )
            .child(
                Label::new(finding.selector.clone())
                    .size(LabelSize::XSmall)
                    .buffer_font(cx)
                    .color(Color::Muted)
                    .truncate(),
            )
            .on_click(cx.listener(move |panel, _, _, cx| {
                if at >= 0 {
                    panel.showing = Tools::Elements;
                    panel.pick(at as usize, cx);
                }
            }))
            .into_any_element()
    }
}

/// One line of what the console answered, as either an answer or a complaint.
/// The page marks a script that threw with two exclamation marks, because an
/// answer and an error come back the same way.
fn said_by_the_page(answer: String) -> Said {
    let complaint = answer.starts_with("!!");
    Said {
        level: match complaint {
            true => "error".into(),
            false => "answer".into(),
        },
        text: match complaint {
            true => answer.trim_start_matches('!').to_string(),
            false => answer,
        },
        from: String::new(),
        times: 1,
    }
}

/// Whether a side of the picked element is worth reading again on every turn,
/// or only when the reader picks something.
///
/// The box an element takes up moves as the page does, and so do the fonts it
/// resolves to and what it listens for -- all of them cost one look at the
/// element. Which rules reach it costs a walk over every rule of every
/// stylesheet the page has, which on a page built from a framework is tens of
/// thousands of them, and the answer does not change unless the page's
/// stylesheets do.
fn worth_reading_again(side: Side) -> bool {
    !matches!(side, Side::Rules)
}

/// The sizes a reader is most likely to want to see a page at. Not every device
/// ever made: a list nobody reads through is worse than five sizes that cover
/// what a page has to work at.
const DEVICES: [(&str, f32, f32); 6] = [
    ("Phone", 375., 667.),
    ("Large phone", 414., 896.),
    ("Tablet", 768., 1024.),
    ("Laptop", 1280., 800.),
    ("Desktop", 1920., 1080.),
    ("Narrow", 320., 568.),
];

/// Which rows of the tree are on screen: everything, less what is inside a
/// folded branch, and -- when the reader is looking for something -- only the
/// rows that match together with the branches that hold them.
fn showing_rows(rows: &[Row], folded: &HashSet<usize>, looking_for: &str) -> Vec<usize> {
    let mut showing = Vec::with_capacity(rows.len());
    if !looking_for.is_empty() {
        // A match is no use without what it sits in, so every match brings its
        // own line of ancestors with it.
        let mut wanted = vec![false; rows.len()];
        for (which, row) in rows.iter().enumerate() {
            if !row.text.to_lowercase().contains(looking_for)
                && !row.preview.to_lowercase().contains(looking_for)
            {
                continue;
            }
            wanted[which] = true;
            let mut depth = row.depth;
            for above in (0..which).rev() {
                if rows[above].depth < depth {
                    wanted[above] = true;
                    depth = rows[above].depth;
                    if depth == 0 {
                        break;
                    }
                }
            }
        }
        for (which, keep) in wanted.into_iter().enumerate() {
            if keep {
                showing.push(which);
            }
        }
        return showing;
    }
    let mut hidden_below: Option<usize> = None;
    for (which, row) in rows.iter().enumerate() {
        match hidden_below {
            Some(depth) if row.depth > depth => continue,
            _ => hidden_below = None,
        }
        showing.push(which);
        if folded.contains(&row.at) {
            hidden_below = Some(row.depth);
        }
    }
    showing
}

/// Whether one line the page said is one the reader asked to see.
fn wanted(line: &Said, quiet: &HashSet<String>, looking_for: &str) -> bool {
    if quiet.contains(&line.level) {
        return false;
    }
    looking_for.is_empty() || line.text.to_lowercase().contains(looking_for)
}

/// Whether one request is one the chips and the filter leave showing.
fn on_the_list(wire: &Wire, hidden: &HashSet<Kind>, looking_for: &str) -> bool {
    if hidden.contains(&kind_of(&wire.kind, &wire.mime, &wire.url)) {
        return false;
    }
    looking_for.is_empty() || wire.url.to_lowercase().contains(looking_for)
}

/// The findings, worst first: what a reader cannot get past before what is only
/// worth a look.
fn worst_first(findings: &mut [Finding]) {
    findings.sort_by_key(|finding| match finding.level.as_str() {
        "error" => 0,
        "warn" => 1,
        _ => 2,
    });
}

/// The request as a line that fetches the same thing again from a terminal.
fn curl_of(detail: &Detail) -> String {
    let quote = |text: &str| text.replace('\'', "'\\''");
    let mut line = format!("curl '{}'", quote(&detail.url));
    if detail.method != "GET" {
        line.push_str(&format!(" -X {}", detail.method));
    }
    for (name, value) in &detail.asked {
        line.push_str(&format!(" -H '{}: {}'", quote(name), quote(value)));
    }
    line
}

/// The two-letter mark that says where a line came from.
fn mark(level: &str) -> &'static str {
    match level {
        "error" => "!!",
        "warn" => " !",
        "asked" => " >",
        "answer" => " <",
        "trace" => " ⋮",
        "group" => " ▾",
        _ => "  ",
    }
}

fn colour_of(level: &str) -> Color {
    match level {
        "error" => Color::Error,
        "warn" => Color::Warning,
        "asked" => Color::Muted,
        "answer" => Color::Accent,
        "debug" | "trace" => Color::Muted,
        _ => Color::Default,
    }
}

fn heading(text: &'static str) -> AnyElement {
    Label::new(text)
        .size(LabelSize::XSmall)
        .color(Color::Accent)
        .into_any_element()
}

fn header_line(name: &str, value: &str, cx: &App) -> AnyElement {
    h_flex()
        .w_full()
        .gap_1()
        .items_start()
        .child(
            div().w(px(110.)).flex_none().child(
                Label::new(name.to_string())
                    .size(LabelSize::XSmall)
                    .buffer_font(cx)
                    .color(Color::Muted),
            ),
        )
        .child(
            div().flex_1().min_w_0().child(
                Label::new(value.to_string())
                    .size(LabelSize::XSmall)
                    .buffer_font(cx),
            ),
        )
        .into_any_element()
}

fn nothing_yet(text: &'static str) -> AnyElement {
    div()
        .p_2()
        .child(Label::new(text).size(LabelSize::Small).color(Color::Muted))
        .into_any_element()
}

/// A number without the noise of a decimal point it does not need.
fn trim_number(value: f32) -> String {
    if (value - value.round()).abs() < 0.01 {
        format!("{}", value.round() as i64)
    } else {
        format!("{value:.2}")
    }
}

fn size_text(bytes: u64) -> String {
    match bytes {
        0 => "cached".to_string(),
        bytes if bytes < 1024 => format!("{bytes} B"),
        bytes if bytes < 1024 * 1024 => format!("{} kB", bytes / 1024),
        bytes => format!("{:.1} MB", bytes as f64 / (1024. * 1024.)),
    }
}

/// An address as much of it as is worth showing in a row.
fn shorten(name: &str) -> String {
    let tail = name.rsplit('/').next().unwrap_or(name);
    match tail.len() {
        0 => name.chars().take(80).collect(),
        _ => tail.chars().take(80).collect(),
    }
}

/// The answer to a request, laid out if it is JSON and left alone if it is not.
fn shape(body: &str, mime: &str) -> String {
    if mime.contains("json") || body.trim_start().starts_with(['{', '[']) {
        return pretty(body);
    }
    body.to_string()
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

impl Focusable for BrowserToolsPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for BrowserToolsPanel {}

impl Render for BrowserToolsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let showing = self.showing;
        let body = match showing {
            Tools::Elements => self.render_elements(cx),
            Tools::Console => self.render_console(cx),
            Tools::Network => self.render_network(cx),
            Tools::Style => self.render_style(cx),
            Tools::Storage => self.render_storage(cx),
            Tools::Performance => self.render_performance(cx),
            Tools::Accessibility => self.render_accessibility(cx),
            Tools::Device => self.render_device(cx),
        };
        let edge = cx.theme().colors().border;
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
                    .border_color(edge)
                    .child(
                        h_flex()
                            .id("browser-tools-tabs")
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .overflow_x_scroll()
                            .children(Tools::ALL.map(|tools| {
                                div()
                                    .id(("tools-hitbox", tools as usize))
                                    .flex_none()
                                    .debug_selector(move || format!("TOOLS-{}", tools.label()))
                                    .child(
                                        Button::new(("tools", tools as usize), tools.label())
                                            .label_size(LabelSize::Small)
                                            .toggle_state(tools == showing)
                                            .on_click(cx.listener(move |panel, _, _, cx| {
                                                panel.show(tools, cx)
                                            })),
                                    )
                            })),
                    )
                    // The tools beside the tabs keep their room whatever the
                    // dock's width: it is the row of tabs that scrolls, so
                    // nothing here can be pushed off the panel's edge.
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .child(
                                IconButton::new("pick-an-element", IconName::Crosshair)
                                    .icon_size(IconSize::Small)
                                    .toggle_state(self.picking)
                                    .tooltip(Tooltip::text("Pick an element out of the page"))
                                    .on_click(
                                        cx.listener(|panel, _, _, cx| panel.start_picking(cx)),
                                    ),
                            )
                            .child(
                                Button::new("rulers", "Rulers")
                                    .label_size(LabelSize::XSmall)
                                    .toggle_state(self.ruled)
                                    .tooltip(Tooltip::text("Lay a grid over the page"))
                                    .on_click(
                                        cx.listener(|panel, _, _, cx| panel.toggle_rulers(cx)),
                                    ),
                            )
                            .child(
                                Button::new("measure", "Measure")
                                    .label_size(LabelSize::XSmall)
                                    .toggle_state(self.measuring)
                                    .tooltip(Tooltip::text("Drag across the page to measure it"))
                                    .on_click(
                                        cx.listener(|panel, _, _, cx| panel.toggle_measure(cx)),
                                    ),
                            )
                            .when(showing == Tools::Elements, |this| {
                                this.child(
                                    div()
                                        .w(px(140.))
                                        .flex_none()
                                        .child(self.tree_filter.clone()),
                                )
                            })
                            // The way out, from the tools themselves rather than
                            // from the dock's own handle.
                            .child(
                                div()
                                    .id("close-tools-hitbox")
                                    .debug_selector(|| "TOOLS-CLOSE".to_string())
                                    .child(
                                        IconButton::new("close-tools", IconName::Close)
                                            .icon_size(IconSize::Small)
                                            .tooltip(Tooltip::text("Close the tools"))
                                            .on_click(cx.listener(|panel, _, _, cx| {
                                                panel.put_the_tools_away(cx);
                                                cx.emit(PanelEvent::Close);
                                            })),
                                    ),
                            ),
                    ),
            )
            .child(div().flex_1().min_h_0().child(body))
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

    /// Told by the dock when this panel comes to the front and when it goes
    /// behind. A panel behind another one asks the page nothing at all, and one
    /// coming to the front asks at once rather than after the next turn, so it
    /// is filled by the time the reader has looked at it.
    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.told_active = Some(active);
        // Put off until the dock has finished: it says this from inside its own
        // update of the workspace, and asking the page anything means reading
        // the workspace to find it.
        cx.defer_in(window, move |panel, _window, cx| {
            if active {
                panel.ask_the_page(cx);
            } else {
                panel.put_the_tools_away(cx);
            }
            cx.notify();
        });
        cx.notify();
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Bounds, Modifiers, TestAppContext, VisualTestContext};
    use project::Project;
    use workspace::AppState;

    const A_TREE: &str = r#"[
        {"at":0,"depth":0,"text":"html","children":2,"preview":"","listens":0},
        {"at":1,"depth":1,"text":"head","children":1,"preview":"","listens":0},
        {"at":2,"depth":2,"text":"title","children":0,"preview":"A page","listens":0},
        {"at":3,"depth":1,"text":"body","children":2,"preview":"","listens":0},
        {"at":4,"depth":2,"text":"div#sheet","children":1,"preview":"","listens":1},
        {"at":5,"depth":3,"text":"p.line","children":0,"preview":"Some words","listens":0},
        {"at":6,"depth":2,"text":"footer","children":0,"preview":"The end","listens":0}
    ]"#;

    fn a_row(at: usize, depth: usize, text: &str, children: usize, preview: &str) -> Row {
        Row {
            at,
            depth,
            text: text.to_string(),
            children,
            preview: preview.to_string(),
            listens: 0,
        }
    }

    fn the_tree() -> Vec<Row> {
        serde_json::from_str::<Vec<Row>>(A_TREE).expect("the tree above has to parse")
    }

    /// Folding a branch has to take away what is inside it, and nothing that
    /// merely comes after it: the rows are a flat list in document order, so a
    /// fold that goes by position rather than by depth swallows the rest of the
    /// page.
    #[test]
    fn a_folded_branch_hides_its_own_children_and_nothing_else() {
        let rows = the_tree();
        let nothing_folded = showing_rows(&rows, &HashSet::new(), "");
        assert_eq!(nothing_folded.len(), rows.len());

        let folded = HashSet::from([3]);
        let showing = showing_rows(&rows, &folded, "");
        let names: Vec<&str> = showing
            .iter()
            .map(|which| rows[*which].text.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["html", "head", "title", "body"],
            "folding body has to hide what is inside body"
        );

        let folded = HashSet::from([1]);
        let showing = showing_rows(&rows, &folded, "");
        let names: Vec<&str> = showing
            .iter()
            .map(|which| rows[*which].text.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["html", "head", "body", "div#sheet", "p.line", "footer"],
            "folding head must not touch body, which comes after it"
        );
    }

    /// A row that matches is no use on its own -- the reader cannot tell where
    /// in the page it sits -- so every match brings the branches that hold it.
    #[test]
    fn looking_for_something_keeps_the_matches_and_what_holds_them() {
        let rows = the_tree();
        let showing = showing_rows(&rows, &HashSet::new(), "p.line");
        let names: Vec<&str> = showing
            .iter()
            .map(|which| rows[*which].text.as_str())
            .collect();
        assert_eq!(names, vec!["html", "body", "div#sheet", "p.line"]);

        // What an element says counts as well as what it is called.
        let showing = showing_rows(&rows, &HashSet::new(), "the end");
        let names: Vec<&str> = showing
            .iter()
            .map(|which| rows[*which].text.as_str())
            .collect();
        assert_eq!(names, vec!["html", "body", "footer"]);

        let showing = showing_rows(&rows, &HashSet::new(), "nothing like this");
        assert!(showing.is_empty());
    }

    /// A fold is remembered by which element it is, not by where the row sat:
    /// the tree is read again every few hundred milliseconds, and rows move.
    #[test]
    fn a_fold_follows_the_element_and_not_the_row_it_sat_in() {
        let folded = HashSet::from([3]);
        let mut rows = the_tree();
        // The same page with one more element above the folded one.
        rows.insert(1, a_row(7, 1, "script", 0, ""));
        let showing = showing_rows(&rows, &folded, "");
        let names: Vec<&str> = showing
            .iter()
            .map(|which| rows[*which].text.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["html", "script", "head", "title", "body"],
            "body is still the folded one after the rows moved"
        );
    }

    fn a_line(level: &str, text: &str) -> Said {
        Said {
            level: level.to_string(),
            text: text.to_string(),
            from: String::new(),
            times: 1,
        }
    }

    #[test]
    fn a_level_turned_off_is_not_shown() {
        let quiet = HashSet::from(["log".to_string()]);
        assert!(!wanted(&a_line("log", "chatter"), &quiet, ""));
        assert!(wanted(&a_line("error", "a real problem"), &quiet, ""));
        // And what the reader is looking for narrows it further.
        assert!(wanted(&a_line("error", "a real problem"), &quiet, "real"));
        assert!(!wanted(
            &a_line("error", "a real problem"),
            &quiet,
            "kettle"
        ));
    }

    #[test]
    fn a_request_is_sorted_by_what_asked_for_it_and_what_came_back() {
        // A request the page made itself is that kind of request, whatever the
        // answer turns out to be.
        assert_eq!(kind_of("xhr", "", "/api"), Kind::Asked);
        assert_eq!(kind_of("fetch", "application/json", "/api"), Kind::Asked);
        // Otherwise what came back decides.
        assert_eq!(kind_of("other", "text/css", "/x"), Kind::Style);
        assert_eq!(kind_of("", "image/png", "/x"), Kind::Image);
        assert_eq!(kind_of("", "font/woff2", "/x"), Kind::Font);
        assert_eq!(kind_of("", "application/font-woff", "/x"), Kind::Font);
        assert_eq!(
            kind_of("", "text/html; charset=utf-8", "/x"),
            Kind::Document
        );
        assert_eq!(kind_of("", "APPLICATION/JavaScript", "/x"), Kind::Script);
        // A font the engine fetched for a stylesheet comes back with nothing but
        // the stylesheet's name on it. Called CSS, a page's whole type foundry
        // reads as stylesheets -- seen on a real page, thirteen of them.
        assert_eq!(
            kind_of(
                "css",
                "",
                "https://fonts.gstatic.com/s/golostext/v1/abc.woff2"
            ),
            Kind::Font
        );
        assert_eq!(
            kind_of(
                "link",
                "",
                "https://fonts.googleapis.com/css2?family=Golos+Text"
            ),
            Kind::Style,
            "and a stylesheet whose address says nothing is still a stylesheet"
        );
        assert_eq!(
            kind_of("css", "", "/type/inter.ttf?v=2#hash"),
            Kind::Font,
            "what comes after the address itself is not part of it"
        );
        // And when nothing says anything, what asked for it is all there is.
        assert_eq!(kind_of("script", "", "/x"), Kind::Script);
        assert_eq!(kind_of("img", "", "/x"), Kind::Image);
        assert_eq!(kind_of("", "", "/x"), Kind::Other);
    }

    fn a_wire(kind: &str, mime: &str, url: &str) -> Wire {
        Wire {
            id: 1,
            method: "GET".into(),
            url: url.to_string(),
            kind: kind.to_string(),
            status: 200,
            size: 10,
            ms: 5,
            start: 0,
            mime: mime.to_string(),
        }
    }

    #[test]
    fn a_chip_turned_off_hides_that_kind() {
        let hidden = HashSet::from([Kind::Image]);
        assert!(!on_the_list(&a_wire("img", "", "a/b.png"), &hidden, ""));
        assert!(on_the_list(&a_wire("script", "", "a/b.js"), &hidden, ""));
        assert!(on_the_list(
            &a_wire("script", "", "a/b.js"),
            &hidden,
            "b.js"
        ));
        assert!(!on_the_list(
            &a_wire("script", "", "a/b.js"),
            &hidden,
            "elsewhere"
        ));
    }

    #[test]
    fn a_curl_line_carries_the_method_and_every_header() {
        let detail = Detail {
            url: "https://example.com/a'b".into(),
            method: "POST".into(),
            status: 201,
            status_text: "Created".into(),
            mime: "application/json".into(),
            size: 12,
            ms: 30,
            asked: vec![
                ("Accept".into(), "application/json".into()),
                ("X-Token".into(), "it's mine".into()),
            ],
            answered: vec![],
            phases: vec![],
            body: String::new(),
        };
        let line = curl_of(&detail);
        assert!(
            line.starts_with("curl 'https://example.com/a'\\''b'"),
            "{line}"
        );
        assert!(line.contains(" -X POST"), "{line}");
        assert!(line.contains("-H 'Accept: application/json'"), "{line}");
        // A quote inside a header must not end the quoting and turn the rest of
        // the line into something the shell runs.
        assert!(line.contains("-H 'X-Token: it'\\''s mine'"), "{line}");

        let plain = Detail {
            method: "GET".into(),
            ..detail
        };
        assert!(
            !curl_of(&plain).contains(" -X "),
            "a plain fetch needs no method spelled out"
        );
    }

    #[test]
    fn the_worst_findings_come_first() {
        let mut findings = vec![
            Finding {
                level: "warn".into(),
                rule: "tab order".into(),
                text: String::new(),
                at: 1,
                selector: String::new(),
            },
            Finding {
                level: "error".into(),
                rule: "image without alt".into(),
                text: String::new(),
                at: 2,
                selector: String::new(),
            },
        ];
        worst_first(&mut findings);
        assert_eq!(findings[0].rule, "image without alt");
        assert_eq!(findings[1].rule, "tab order");
    }

    #[test]
    fn a_script_that_threw_comes_back_as_a_complaint() {
        let complaint = said_by_the_page("!!TypeError: nothing is not a function".into());
        assert_eq!(complaint.level, "error");
        assert_eq!(complaint.text, "TypeError: nothing is not a function");

        let answer = said_by_the_page("42".into());
        assert_eq!(answer.level, "answer");
        assert_eq!(answer.text, "42");
    }

    #[test]
    fn sizes_are_written_the_way_a_reader_reads_them() {
        assert_eq!(size_text(0), "cached");
        assert_eq!(size_text(512), "512 B");
        assert_eq!(size_text(2048), "2 kB");
        assert_eq!(size_text(3 * 1024 * 1024), "3.0 MB");
        assert_eq!(trim_number(12.0), "12");
        assert_eq!(trim_number(12.5), "12.50");
    }

    /// Only the tab in front of the reader is asked about, because every answer
    /// is a script the page has to run.
    /// A panel behind another one must ask the page nothing: asking runs a
    /// script, and a script is work the engine wakes up for -- which is the
    /// thing that once cost this preview ten thousand turns of the engine for
    /// one frame.
    #[gpui::test]
    async fn a_panel_nobody_is_looking_at_asks_the_page_nothing(cx: &mut TestAppContext) {
        let (frame, cx) = a_panel(cx).await;
        let looked_at = |cx: &mut VisualTestContext| {
            frame.read_with(cx, |frame, cx| frame.panel.read(cx).being_looked_at())
        };
        assert!(
            looked_at(cx),
            "a panel the dock has said nothing about is read as being on screen"
        );

        let told = |cx: &mut VisualTestContext, active: bool| {
            let panel = frame.read_with(cx, |frame, _| frame.panel.clone());
            cx.update(|window, cx| {
                panel.update(cx, |panel, cx| panel.set_active(active, window, cx));
            });
        };

        told(cx, false);
        assert!(!looked_at(cx), "a panel put behind another asks nothing");

        told(cx, true);
        assert!(looked_at(cx), "and asks again once it is back in front");
    }

    #[test]
    fn the_rules_of_an_element_are_read_when_it_is_picked_rather_than_twice_a_second() {
        assert!(
            !worth_reading_again(Side::Rules),
            "reading which rules reach an element walks every rule of every \
             stylesheet, which is not a thing to do twice a second"
        );
        for side in [Side::Computed, Side::Layout, Side::Fonts, Side::Events] {
            assert!(
                worth_reading_again(side),
                "{side:?} costs one look at the element, and it moves as the page does"
            );
        }
    }

    #[test]
    fn a_tab_that_reads_the_whole_page_is_not_asked_again_and_again() {
        // Reading the page for what stands in a reader's way walks all of it, and
        // the size the page is shown at is the editor's own doing rather than the
        // page's, so neither is worth asking for twice a second.
        let asked_once = [Tools::Accessibility, Tools::Device];
        for tools in asked_once {
            assert!(!tools.keeps_changing(), "{tools:?} is read once, on demand");
        }
        for tools in Tools::ALL {
            if !asked_once.contains(&tools) {
                assert!(
                    tools.keeps_changing(),
                    "{tools:?} shows what the page is doing now"
                );
            }
        }
    }

    struct ToolsFrame {
        panel: Entity<BrowserToolsPanel>,
        workspace: Entity<Workspace>,
        /// Set when the panel has asked the dock to close it.
        closed: Rc<std::cell::Cell<bool>>,
        _closing: Subscription,
    }

    impl Render for ToolsFrame {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.panel.clone())
        }
    }

    async fn a_panel(cx: &mut TestAppContext) -> (Entity<ToolsFrame>, &mut VisualTestContext) {
        let app_state = cx.update(|cx| {
            let app_state = AppState::test(cx);
            editor::init(cx);
            app_state
        });
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (frame, cx) = cx.add_window_view(|window, cx| {
            let workspace = cx.new(|cx| Workspace::test_new(project.clone(), window, cx));
            let panel = BrowserToolsPanel::new(workspace.downgrade(), window, cx);
            let closed = Rc::new(std::cell::Cell::new(false));
            let closing = cx.subscribe(&panel, {
                let closed = closed.clone();
                move |_: &mut ToolsFrame, _, event: &PanelEvent, _| {
                    if matches!(event, PanelEvent::Close) {
                        closed.set(true);
                    }
                }
            });
            ToolsFrame {
                panel,
                workspace,
                closed,
                _closing: closing,
            }
        });
        cx.run_until_parked();
        draw(cx);
        (frame, cx)
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear();
        });
        cx.run_until_parked();
    }

    /// A name for one painted thing, kept for as long as the window that was
    /// asked about it: `debug_bounds` holds on to the name, and the ones here are
    /// built from a row number rather than written out.
    fn named(text: String) -> &'static str {
        text.leak()
    }

    fn painted(cx: &mut VisualTestContext, selector: &'static str) -> Bounds<Pixels> {
        cx.debug_bounds(selector)
            .unwrap_or_else(|| panic!("{selector} was expected to have been painted"))
    }

    fn click(cx: &mut VisualTestContext, selector: &'static str) {
        let at = painted(cx, selector).center();
        cx.simulate_click(at, Modifiers::none());
        draw(cx);
    }

    /// Hands the panel an answer as though the page had just given it.
    fn answered(frame: &Entity<ToolsFrame>, cx: &mut VisualTestContext, answers: &[(Ask, &str)]) {
        frame.update(cx, |frame, cx| {
            frame.panel.update(cx, |panel, cx| {
                for (ask, answer) in answers {
                    panel
                        .answers
                        .borrow_mut()
                        .got
                        .push((*ask, answer.to_string()));
                }
                panel.take_answers(cx);
            });
        });
        draw(cx);
    }

    #[gpui::test]
    async fn what_the_page_is_made_of_is_drawn_as_a_tree(cx: &mut TestAppContext) {
        let (frame, cx) = a_panel(cx).await;
        answered(&frame, cx, &[(Ask::Tree, A_TREE)]);

        for at in 0..7 {
            let row = painted(cx, named(format!("TREE-{at}")));
            assert!(
                row.size.width > px(1.) && row.size.height > px(1.),
                "row {at} has to take up real screen area, not {:?}",
                row.size
            );
        }
        // Deeper elements are drawn further in, which is what makes a tree
        // readable as one. The indent is padding inside the row, so where the row
        // itself starts says nothing: what moves is everything in it.
        let html = painted(cx, "INDENT-0");
        let head = painted(cx, "INDENT-1");
        let title = painted(cx, "INDENT-2");
        assert!(
            head.origin.x > html.origin.x && title.origin.x > head.origin.x,
            "each level has to be drawn further in than the one above it: {:?}, \
             {:?}, {:?}",
            html.origin,
            head.origin,
            title.origin
        );
    }

    #[gpui::test]
    async fn clicking_the_arrow_folds_the_branch_it_belongs_to(cx: &mut TestAppContext) {
        let (frame, cx) = a_panel(cx).await;
        answered(&frame, cx, &[(Ask::Tree, A_TREE)]);
        assert!(
            cx.debug_bounds("TREE-4").is_some(),
            "what is inside body is drawn before the fold"
        );

        click(cx, "FOLD-3");

        assert!(
            cx.debug_bounds("TREE-3").is_some(),
            "the folded element itself stays"
        );
        assert!(
            cx.debug_bounds("TREE-4").is_none(),
            "what is inside it has to go"
        );
        assert!(
            cx.debug_bounds("TREE-5").is_none(),
            "and what is inside that, however deep"
        );
        assert!(
            cx.debug_bounds("TREE-1").is_some(),
            "a branch beside it must not be touched"
        );

        click(cx, "FOLD-3");
        assert!(
            cx.debug_bounds("TREE-4").is_some(),
            "clicking again has to open it back up"
        );
    }

    #[gpui::test]
    async fn picking_a_row_reads_the_rules_that_reach_it(cx: &mut TestAppContext) {
        let (frame, cx) = a_panel(cx).await;
        answered(&frame, cx, &[(Ask::Tree, A_TREE)]);
        click(cx, "TREE-5");
        answered(
            &frame,
            cx,
            &[(
                Ask::Rules,
                r#"[
                  {"sheet":"page.css","selector":"p","media":"",
                   "declarations":[["color","rgb(1, 2, 3)"]]},
                  {"sheet":"element","selector":"style attribute","media":"",
                   "declarations":[["color","red"]]}
                ]"#,
            )],
        );

        assert!(cx.debug_bounds("RULE-0").is_some());
        assert!(cx.debug_bounds("RULE-1").is_some());
        // What won is what the reader is looking for, so the cascade is shown
        // from the end backwards: the style attribute comes first.
        let first = painted(cx, "RULE-0");
        let second = painted(cx, "RULE-1");
        assert!(
            first.origin.y < second.origin.y,
            "the last rule in the cascade has to be drawn first"
        );
    }

    #[gpui::test]
    async fn every_side_of_an_element_shows_what_the_page_answered(cx: &mut TestAppContext) {
        let (frame, cx) = a_panel(cx).await;
        answered(&frame, cx, &[(Ask::Tree, A_TREE)]);
        click(cx, "TREE-4");

        click(cx, "SIDE-Computed");
        answered(
            &frame,
            cx,
            &[(
                Ask::Computed,
                r#"[["color","rgb(0, 0, 0)"],["display","block"]]"#,
            )],
        );
        assert!(cx.debug_bounds("COMPUTED-0").is_some());
        assert!(cx.debug_bounds("COMPUTED-1").is_some());

        click(cx, "SIDE-Layout");
        answered(
            &frame,
            cx,
            &[(
                Ask::Layout,
                r#"{"box":{"left":8,"top":16,"width":300,"height":40},
                    "margin":[8,0,8,0],"border":[1,1,1,1],"padding":[4,4,4,4],
                    "content":{"width":292,"height":30},"display":"flex",
                    "position":"static","boxSizing":"border-box","zIndex":"auto",
                    "overflow":"visible",
                    "flex":{"direction":"row","wrap":"nowrap","justify":"center",
                            "align":"stretch","gap":"8px"},
                    "grid":null}"#,
            )],
        );
        assert!(
            cx.debug_bounds("LAYOUT").is_some(),
            "the box the element takes up has to be drawn"
        );

        click(cx, "SIDE-Fonts");
        answered(
            &frame,
            cx,
            &[(
                Ask::Fonts,
                r#"{"element":{"family":"\"Golos Text\", serif","size":"16px",
                     "weight":"400","style":"normal","height":"24px","spacing":"normal"},
                    "faces":[{"family":"Golos Text","weight":"400","style":"normal",
                              "status":"loaded"},
                             {"family":"Golos Text","weight":"700","style":"normal",
                              "status":"error"}]}"#,
            )],
        );
        assert!(
            cx.debug_bounds("FONTS").is_some(),
            "the fonts of the picked element have to be drawn"
        );
        assert!(cx.debug_bounds("FACE-0").is_some());
        assert!(
            cx.debug_bounds("FACE-1").is_some(),
            "a face that never arrived has to be listed too, since that is why \
             words turn up in the wrong font"
        );

        click(cx, "SIDE-Events");
        answered(&frame, cx, &[(Ask::Events, r#"[["click",2,"script"]]"#)]);
        assert!(cx.debug_bounds("EVENT-0").is_some());
    }

    #[gpui::test]
    async fn what_the_page_said_is_drawn_once_however_often_it_says_it(cx: &mut TestAppContext) {
        let (frame, cx) = a_panel(cx).await;
        click(cx, "TOOLS-Console");
        answered(
            &frame,
            cx,
            &[(
                Ask::Said,
                r#"[{"level":"log","text":"hello","at":1,"from":"","times":1}]"#,
            )],
        );
        assert!(cx.debug_bounds("SAID-0").is_some());

        // The same line again, in a later answer: counted, not repeated.
        answered(
            &frame,
            cx,
            &[(
                Ask::Said,
                r#"[{"level":"log","text":"hello","at":2,"from":"","times":1}]"#,
            )],
        );
        assert!(
            cx.debug_bounds("SAID-1").is_none(),
            "a page that logs in a loop must not fill the panel with the same line"
        );

        answered(
            &frame,
            cx,
            &[(
                Ask::Said,
                r#"[{"level":"error","text":"and a complaint","at":3,"from":"","times":1}]"#,
            )],
        );
        assert!(
            cx.debug_bounds("SAID-1").is_some(),
            "a different line is a line of its own"
        );
    }

    #[gpui::test]
    async fn the_console_walks_back_through_what_was_typed(cx: &mut TestAppContext) {
        let (frame, cx) = a_panel(cx).await;
        click(cx, "TOOLS-Console");
        let console = frame.read_with(cx, |frame, cx| frame.panel.read(cx).console.clone());
        cx.update(|window, cx| {
            let handle = console.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        });
        draw(cx);

        cx.simulate_input("1 + 1");
        cx.dispatch_action(menu::Confirm);
        draw(cx);
        assert_eq!(
            console.read_with(cx, |console, cx| console.text(cx)),
            "",
            "what was run is taken out of the line"
        );

        cx.simulate_input("$0.tagName");
        cx.dispatch_action(menu::Confirm);
        draw(cx);

        cx.dispatch_action(zed_actions::editor::MoveUp);
        draw(cx);
        assert_eq!(
            console.read_with(cx, |console, cx| console.text(cx)),
            "$0.tagName",
            "up has to bring back the last thing run"
        );
        cx.dispatch_action(zed_actions::editor::MoveUp);
        draw(cx);
        assert_eq!(
            console.read_with(cx, |console, cx| console.text(cx)),
            "1 + 1",
            "and again the one before it"
        );
        cx.dispatch_action(zed_actions::editor::MoveDown);
        cx.dispatch_action(zed_actions::editor::MoveDown);
        draw(cx);
        assert_eq!(
            console.read_with(cx, |console, cx| console.text(cx)),
            "",
            "coming back down past the newest leaves an empty line to type in"
        );
    }

    #[gpui::test]
    async fn requests_are_listed_with_what_they_cost(cx: &mut TestAppContext) {
        let (frame, cx) = a_panel(cx).await;
        click(cx, "TOOLS-Network");
        answered(
            &frame,
            cx,
            &[(
                Ask::Network,
                r#"[
                  {"id":1,"method":"GET","url":"https://example.com/a.css","kind":"link",
                   "status":200,"size":2048,"ms":10,"start":0,"type":"text/css"},
                  {"id":2,"method":"POST","url":"https://example.com/api","kind":"fetch",
                   "status":500,"size":40,"ms":100,"start":20,"type":"application/json"}
                ]"#,
            )],
        );
        let first = painted(cx, "WIRE-1");
        let second = painted(cx, "WIRE-2");
        assert!(first.origin.y < second.origin.y);

        // A chip turned off takes its kind out of the list.
        click(cx, "TOOLS-Network");
        frame.update(cx, |frame, cx| {
            frame.panel.update(cx, |panel, cx| {
                panel.hidden_kinds.insert(Kind::Style);
                cx.notify();
            });
        });
        draw(cx);
        assert!(
            cx.debug_bounds("WIRE-1").is_none(),
            "the stylesheet was hidden"
        );
        assert!(cx.debug_bounds("WIRE-2").is_some());
    }

    #[gpui::test]
    async fn a_request_opens_what_was_asked_and_what_came_back(cx: &mut TestAppContext) {
        let (frame, cx) = a_panel(cx).await;
        click(cx, "TOOLS-Network");
        answered(
            &frame,
            cx,
            &[(
                Ask::Network,
                r#"[{"id":7,"method":"GET","url":"https://example.com/api","kind":"fetch",
                     "status":200,"size":40,"ms":100,"start":20,"type":"application/json"}]"#,
            )],
        );
        click(cx, "WIRE-7");
        answered(
            &frame,
            cx,
            &[(
                Ask::Request,
                r#"{"url":"https://example.com/api","method":"GET","status":200,
                    "statusText":"OK","type":"application/json","size":40,"ms":100,
                    "reqHeaders":[["Accept","application/json"]],
                    "resHeaders":[["Content-Type","application/json"]],
                    "phases":[["dns",2],["connect",4],["wait",80],["receive",14]],
                    "body":"{\"ok\":true}"}"#,
            )],
        );
        let asked = frame.read_with(cx, |frame, cx| {
            frame
                .panel
                .read(cx)
                .wire
                .as_ref()
                .map(|wire| (wire.asked.len(), wire.answered.len(), wire.phases.len()))
        });
        assert_eq!(asked, Some((1, 1, 4)));
    }

    #[gpui::test]
    async fn a_stylesheet_can_be_read_and_turned_off(cx: &mut TestAppContext) {
        let (frame, cx) = a_panel(cx).await;
        click(cx, "TOOLS-Style");
        answered(
            &frame,
            cx,
            &[(
                Ask::Sheets,
                r#"[
                  {"id":0,"name":"page.css","href":"","rules":12,"disabled":false,
                   "media":"","readable":true},
                  {"id":1,"name":"print.css","href":"","rules":3,"disabled":true,
                   "media":"print","readable":true}
                ]"#,
            )],
        );
        assert!(cx.debug_bounds("SHEET-0").is_some());
        assert!(cx.debug_bounds("SHEET-1").is_some());
        // The eye of a stylesheet that is off has to be the other one, or the
        // reader cannot tell which is which.
        assert!(cx.debug_bounds("ICON-EyeOff").is_some());

        click(cx, "SHEET-0");
        answered(
            &frame,
            cx,
            &[(
                Ask::SheetText,
                r#"{"name":"page.css","text":"p { color: red }","rules":1}"#,
            )],
        );
        let showing = frame.read_with(cx, |frame, cx| {
            frame
                .panel
                .read(cx)
                .sheet
                .as_ref()
                .map(|sheet| sheet.text.clone())
        });
        assert_eq!(showing.as_deref(), Some("p { color: red }"));
    }

    #[gpui::test]
    async fn what_the_page_keeps_is_listed_store_by_store(cx: &mut TestAppContext) {
        let (frame, cx) = a_panel(cx).await;
        click(cx, "TOOLS-Storage");
        answered(
            &frame,
            cx,
            &[(
                Ask::Storage,
                r#"{"cookies":[["session","abc"]],
                    "local":[["theme","dark"],["seen","1"]],
                    "session":[],
                    "databases":["notes (v2)"],
                    "caches":["pages"]}"#,
            )],
        );
        assert!(cx.debug_bounds("STORE-cookie-0").is_some());
        assert!(cx.debug_bounds("STORE-local-0").is_some());
        assert!(cx.debug_bounds("STORE-local-1").is_some());
        assert!(
            cx.debug_bounds("STORE-session-0").is_none(),
            "an empty store has no rows"
        );

        // What the page installed to work without the network belongs here too.
        answered(
            &frame,
            cx,
            &[(
                Ask::Installed,
                r#"{"manifest":"https://example.com/app.webmanifest",
                    "workers":["https://example.com/  activated"],"supported":true}"#,
            )],
        );
        assert!(cx.debug_bounds("WORKER-0").is_some());
    }

    #[gpui::test]
    async fn how_the_page_arrived_is_drawn_in_proportion(cx: &mut TestAppContext) {
        let (frame, cx) = a_panel(cx).await;
        click(cx, "TOOLS-Performance");
        answered(
            &frame,
            cx,
            &[(
                Ask::Cost,
                r#"{"phases":[["dns",2],["request",20],["response",200]],
                    "paints":[["first-paint",120]],
                    "counts":{"elements":42,"text":900,"images":2,"scripts":1,
                              "stylesheets":2,"rules":80,"listeners":5,
                              "requests":7,"transferred":40960},
                    "memory":null}"#,
            )],
        );
        let short = painted(cx, "PHASE-dns");
        let long = painted(cx, "PHASE-response");
        assert!(
            short.size.height > px(1.) && long.size.height > px(1.),
            "the bars have to be drawn at all"
        );
        // The bar is inside the row, so the row's own width says nothing; what
        // the panel holds is what a test can compare.
        let phases = frame.read_with(cx, |frame, cx| frame.panel.read(cx).cost.phases.clone());
        assert_eq!(phases.len(), 3);
        assert!(phases[2].1 > phases[0].1);
    }

    #[gpui::test]
    async fn what_stands_in_a_readers_way_is_listed_worst_first(cx: &mut TestAppContext) {
        let (frame, cx) = a_panel(cx).await;
        click(cx, "TOOLS-Accessibility");
        answered(
            &frame,
            cx,
            &[(
                Ask::Findings,
                r#"[
                  {"level":"warn","rule":"tab order","text":"A tabindex above zero.",
                   "at":4,"selector":"div#sheet"},
                  {"level":"error","rule":"image without alt","text":"An image says nothing.",
                   "at":5,"selector":"p.line"}
                ]"#,
            )],
        );
        let first = painted(cx, "FINDING-0");
        let second = painted(cx, "FINDING-1");
        assert!(first.origin.y < second.origin.y);
        let worst = frame.read_with(cx, |frame, cx| {
            frame.panel.read(cx).findings[0].rule.clone()
        });
        assert_eq!(worst, "image without alt", "the error has to come first");

        // Clicking a finding takes the reader to the element it is about.
        answered(&frame, cx, &[(Ask::Tree, A_TREE)]);
        click(cx, "FINDING-0");
        let (showing, picked) = frame.read_with(cx, |frame, cx| {
            let panel = frame.panel.read(cx);
            (panel.showing, panel.picked)
        });
        assert_eq!(showing, Tools::Elements);
        assert_eq!(picked, Some(5));
    }

    #[gpui::test]
    async fn the_picker_hands_the_reader_the_element_they_clicked(cx: &mut TestAppContext) {
        let (frame, cx) = a_panel(cx).await;
        answered(&frame, cx, &[(Ask::Tree, A_TREE)]);
        click(cx, "ICON-Crosshair");
        let armed = frame.read_with(cx, |frame, cx| frame.panel.read(cx).picking);
        assert!(armed, "the picker has to be armed by its own button");

        answered(
            &frame,
            cx,
            &[(Ask::Picked, r#"{"at":4,"selector":"div#sheet"}"#)],
        );
        let (armed, picked, selector) = frame.read_with(cx, |frame, cx| {
            let panel = frame.panel.read(cx);
            (panel.picking, panel.picked, panel.picked_selector.clone())
        });
        assert!(!armed, "picking one element disarms the picker");
        assert_eq!(picked, Some(4));
        assert_eq!(selector, "div#sheet");
    }

    #[gpui::test]
    async fn an_element_can_be_rewritten_in_place(cx: &mut TestAppContext) {
        let (frame, cx) = a_panel(cx).await;
        answered(&frame, cx, &[(Ask::Tree, A_TREE)]);
        click(cx, "TREE-5");
        answered(
            &frame,
            cx,
            &[(Ask::Html, r#"<p class="line">Some words</p>"#)],
        );
        assert!(
            cx.debug_bounds("EDIT-HTML").is_none(),
            "the markup is not open for rewriting until it is asked for"
        );

        click(cx, "EDIT-OPEN");
        let editor = painted(cx, "EDIT-HTML");
        assert!(
            editor.size.height > px(40.),
            "a multi-line editor with no height of its own is painted as a sliver: {:?}",
            editor.size
        );
        let markup = frame.read_with(cx, |frame, cx| {
            let panel = frame.panel.read(cx);
            panel.html_editor.read(cx).text(cx)
        });
        assert_eq!(
            markup, r#"<p class="line">Some words</p>"#,
            "the element's own markup has to be what the reader starts from"
        );

        click(cx, "EDIT-APPLY");
        let (editing, picked) = frame.read_with(cx, |frame, cx| {
            let panel = frame.panel.read(cx);
            (panel.editing_html, panel.picked)
        });
        assert!(!editing, "putting it in the page closes the editor");
        assert_eq!(
            picked, None,
            "what replaced the element is a different element, so the numbering \
             the panel is holding no longer points at it"
        );
    }

    #[gpui::test]
    async fn the_sizes_a_page_can_be_shown_at_are_offered(cx: &mut TestAppContext) {
        let (_frame, cx) = a_panel(cx).await;
        click(cx, "TOOLS-Device");
        for (name, _, _) in DEVICES {
            assert!(
                cx.debug_bounds(named(format!("DEVICE-{name}"))).is_some(),
                "{name} has to be offered"
            );
        }
        assert!(
            cx.debug_bounds("DEVICE-Full").is_some(),
            "and going back to the pane's own size has to be offered too"
        );
        // Nothing is being read here, so choosing one changes nothing but must
        // not fall over either.
        click(cx, "DEVICE-Phone");
        click(cx, "DEVICE-Full");
    }

    /// The dock tells a panel it has come to the front from inside its own update
    /// of the workspace, and everything the panel does about a page begins by
    /// reading the workspace to find that page. Read there, gpui stops the editor
    /// dead -- "cannot read workspace::Workspace while it is already being
    /// updated" -- which is what happened on the first press of the button that
    /// opens these tools.
    #[gpui::test]
    async fn coming_to_the_front_does_not_read_what_the_dock_is_holding(cx: &mut TestAppContext) {
        let (frame, cx) = a_panel(cx).await;
        let (panel, workspace) = frame.read_with(cx, |frame, _| {
            (frame.panel.clone(), frame.workspace.clone())
        });

        for active in [true, false, true] {
            cx.update(|window, cx| {
                workspace.update(cx, |_, cx| {
                    panel.update(cx, |panel, cx| panel.set_active(active, window, cx));
                });
            });
            cx.run_until_parked();
            assert_eq!(
                panel.read_with(cx, |panel, _| panel.told_active),
                Some(active),
                "the panel has to hear the dock either way"
            );
        }
        // And it is still there to be drawn afterwards.
        draw(cx);
        assert!(cx.debug_bounds("TOOLS-Elements").is_some());
    }

    /// The reader has to be able to put the tools away from the tools
    /// themselves, without hunting for the dock's own handle.
    #[gpui::test]
    async fn the_tools_can_be_closed_from_where_they_are(cx: &mut TestAppContext) {
        let (frame, cx) = a_panel(cx).await;
        let button = painted(cx, "TOOLS-CLOSE");
        assert!(
            button.size.width > px(1.) && button.size.height > px(1.),
            "the close button has to occupy real screen area, not {:?}",
            button.size
        );
        assert!(
            !frame.read_with(cx, |frame, _| frame.closed.get()),
            "nothing is closed before it is clicked"
        );

        // Whatever the tools were drawing on the page goes with them: a page
        // left with the measuring drag armed holds back every press the reader
        // makes, which reads as a page that has stopped working.
        frame.update(cx, |frame, cx| {
            frame.panel.update(cx, |panel, cx| {
                panel.measuring = true;
                panel.ruled = true;
                panel.numbering = true;
                panel.picking = true;
                cx.notify();
            });
        });
        draw(cx);

        click(cx, "TOOLS-CLOSE");

        assert!(
            frame.read_with(cx, |frame, _| frame.closed.get()),
            "clicking it has to ask the dock to close the panel"
        );
        let (measuring, ruled, numbering, picking) = frame.read_with(cx, |frame, cx| {
            let panel = frame.panel.read(cx);
            (panel.measuring, panel.ruled, panel.numbering, panel.picking)
        });
        assert!(
            !measuring && !ruled && !numbering && !picking,
            "nothing of ours may be left drawn on a page the reader can no \
             longer see the panel for"
        );
    }

    /// A number that names an element is the page's own numbering. Carried into
    /// another page it names something else, so the reading of one page is let go
    /// of when the reader is looking at the next.
    #[gpui::test]
    async fn what_was_read_of_one_page_is_not_shown_for_another(cx: &mut TestAppContext) {
        let (frame, cx) = a_panel(cx).await;
        answered(
            &frame,
            cx,
            &[
                (Ask::Who, "the first page"),
                (Ask::Tree, A_TREE),
                (
                    Ask::Said,
                    r#"[{"level":"log","text":"hello","at":1,"from":"","times":1}]"#,
                ),
            ],
        );
        click(cx, "TREE-4");
        answered(
            &frame,
            cx,
            &[
                (Ask::Html, "<div id=\"sheet\"></div>"),
                (
                    Ask::Storage,
                    r#"{"cookies":[["session","abc"]],"local":[],"session":[],
                        "databases":[],"caches":[]}"#,
                ),
            ],
        );
        assert_eq!(
            frame.read_with(cx, |frame, cx| frame.panel.read(cx).picked),
            Some(4)
        );

        answered(&frame, cx, &[(Ask::Who, "another page altogether")]);

        let (picked, rows, cookies, said) = frame.read_with(cx, |frame, cx| {
            let panel = frame.panel.read(cx);
            (
                panel.picked,
                panel.rows.len(),
                panel.stores.cookies.len(),
                panel.said.len(),
            )
        });
        assert_eq!(picked, None, "nothing is picked in a page not yet read");
        assert_eq!(rows, 0, "and the tree it was picked from is gone");
        assert_eq!(cookies, 0, "as is what the other page kept");
        assert_eq!(
            said, 1,
            "what the pages have said is kept: it is often the reason the reader \
             went to the next one"
        );
        assert!(
            cx.debug_bounds("TREE-4").is_none(),
            "and nothing of the other page is still drawn"
        );
    }

    #[gpui::test]
    async fn every_tab_can_be_opened_and_says_what_it_has(cx: &mut TestAppContext) {
        let (_frame, cx) = a_panel(cx).await;
        for tools in Tools::ALL {
            let selector = named(format!("TOOLS-{}", tools.label()));
            let tab = painted(cx, selector);
            assert!(
                tab.size.width > px(1.),
                "{selector} has to be a real button, not {:?}",
                tab.size
            );
            click(cx, selector);
        }
    }
}
