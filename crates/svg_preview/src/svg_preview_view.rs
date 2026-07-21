use std::mem;
use std::sync::Arc;

use file_icons::FileIcons;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement, IntoElement,
    ParentElement, Render, RenderImage, ScrollDelta, ScrollWheelEvent, Styled, Subscription, Task,
    WeakEntity, Window, div, img, px,
};
use language::{Buffer, BufferEvent};
use multi_buffer::MultiBuffer;
use theme::{Theme, ThemeRegistry};
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::item::Item;
use workspace::{Pane, Workspace};

use crate::{OpenFollowingPreview, OpenPreview, OpenPreviewToTheSide};

const MIN_SVG_ZOOM: f32 = 0.1;
const MAX_SVG_ZOOM: f32 = 10.0;

const ONE_LIGHT_THEME: &str = "One Light";
const ONE_DARK_THEME: &str = "One Dark";

/// Per-view backdrop theme for a preview. It affects only the chrome of this
/// single view, never the global application theme.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ViewerThemeMode {
    #[default]
    Match,
    Light,
    Dark,
}

impl ViewerThemeMode {
    fn next(self) -> Self {
        match self {
            ViewerThemeMode::Match => ViewerThemeMode::Light,
            ViewerThemeMode::Light => ViewerThemeMode::Dark,
            ViewerThemeMode::Dark => ViewerThemeMode::Match,
        }
    }

    fn resolve(self, cx: &App) -> Arc<Theme> {
        let name = match self {
            ViewerThemeMode::Match => return cx.theme().clone(),
            ViewerThemeMode::Light => ONE_LIGHT_THEME,
            ViewerThemeMode::Dark => ONE_DARK_THEME,
        };
        ThemeRegistry::global(cx)
            .get(name)
            .log_err()
            .unwrap_or_else(|| cx.theme().clone())
    }

    fn tooltip(self) -> &'static str {
        match self {
            ViewerThemeMode::Match => "Backdrop theme: follow global (click for One Light)",
            ViewerThemeMode::Light => "Backdrop theme: One Light (click for One Dark)",
            ViewerThemeMode::Dark => "Backdrop theme: One Dark (click to follow global)",
        }
    }
}

pub struct SvgPreviewView {
    focus_handle: FocusHandle,
    buffer: Option<Entity<Buffer>>,
    current_svg: Option<Result<Arc<RenderImage>, SharedString>>,
    zoom_level: f32,
    theme_mode: ViewerThemeMode,
    _refresh: Task<()>,
    _buffer_subscription: Option<Subscription>,
    _workspace_subscription: Option<Subscription>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SvgPreviewMode {
    /// The preview will always show the contents of the provided editor.
    Default,
    /// The preview will "follow" the last active editor of an SVG file.
    Follow,
}

impl SvgPreviewView {
    pub fn new(
        mode: SvgPreviewMode,
        active_buffer: Entity<MultiBuffer>,
        workspace_handle: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let workspace_subscription = if mode == SvgPreviewMode::Follow
                && let Some(workspace) = workspace_handle.upgrade()
            {
                Some(Self::subscribe_to_workspace(workspace, window, cx))
            } else {
                None
            };

            let buffer = active_buffer.read_with(cx, |buffer, _cx| buffer.as_singleton());

            let subscription = buffer
                .as_ref()
                .map(|buffer| Self::create_buffer_subscription(buffer, window, cx));

            let mut this = Self {
                focus_handle: cx.focus_handle(),
                buffer,
                current_svg: None,
                zoom_level: 1.0,
                theme_mode: ViewerThemeMode::default(),
                _buffer_subscription: subscription,
                _workspace_subscription: workspace_subscription,
                _refresh: Task::ready(()),
            };
            this.render_image(window, cx);

            this
        })
    }

    fn subscribe_to_workspace(
        workspace: Entity<Workspace>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(
            &workspace,
            window,
            move |this: &mut SvgPreviewView, workspace, event: &workspace::Event, window, cx| {
                if let workspace::Event::ActiveItemChanged = event {
                    let workspace = workspace.read(cx);
                    if let Some(active_item) = workspace.active_item(cx)
                        && let Some(buffer) = active_item.downcast::<MultiBuffer>()
                        && Self::is_svg_file(&buffer, cx)
                    {
                        let Some(buffer) = buffer.read(cx).as_singleton() else {
                            return;
                        };
                        if this.buffer.as_ref() != Some(&buffer) {
                            this._buffer_subscription =
                                Some(Self::create_buffer_subscription(&buffer, window, cx));
                            this.buffer = Some(buffer);
                            this.render_image(window, cx);
                            cx.notify();
                        }
                    } else {
                        this.set_current(None, window, cx);
                    }
                }
            },
        )
    }

    fn render_image(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(buffer) = self.buffer.as_ref() else {
            return;
        };
        const SCALE_FACTOR: f32 = 1.0;

        let renderer = cx.svg_renderer();
        let content = buffer.read(cx).snapshot();
        let background_task = cx.background_spawn(async move {
            renderer.render_single_frame(content.text().as_bytes(), SCALE_FACTOR)
        });

        self._refresh = cx.spawn_in(window, async move |this, cx| {
            let result = background_task.await;

            this.update_in(cx, |view, window, cx| {
                let current = result.map_err(|e| e.to_string().into());
                view.set_current(Some(current), window, cx);
            })
            .ok();
        });
    }

    fn handle_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !(event.modifiers.control || event.modifiers.platform) {
            return;
        }
        let delta: f32 = match event.delta {
            ScrollDelta::Pixels(pixels) => pixels.y.into(),
            ScrollDelta::Lines(lines) => lines.y * 20.0,
        };
        if delta == 0.0 {
            return;
        }
        let factor = if delta > 0.0 {
            1.0 + delta.abs() * 0.01
        } else {
            1.0 / (1.0 + delta.abs() * 0.01)
        };
        self.set_zoom(self.zoom_level * factor, cx);
    }

    fn set_zoom(&mut self, zoom: f32, cx: &mut Context<Self>) {
        self.zoom_level = zoom.clamp(MIN_SVG_ZOOM, MAX_SVG_ZOOM);
        cx.notify();
    }

    fn zoom_in(&mut self, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom_level * 1.2, cx);
    }

    fn zoom_out(&mut self, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom_level / 1.2, cx);
    }

    fn resolved_theme(&self, cx: &App) -> Arc<Theme> {
        self.theme_mode.resolve(cx)
    }

    fn cycle_theme_mode(&mut self, cx: &mut Context<Self>) {
        self.theme_mode = self.theme_mode.next();
        cx.notify();
    }

    fn set_current(
        &mut self,
        image: Option<Result<Arc<RenderImage>, SharedString>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(Ok(image)) = mem::replace(&mut self.current_svg, image) {
            window.drop_image(image).ok();
        }
        cx.notify();
    }

    fn find_existing_preview_item_idx(
        pane: &Pane,
        buffer: &Entity<MultiBuffer>,
        cx: &App,
    ) -> Option<usize> {
        let buffer_id = buffer.read(cx).as_singleton()?.entity_id();
        pane.items_of_type::<SvgPreviewView>()
            .find(|view| {
                view.read(cx)
                    .buffer
                    .as_ref()
                    .is_some_and(|buffer| buffer.entity_id() == buffer_id)
            })
            .and_then(|view| pane.index_for_item(&view))
    }

    pub fn resolve_active_item_as_svg_buffer(
        workspace: &Workspace,
        cx: &mut Context<Workspace>,
    ) -> Option<Entity<MultiBuffer>> {
        workspace
            .active_item(cx)?
            .act_as::<MultiBuffer>(cx)
            .filter(|buffer| Self::is_svg_file(&buffer, cx))
    }

    fn create_svg_view(
        mode: SvgPreviewMode,
        workspace: &mut Workspace,
        buffer: Entity<MultiBuffer>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<SvgPreviewView> {
        let workspace_handle = workspace.weak_handle();
        SvgPreviewView::new(mode, buffer, workspace_handle, window, cx)
    }

    fn create_buffer_subscription(
        buffer: &Entity<Buffer>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(
            buffer,
            window,
            move |this, _buffer, event: &BufferEvent, window, cx| match event {
                BufferEvent::Edited { .. } | BufferEvent::Saved => {
                    this.render_image(window, cx);
                }
                _ => {}
            },
        )
    }

    pub fn is_svg_file(buffer: &Entity<MultiBuffer>, cx: &App) -> bool {
        buffer
            .read(cx)
            .as_singleton()
            .and_then(|buffer| buffer.read(cx).file())
            .is_some_and(|file| {
                std::path::Path::new(file.file_name(cx))
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("svg"))
            })
    }

    pub fn register(workspace: &mut Workspace, _window: &mut Window, _cx: &mut Context<Workspace>) {
        workspace.register_action(move |workspace, _: &OpenPreview, window, cx| {
            if let Some(buffer) = Self::resolve_active_item_as_svg_buffer(workspace, cx)
                && Self::is_svg_file(&buffer, cx)
            {
                let view = Self::create_svg_view(
                    SvgPreviewMode::Default,
                    workspace,
                    buffer.clone(),
                    window,
                    cx,
                );
                workspace.active_pane().update(cx, |pane, cx| {
                    if let Some(existing_view_idx) =
                        Self::find_existing_preview_item_idx(pane, &buffer, cx)
                    {
                        pane.activate_item(existing_view_idx, true, true, window, cx);
                    } else {
                        pane.add_item(Box::new(view), true, true, None, window, cx)
                    }
                });
                cx.notify();
            }
        });

        workspace.register_action(move |workspace, _: &OpenPreviewToTheSide, window, cx| {
            if let Some(editor) = Self::resolve_active_item_as_svg_buffer(workspace, cx)
                && Self::is_svg_file(&editor, cx)
            {
                let editor_clone = editor.clone();
                let view = Self::create_svg_view(
                    SvgPreviewMode::Default,
                    workspace,
                    editor_clone,
                    window,
                    cx,
                );
                let pane = workspace
                    .find_pane_in_direction(workspace::SplitDirection::Right, cx)
                    .unwrap_or_else(|| {
                        workspace.split_pane(
                            workspace.active_pane().clone(),
                            workspace::SplitDirection::Right,
                            window,
                            cx,
                        )
                    });
                pane.update(cx, |pane, cx| {
                    if let Some(existing_view_idx) =
                        Self::find_existing_preview_item_idx(pane, &editor, cx)
                    {
                        pane.activate_item(existing_view_idx, true, true, window, cx);
                    } else {
                        pane.add_item(Box::new(view), false, false, None, window, cx)
                    }
                });
                cx.notify();
            }
        });

        workspace.register_action(move |workspace, _: &OpenFollowingPreview, window, cx| {
            if let Some(editor) = Self::resolve_active_item_as_svg_buffer(workspace, cx)
                && Self::is_svg_file(&editor, cx)
            {
                let view =
                    Self::create_svg_view(SvgPreviewMode::Follow, workspace, editor, window, cx);
                workspace.active_pane().update(cx, |pane, cx| {
                    pane.add_item(Box::new(view), true, true, None, window, cx)
                });
                cx.notify();
            }
        });
    }
}

impl Render for SvgPreviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let zoom = self.zoom_level;
        let has_image = matches!(self.current_svg, Some(Ok(_)));
        let theme = self.resolved_theme(cx);
        let colors = theme.colors();
        let editor_background = colors.editor_background;
        let border = colors.border;
        let elevated_surface_background = colors.elevated_surface_background;
        let theme_tooltip = self.theme_mode.tooltip();
        v_flex()
            .id("SvgPreview")
            .key_context("SvgPreview")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .relative()
            .bg(editor_background)
            .on_scroll_wheel(cx.listener(Self::handle_scroll_wheel))
            .child(
                div()
                    .id("svg-scroll")
                    .size_full()
                    .overflow_scroll()
                    .flex()
                    .justify_center()
                    .items_center()
                    .map(|this| match self.current_svg.clone() {
                        Some(Ok(image)) => {
                            let image_size = image.size(0);
                            let width = px(image_size.width.0 as f32 * zoom);
                            let height = px(image_size.height.0 as f32 * zoom);
                            this.child(img(image).w(width).h(height).with_fallback(|| {
                                h_flex()
                                    .p_4()
                                    .gap_2()
                                    .child(Icon::new(IconName::Warning))
                                    .child("Failed to load SVG image")
                                    .into_any_element()
                            }))
                        }
                        Some(Err(e)) => this.child(div().p_4().child(e).into_any_element()),
                        None => this.child(div().p_4().child("No SVG file selected")),
                    }),
            )
            .when(has_image, |this| {
                this.child(
                    h_flex()
                        .absolute()
                        .bottom_2()
                        .right_2()
                        .gap_1()
                        .p_1()
                        .rounded_md()
                        .border_1()
                        .border_color(border)
                        .bg(elevated_surface_background)
                        .child(
                            IconButton::new("svg-theme-toggle", IconName::Screen)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text(theme_tooltip))
                                .on_click(cx.listener(|this, _, _, cx| this.cycle_theme_mode(cx))),
                        )
                        .child(
                            IconButton::new("svg-zoom-out", IconName::Dash)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Zoom out"))
                                .on_click(cx.listener(|this, _, _, cx| this.zoom_out(cx))),
                        )
                        .child(
                            IconButton::new("svg-zoom-in", IconName::Plus)
                                .icon_size(IconSize::Small)
                                .tooltip(Tooltip::text("Zoom in"))
                                .on_click(cx.listener(|this, _, _, cx| this.zoom_in(cx))),
                        ),
                )
            })
    }
}

impl Focusable for SvgPreviewView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for SvgPreviewView {}

impl Item for SvgPreviewView {
    type Event = ();

    fn tab_icon(&self, _window: &Window, cx: &App) -> Option<Icon> {
        self.buffer
            .as_ref()
            .and_then(|buffer| buffer.read(cx).file())
            .and_then(|file| FileIcons::get_icon(file.path().as_std_path(), cx))
            .map(Icon::from_path)
            .or_else(|| Some(Icon::new(IconName::Image)))
    }

    fn tab_content_text(&self, _detail: usize, cx: &App) -> SharedString {
        self.buffer
            .as_ref()
            .and_then(|svg_path| svg_path.read(cx).file())
            .map(|name| format!("Preview {}", name.file_name(cx)).into())
            .unwrap_or_else(|| "SVG Preview".into())
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("svg preview: open")
    }

    fn to_item_events(_event: &Self::Event, _f: &mut dyn FnMut(workspace::item::ItemEvent)) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, hsla};

    #[test]
    fn test_viewer_theme_mode_cycles() {
        assert_eq!(ViewerThemeMode::Match.next(), ViewerThemeMode::Light);
        assert_eq!(ViewerThemeMode::Light.next(), ViewerThemeMode::Dark);
        assert_eq!(ViewerThemeMode::Dark.next(), ViewerThemeMode::Match);
    }

    #[gpui::test]
    fn test_viewer_theme_mode_resolves_named_backdrop(cx: &mut TestAppContext) {
        cx.update(|cx| {
            theme::init(theme::LoadThemes::JustBase, cx);

            let base_theme = ViewerThemeMode::Match.resolve(cx);
            let base_background = base_theme.colors().editor_background;
            let dark_background = hsla(0.62, 0.25, 0.12, 1.0);
            let light_background = hsla(0.10, 0.30, 0.94, 1.0);

            let mut dark = (*base_theme).clone();
            dark.name = ONE_DARK_THEME.into();
            dark.styles.colors.editor_background = dark_background;
            let mut light = (*base_theme).clone();
            light.name = ONE_LIGHT_THEME.into();
            light.styles.colors.editor_background = light_background;
            ThemeRegistry::global(cx).insert_themes([dark, light]);

            assert_ne!(dark_background, light_background);
            assert_ne!(dark_background, base_background);
            assert_ne!(light_background, base_background);

            assert_eq!(
                ViewerThemeMode::Match
                    .resolve(cx)
                    .colors()
                    .editor_background,
                base_background
            );
            assert_eq!(
                ViewerThemeMode::Dark.resolve(cx).colors().editor_background,
                dark_background
            );
            assert_eq!(
                ViewerThemeMode::Light
                    .resolve(cx)
                    .colors()
                    .editor_background,
                light_background
            );
        });
    }
}
