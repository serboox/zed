use gpui::{
    AnyElement, AnyView, ClickEvent, ElementId, Hsla, IntoElement, KeybindingKeystroke, Keystroke,
    Role, Styled, Toggled, Window, div, hsla, prelude::*,
};
use std::{rc::Rc, sync::Arc};

use crate::utils::is_light;
use crate::{Color, Icon, IconName, ToggleState, Tooltip};
use crate::{ElevationIndex, KeyBinding, prelude::*};

// TODO: Checkbox, CheckboxWithLabel, and Switch could all be
// restructured to use a ToggleLike, similar to Button/Buttonlike, Label/Labellike

/// Creates a new checkbox.
pub fn checkbox(id: impl Into<ElementId>, toggle_state: ToggleState) -> Checkbox {
    Checkbox::new(id, toggle_state)
}

/// Creates a new switch.
pub fn switch(id: impl Into<ElementId>, toggle_state: ToggleState) -> Switch {
    Switch::new(id, toggle_state)
}

/// The visual style of a toggle.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum ToggleStyle {
    /// Toggle has a transparent background
    #[default]
    Ghost,
    /// Toggle has a filled background based on the
    /// elevation index of the parent container
    ElevationBased(ElevationIndex),
    /// A custom style using a color to tint the toggle
    Custom(Hsla),
}

/// # Checkbox
///
/// Checkboxes are used for multiple choices, not for mutually exclusive choices.
/// Each checkbox works independently from other checkboxes in the list,
/// therefore checking an additional box does not affect any other selections.
#[derive(IntoElement, RegisterComponent)]
pub struct Checkbox {
    id: ElementId,
    toggle_state: ToggleState,
    style: ToggleStyle,
    disabled: bool,
    placeholder: bool,
    filled: bool,
    visualization: bool,
    label: Option<SharedString>,
    label_size: LabelSize,
    label_color: Color,
    tooltip: Option<Box<dyn Fn(&mut Window, &mut App) -> AnyView>>,
    on_click: Option<Rc<dyn Fn(&ToggleState, &ClickEvent, &mut Window, &mut App) + 'static>>,
    tab_index: Option<isize>,
    aria_label: Option<SharedString>,
}

impl Checkbox {
    /// Creates a new [`Checkbox`].
    pub fn new(id: impl Into<ElementId>, checked: ToggleState) -> Self {
        Self {
            id: id.into(),
            toggle_state: checked,
            style: ToggleStyle::default(),
            disabled: false,
            placeholder: false,
            filled: false,
            visualization: false,
            label: None,
            label_size: LabelSize::Default,
            label_color: Color::Muted,
            tooltip: None,
            on_click: None,
            tab_index: None,
            aria_label: None,
        }
    }

    /// Places the [`Checkbox`] in the keyboard tab order, which also gives it a
    /// focus ring and lets Enter and Space toggle it.
    pub fn tab_index(mut self, tab_index: impl Into<isize>) -> Self {
        self.tab_index = Some(tab_index.into());
        self
    }

    /// Sets the accessible name, for when the visible label is absent or is not
    /// the whole story.
    pub fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
        self.aria_label = Some(label.into());
        self
    }

    /// Sets the disabled state of the [`Checkbox`].
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Sets the disabled state of the [`Checkbox`].
    pub fn placeholder(mut self, placeholder: bool) -> Self {
        self.placeholder = placeholder;
        self
    }

    /// Binds a handler to the [`Checkbox`] that will be called when clicked.
    pub fn on_click(
        mut self,
        handler: impl Fn(&ToggleState, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(move |state, _, window, cx| {
            handler(state, window, cx)
        }));
        self
    }

    pub fn on_click_ext(
        mut self,
        handler: impl Fn(&ToggleState, &ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }

    /// Sets the `fill` setting of the checkbox, indicating whether it should be filled.
    pub fn fill(mut self) -> Self {
        self.filled = true;
        self
    }

    /// Makes the checkbox look enabled but without pointer cursor and hover styles.
    /// Primarily used for uninteractive markdown previews.
    pub fn visualization_only(mut self, visualization: bool) -> Self {
        self.visualization = visualization;
        self
    }

    /// Sets the style of the checkbox using the specified [`ToggleStyle`].
    pub fn style(mut self, style: ToggleStyle) -> Self {
        self.style = style;
        self
    }

    /// Match the style of the checkbox to the current elevation using [`ToggleStyle::ElevationBased`].
    pub fn elevation(mut self, elevation: ElevationIndex) -> Self {
        self.style = ToggleStyle::ElevationBased(elevation);
        self
    }

    /// Sets the tooltip for the checkbox.
    pub fn tooltip(mut self, tooltip: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Self {
        self.tooltip = Some(Box::new(tooltip));
        self
    }

    /// Set the label for the checkbox.
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn label_size(mut self, size: LabelSize) -> Self {
        self.label_size = size;
        self
    }

    pub fn label_color(mut self, color: Color) -> Self {
        self.label_color = color;
        self
    }
}

impl Checkbox {
    fn bg_color(&self, cx: &App) -> Hsla {
        let style = self.style.clone();
        match (style, self.filled) {
            (ToggleStyle::Ghost, false) => cx.theme().colors().ghost_element_background,
            (ToggleStyle::Ghost, true) => cx.theme().colors().element_background,
            (ToggleStyle::ElevationBased(_), false) => gpui::transparent_black(),
            (ToggleStyle::ElevationBased(elevation), true) => elevation.darker_bg(cx),
            (ToggleStyle::Custom(_), false) => gpui::transparent_black(),
            (ToggleStyle::Custom(color), true) => color.opacity(0.2),
        }
    }

    fn border_color(&self, cx: &App) -> Hsla {
        if self.disabled {
            return cx.theme().colors().border_variant;
        }

        match self.style.clone() {
            ToggleStyle::Ghost => cx.theme().colors().border,
            ToggleStyle::ElevationBased(_) => cx.theme().colors().border,
            ToggleStyle::Custom(color) => color.opacity(0.3),
        }
    }

    /// The border color while the pointer is over the checkbox. It is a step
    /// *stronger* than the resting border: a hover that fades the outline reads
    /// as the control going away.
    fn hover_border_color(&self, cx: &mut App) -> Hsla {
        let base = self.border_color(cx);
        let step = if is_light(cx) { -0.14 } else { 0.16 };
        hsla(base.h, base.s, (base.l + step).clamp(0.0, 1.0), base.a)
    }

    /// The full footprint of the checkbox, which callers use to line siblings up
    /// with it. It is also the pointer target, so it may not go below the 24
    /// pixels that WCAG 2.5.8 asks for.
    pub fn container_size() -> Pixels {
        px(28.0)
    }

    fn box_size() -> Pixels {
        px(18.0)
    }
}

impl RenderOnce for Checkbox {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let group_id = format!("checkbox_group_{:?}", self.id);
        let color = if self.disabled {
            Color::Disabled
        } else {
            Color::Selected
        };

        let icon = match self.toggle_state {
            ToggleState::Selected => {
                if self.placeholder {
                    None
                } else {
                    Some(
                        Icon::new(IconName::Check)
                            .size(IconSize::XSmall)
                            .color(color),
                    )
                }
            }
            ToggleState::Indeterminate => Some(
                Icon::new(IconName::Dash)
                    .size(IconSize::XSmall)
                    .color(color),
            ),
            ToggleState::Unselected => None,
        };

        let bg_color = self.bg_color(cx);
        let border_color = self.border_color(cx);
        let hover_border_color = self.hover_border_color(cx);
        let focus_border_color = cx.theme().colors().border_focused;
        let transparent_border = cx.theme().colors().border_transparent;
        let aria_label = self.aria_label.clone().or_else(|| self.label.clone());
        let keyboard_on_click = self.on_click.clone();
        let toggle_state = self.toggle_state;

        let size = Self::container_size();

        let checkbox = h_flex()
            .group(group_id.clone())
            .id(self.id.clone())
            .debug_selector({
                let id = self.id.clone();
                move || format!("CHECKBOX-{}", id)
            })
            .role(Role::CheckBox)
            .when_some(aria_label, |this, label| this.aria_label(label))
            .aria_toggled(match self.toggle_state {
                ToggleState::Selected => Toggled::True,
                ToggleState::Indeterminate => Toggled::Mixed,
                ToggleState::Unselected => Toggled::False,
            })
            .size(size)
            .flex_none()
            .items_center()
            .justify_center()
            .border_2()
            .border_color(transparent_border)
            .rounded_md()
            .when_some(
                self.tab_index
                    .filter(|_| !self.disabled && !self.visualization),
                |this, tab_index| {
                    this.tab_index(tab_index)
                        .focus_visible(move |mut style| {
                            style.border_color = Some(focus_border_color);
                            style
                        })
                        .when_some(keyboard_on_click, |this, on_click| {
                            // Keyboard only. A pointer click on the indicator also
                            // bubbles to the row handler below, and both listeners
                            // would fire for one press.
                            this.on_click(move |event, window, cx| {
                                if matches!(event, ClickEvent::Keyboard(_)) {
                                    on_click(&toggle_state.inverse(), event, window, cx)
                                }
                            })
                        })
                },
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .justify_center()
                    .items_center()
                    .size(Self::box_size())
                    .rounded(px(5.))
                    .bg(bg_color)
                    .border_1()
                    .border_color(border_color)
                    .when(self.disabled, |this| this.cursor_not_allowed())
                    .when(self.disabled, |this| {
                        this.bg(cx.theme().colors().element_disabled.opacity(0.6))
                    })
                    .when(!self.disabled && !self.visualization, |this| {
                        this.group_hover(group_id.clone(), |el| el.border_color(hover_border_color))
                    })
                    .when(self.placeholder, |this| {
                        this.child(
                            div()
                                .flex_none()
                                .rounded_full()
                                .bg(color.color(cx).alpha(0.5))
                                .size(px(4.)),
                        )
                    })
                    .children(icon),
            );

        h_flex()
            .id(self.id)
            .map(|this| {
                if self.disabled {
                    this.cursor_not_allowed()
                } else if self.visualization {
                    this.cursor_default()
                } else {
                    this.cursor_pointer()
                }
            })
            .gap(DynamicSpacing::Base06.rems(cx))
            .child(checkbox)
            .when_some(self.label, |this, label| {
                this.child(
                    Label::new(label)
                        .color(self.label_color)
                        .size(self.label_size),
                )
            })
            .when_some(self.tooltip, |this, tooltip| {
                this.tooltip(move |window, cx| tooltip(window, cx))
            })
            .when_some(
                self.on_click.filter(|_| !self.disabled),
                |this, on_click| {
                    this.on_click(move |click, window, cx| {
                        on_click(&self.toggle_state.inverse(), click, window, cx)
                    })
                },
            )
    }
}

/// Creates a new radio button.
pub fn radio_button(id: impl Into<ElementId>, selected: bool) -> RadioButton {
    RadioButton::new(id, selected)
}

/// A radio button: one option out of a set, where choosing one clears the rest.
///
/// Use it when the options are mutually exclusive and all of them should stay
/// visible. For a single on/off answer use [`Checkbox`]; for one that applies
/// immediately use [`Switch`].
///
/// Keyboard: give every radio in a set a `tab_index` so each is reachable.
/// Arrow-key movement *within* a set, which the ARIA radio-group pattern also
/// asks for, needs a focus-owning container and is not provided here yet.
#[derive(IntoElement, RegisterComponent)]
pub struct RadioButton {
    id: ElementId,
    selected: bool,
    style: ToggleStyle,
    disabled: bool,
    label: Option<SharedString>,
    label_size: LabelSize,
    label_color: Color,
    tooltip: Option<Box<dyn Fn(&mut Window, &mut App) -> AnyView>>,
    on_click: Option<Rc<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>>,
    tab_index: Option<isize>,
    aria_label: Option<SharedString>,
}

impl RadioButton {
    /// Creates a new [`RadioButton`].
    pub fn new(id: impl Into<ElementId>, selected: bool) -> Self {
        Self {
            id: id.into(),
            selected,
            style: ToggleStyle::default(),
            disabled: false,
            label: None,
            label_size: LabelSize::Default,
            label_color: Color::Default,
            tooltip: None,
            on_click: None,
            tab_index: None,
            aria_label: None,
        }
    }

    /// Sets the disabled state of the [`RadioButton`].
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Binds a handler called when the option is chosen.
    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }

    /// Sets the style of the [`RadioButton`] using the specified [`ToggleStyle`].
    pub fn style(mut self, style: ToggleStyle) -> Self {
        self.style = style;
        self
    }

    /// Matches the style to the current elevation using [`ToggleStyle::ElevationBased`].
    pub fn elevation(mut self, elevation: ElevationIndex) -> Self {
        self.style = ToggleStyle::ElevationBased(elevation);
        self
    }

    /// Sets the tooltip for the [`RadioButton`].
    pub fn tooltip(mut self, tooltip: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Self {
        self.tooltip = Some(Box::new(tooltip));
        self
    }

    /// Sets the label shown next to the [`RadioButton`].
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Sets the size of the label.
    pub fn label_size(mut self, size: LabelSize) -> Self {
        self.label_size = size;
        self
    }

    /// Sets the color of the label.
    pub fn label_color(mut self, color: Color) -> Self {
        self.label_color = color;
        self
    }

    /// Places the [`RadioButton`] in the keyboard tab order, which also gives it
    /// a focus ring and lets Enter and Space choose it.
    pub fn tab_index(mut self, tab_index: impl Into<isize>) -> Self {
        self.tab_index = Some(tab_index.into());
        self
    }

    /// Sets the accessible name, for when the visible label is absent or is not
    /// the whole story.
    pub fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
        self.aria_label = Some(label.into());
        self
    }

    fn bg_color(&self, cx: &App) -> Hsla {
        match (self.style.clone(), self.selected) {
            (_, false) => gpui::transparent_black(),
            (ToggleStyle::Ghost, true) => cx.theme().colors().element_background,
            (ToggleStyle::ElevationBased(elevation), true) => elevation.darker_bg(cx),
            (ToggleStyle::Custom(color), true) => color.opacity(0.2),
        }
    }

    fn border_color(&self, cx: &App) -> Hsla {
        if self.disabled {
            return cx.theme().colors().border_variant;
        }

        match self.style.clone() {
            ToggleStyle::Ghost | ToggleStyle::ElevationBased(_) => {
                if self.selected {
                    cx.theme().colors().border_selected
                } else {
                    cx.theme().colors().border
                }
            }
            ToggleStyle::Custom(color) => color.opacity(0.3),
        }
    }

    fn hover_border_color(&self, cx: &mut App) -> Hsla {
        let base = self.border_color(cx);
        let step = if is_light(cx) { -0.14 } else { 0.16 };
        hsla(base.h, base.s, (base.l + step).clamp(0.0, 1.0), base.a)
    }

    /// The full footprint, which is also the pointer target. It matches the
    /// height of a default button so a radio lines up with the controls beside
    /// it, and it clears the 24 pixels WCAG 2.5.8 asks for.
    pub fn container_size() -> Pixels {
        px(28.0)
    }
}

impl RenderOnce for RadioButton {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let group_id = format!("radio_group_{:?}", self.id);
        let bg_color = self.bg_color(cx);
        let border_color = self.border_color(cx);
        let hover_border_color = self.hover_border_color(cx);
        let focus_border_color = cx.theme().colors().border_focused;
        let transparent_border = cx.theme().colors().border_transparent;
        let dot_color = if self.disabled {
            Color::Disabled
        } else {
            Color::Selected
        };
        let aria_label = self.aria_label.clone().or_else(|| self.label.clone());
        let keyboard_on_click = self.on_click.clone();
        let pointer_on_click = self.on_click.clone();

        let indicator = h_flex()
            .group(group_id.clone())
            .id(self.id.clone())
            .debug_selector({
                let id = self.id.clone();
                move || format!("RADIO-{}", id)
            })
            .role(Role::RadioButton)
            .when_some(aria_label, |this, label| this.aria_label(label))
            .aria_toggled(if self.selected {
                Toggled::True
            } else {
                Toggled::False
            })
            .size(Self::container_size())
            .flex_none()
            .items_center()
            .justify_center()
            .border_2()
            .border_color(transparent_border)
            .rounded_full()
            .when_some(
                self.tab_index.filter(|_| !self.disabled),
                |this, tab_index| {
                    this.tab_index(tab_index)
                        .focus_visible(move |mut style| {
                            style.border_color = Some(focus_border_color);
                            style
                        })
                        .when_some(keyboard_on_click, |this, on_click| {
                            // Keyboard only, for the same reason as in `Checkbox`:
                            // a pointer click here also reaches the row handler.
                            this.on_click(move |event, window, cx| {
                                if matches!(event, ClickEvent::Keyboard(_)) {
                                    on_click(event, window, cx)
                                }
                            })
                        })
                },
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .justify_center()
                    .items_center()
                    .size(Checkbox::box_size())
                    .rounded_full()
                    .bg(bg_color)
                    .border_1()
                    .border_color(border_color)
                    .when(self.disabled, |this| {
                        this.cursor_not_allowed().bg(cx
                            .theme()
                            .colors()
                            .element_disabled
                            .opacity(0.6))
                    })
                    .when(!self.disabled, |this| {
                        this.group_hover(group_id.clone(), |el| el.border_color(hover_border_color))
                    })
                    .when(self.selected, |this| {
                        this.child(
                            div()
                                .flex_none()
                                .rounded_full()
                                .bg(dot_color.color(cx))
                                .size(px(9.)),
                        )
                    }),
            );

        h_flex()
            .id((self.id.clone(), "radio-row"))
            .map(|this| {
                if self.disabled {
                    this.cursor_not_allowed()
                } else {
                    this.cursor_pointer()
                }
            })
            .gap(DynamicSpacing::Base06.rems(cx))
            .child(indicator)
            .when_some(self.label, |this, label| {
                this.child(
                    Label::new(label)
                        .color(self.label_color)
                        .size(self.label_size),
                )
            })
            .when_some(self.tooltip, |this, tooltip| {
                this.tooltip(move |window, cx| tooltip(window, cx))
            })
            .when_some(
                pointer_on_click.filter(|_| !self.disabled),
                |this, on_click| {
                    this.on_click(move |event, window, cx| on_click(event, window, cx))
                },
            )
    }
}

/// Defines the color for a switch component.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy, Default)]
pub enum SwitchColor {
    #[default]
    Accent,
    Custom(Hsla),
}

impl SwitchColor {
    fn get_colors(&self, is_on: bool, cx: &App) -> (Hsla, Hsla) {
        if !is_on {
            return (
                cx.theme().colors().element_disabled,
                cx.theme().colors().border,
            );
        }

        match self {
            SwitchColor::Accent => {
                let status = cx.theme().status();
                let colors = cx.theme().colors();
                (status.info.opacity(0.4), colors.text_accent.opacity(0.2))
            }
            SwitchColor::Custom(color) => (*color, color.opacity(0.6)),
        }
    }
}

impl From<SwitchColor> for Color {
    fn from(color: SwitchColor) -> Self {
        match color {
            SwitchColor::Accent => Color::Accent,
            SwitchColor::Custom(_) => Color::Default,
        }
    }
}

/// Defines the color for a switch component.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy, Default)]
pub enum SwitchLabelPosition {
    Start,
    #[default]
    End,
}

/// # Switch
///
/// Switches are used to represent opposite states, such as enabled or disabled.
#[derive(IntoElement, RegisterComponent)]
pub struct Switch {
    id: ElementId,
    toggle_state: ToggleState,
    disabled: bool,
    on_click: Option<Rc<dyn Fn(&ToggleState, &mut Window, &mut App) + 'static>>,
    label: Option<SharedString>,
    label_position: Option<SwitchLabelPosition>,
    label_size: LabelSize,
    label_color: Color,
    full_width: bool,
    key_binding: Option<KeyBinding>,
    color: SwitchColor,
    tab_index: Option<isize>,
    aria_label: Option<SharedString>,
    aria_description: Option<SharedString>,
}

impl Switch {
    /// Creates a new [`Switch`].
    pub fn new(id: impl Into<ElementId>, state: ToggleState) -> Self {
        Self {
            id: id.into(),
            toggle_state: state,
            disabled: false,
            on_click: None,
            label: None,
            label_position: None,
            label_size: LabelSize::Small,
            label_color: Color::Default,
            full_width: false,
            key_binding: None,
            color: SwitchColor::default(),
            tab_index: None,
            aria_label: None,
            aria_description: None,
        }
    }

    /// Sets the color of the switch using the specified [`SwitchColor`].
    pub fn color(mut self, color: SwitchColor) -> Self {
        self.color = color;
        self
    }

    /// Sets the disabled state of the [`Switch`].
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Binds a handler to the [`Switch`] that will be called when clicked.
    pub fn on_click(
        mut self,
        handler: impl Fn(&ToggleState, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Rc::new(handler));
        self
    }

    /// Sets the label of the [`Switch`].
    pub fn label(mut self, label: impl Into<SharedString>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn label_position(
        mut self,
        label_position: impl Into<Option<SwitchLabelPosition>>,
    ) -> Self {
        self.label_position = label_position.into();
        self
    }

    pub fn label_size(mut self, size: LabelSize) -> Self {
        self.label_size = size;
        self
    }

    pub fn label_color(mut self, color: Color) -> Self {
        self.label_color = color;
        self
    }

    pub fn full_width(mut self, full_width: bool) -> Self {
        self.full_width = full_width;
        self
    }

    /// Display the keybinding that triggers the switch action.
    pub fn key_binding(mut self, key_binding: impl Into<Option<KeyBinding>>) -> Self {
        self.key_binding = key_binding.into();
        self
    }

    pub fn tab_index(mut self, tab_index: impl Into<isize>) -> Self {
        self.tab_index = Some(tab_index.into());
        self
    }

    /// Sets the label announced by assistive technology.
    /// Defaults to the switch's visible label, if any.
    pub fn aria_label(mut self, label: impl Into<SharedString>) -> Self {
        self.aria_label = Some(label.into());
        self
    }

    /// Sets the supplementary description announced by assistive technology
    /// after the switch's name, role, and state.
    pub fn aria_description(mut self, description: impl Into<SharedString>) -> Self {
        self.aria_description = Some(description.into());
        self
    }
}

impl RenderOnce for Switch {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let is_on = self.toggle_state == ToggleState::Selected;
        let adjust_ratio = if is_light(cx) { 1.5 } else { 1.0 };

        let base_color = cx.theme().colors().text;
        let thumb_color = base_color;
        let (bg_color, border_color) = self.color.get_colors(is_on, cx);

        let bg_hover_color = if is_on {
            bg_color.blend(base_color.opacity(0.16 * adjust_ratio))
        } else {
            bg_color.blend(base_color.opacity(0.05 * adjust_ratio))
        };

        let thumb_opacity = match (is_on, self.disabled) {
            (_, true) => 0.2,
            (true, false) => 1.0,
            (false, false) => 0.5,
        };

        let group_id = format!("switch_group_{:?}", self.id);
        let label = self.label;
        let aria_label = self.aria_label.or_else(|| label.clone());
        let aria_description = self.aria_description;
        let aria_keyshortcuts = self
            .key_binding
            .as_ref()
            .and_then(|key_binding| key_binding.keyboard_shortcut_text(window, cx));

        let switch = div()
            .id((self.id.clone(), "switch"))
            .role(Role::Switch)
            .when_some(aria_label, |this, label| this.aria_label(label))
            .when_some(aria_keyshortcuts, |this, keyshortcuts| {
                this.aria_keyshortcuts(keyshortcuts)
            })
            .when_some(aria_description, |this, description| {
                this.aria_description(description)
            })
            .aria_toggled(match self.toggle_state {
                ToggleState::Selected => Toggled::True,
                ToggleState::Indeterminate => Toggled::Mixed,
                ToggleState::Unselected => Toggled::False,
            })
            .p(px(1.0))
            .border_2()
            .border_color(cx.theme().colors().border_transparent)
            .rounded_full()
            .when_some(
                self.tab_index.filter(|_| !self.disabled),
                |this, tab_index| {
                    this.tab_index(tab_index)
                        .focus_visible(|mut style| {
                            style.border_color = Some(cx.theme().colors().border_focused);
                            style
                        })
                        .when_some(self.on_click.clone(), |this, on_click| {
                            // Keyboard only: a pointer click on the track also
                            // bubbles to the row handler, which would fire the
                            // callback a second time for one press.
                            this.on_click(move |event, window, cx| {
                                if matches!(event, ClickEvent::Keyboard(_)) {
                                    on_click(&self.toggle_state.inverse(), window, cx)
                                }
                            })
                        })
                },
            )
            .child(
                h_flex()
                    .w(DynamicSpacing::Base32.rems(cx))
                    .h(DynamicSpacing::Base20.rems(cx))
                    .group(group_id.clone())
                    .child(
                        h_flex()
                            .when(is_on, |on| on.justify_end())
                            .when(!is_on, |off| off.justify_start())
                            .size_full()
                            .rounded_full()
                            .px(DynamicSpacing::Base02.px(cx))
                            .bg(bg_color)
                            .when(!self.disabled, |this| {
                                this.group_hover(group_id.clone(), |el| el.bg(bg_hover_color))
                            })
                            .border_1()
                            .border_color(border_color)
                            .child(
                                div()
                                    .size(DynamicSpacing::Base12.rems(cx))
                                    .rounded_full()
                                    .bg(thumb_color)
                                    .opacity(thumb_opacity),
                            ),
                    ),
            );

        h_flex()
            .id(self.id)
            .cursor_pointer()
            .gap(DynamicSpacing::Base06.rems(cx))
            .when(self.full_width, |this| this.w_full().justify_between())
            .when(
                self.label_position == Some(SwitchLabelPosition::Start),
                |this| {
                    this.when_some(label.clone(), |this, label| {
                        this.child(
                            Label::new(label)
                                .size(self.label_size)
                                .color(self.label_color),
                        )
                    })
                },
            )
            .child(switch)
            .when(
                self.label_position == Some(SwitchLabelPosition::End),
                |this| {
                    this.when_some(label, |this, label| {
                        this.child(
                            Label::new(label)
                                .size(self.label_size)
                                .color(self.label_color),
                        )
                    })
                },
            )
            .children(self.key_binding)
            .when_some(
                self.on_click.filter(|_| !self.disabled),
                |this, on_click| {
                    this.on_click(move |_, window, cx| {
                        on_click(&self.toggle_state.inverse(), window, cx)
                    })
                },
            )
    }
}

/// # SwitchField
///
/// A field component that combines a label, description, and switch into one reusable component.
///
/// # Examples
///
/// ```
/// use ui::prelude::*;
/// use ui::{SwitchField, ToggleState};
///
/// let switch_field = SwitchField::new(
///     "feature-toggle",
///     Some("Enable feature"),
///     Some("This feature adds new functionality to the app.".into()),
///     ToggleState::Unselected,
///     |state, window, cx| {
///         // Logic here
///     }
/// );
/// ```
#[derive(IntoElement, RegisterComponent)]
pub struct SwitchField {
    id: ElementId,
    label: Option<SharedString>,
    description: Option<SharedString>,
    toggle_state: ToggleState,
    on_click: Arc<dyn Fn(&ToggleState, &mut Window, &mut App) + 'static>,
    disabled: bool,
    color: SwitchColor,
    tooltip: Option<Rc<dyn Fn(&mut Window, &mut App) -> AnyView>>,
    tab_index: Option<isize>,
}

impl SwitchField {
    pub fn new(
        id: impl Into<ElementId>,
        label: Option<impl Into<SharedString>>,
        description: Option<SharedString>,
        toggle_state: impl Into<ToggleState>,
        on_click: impl Fn(&ToggleState, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.map(Into::into),
            description,
            toggle_state: toggle_state.into(),
            on_click: Arc::new(on_click),
            disabled: false,
            color: SwitchColor::Accent,
            tooltip: None,
            tab_index: None,
        }
    }

    pub fn description(mut self, description: impl Into<SharedString>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Sets the color of the switch using the specified [`SwitchColor`].
    /// This changes the color scheme of the switch when it's in the "on" state.
    pub fn color(mut self, color: SwitchColor) -> Self {
        self.color = color;
        self
    }

    pub fn tooltip(mut self, tooltip: impl Fn(&mut Window, &mut App) -> AnyView + 'static) -> Self {
        self.tooltip = Some(Rc::new(tooltip));
        self
    }

    pub fn tab_index(mut self, tab_index: isize) -> Self {
        self.tab_index = Some(tab_index);
        self
    }
}

impl RenderOnce for SwitchField {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let tooltip = self
            .tooltip
            .zip(self.label.clone())
            .map(|(tooltip_fn, label)| {
                h_flex().gap_0p5().child(Label::new(label)).child(
                    IconButton::new("tooltip_button", IconName::Info)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .shape(crate::IconButtonShape::Square)
                        .style(ButtonStyle::Transparent)
                        .tooltip({
                            let tooltip = tooltip_fn.clone();
                            move |window, cx| tooltip(window, cx)
                        })
                        .on_click(|_, _, _| {}), // Intentional empty on click handler so that clicking on the info tooltip icon doesn't trigger the switch toggle
                )
            });

        h_flex()
            .id((self.id.clone(), "container"))
            .when(!self.disabled, |this| {
                this.hover(|this| this.cursor_pointer())
            })
            .w_full()
            .gap_4()
            .justify_between()
            .flex_wrap()
            .child(match (&self.description, tooltip) {
                (Some(description), Some(tooltip)) => v_flex()
                    .gap_0p5()
                    .max_w_5_6()
                    .child(tooltip)
                    .child(Label::new(description.clone()).color(Color::Muted))
                    .into_any_element(),
                (Some(description), None) => v_flex()
                    .gap_0p5()
                    .max_w_5_6()
                    .when_some(self.label, |this, label| this.child(Label::new(label)))
                    .child(Label::new(description.clone()).color(Color::Muted))
                    .into_any_element(),
                (None, Some(tooltip)) => tooltip.into_any_element(),
                (None, None) => {
                    if let Some(label) = self.label.clone() {
                        Label::new(label).into_any_element()
                    } else {
                        gpui::Empty.into_any_element()
                    }
                }
            })
            .child(
                Switch::new((self.id.clone(), "switch"), self.toggle_state)
                    .color(self.color)
                    .disabled(self.disabled)
                    .when_some(
                        self.tab_index.filter(|_| !self.disabled),
                        |this, tab_index| this.tab_index(tab_index),
                    )
                    .on_click({
                        let on_click = self.on_click.clone();
                        move |state, window, cx| {
                            (on_click)(state, window, cx);
                        }
                    }),
            )
            .when(!self.disabled, |this| {
                this.on_click({
                    let on_click = self.on_click.clone();
                    let toggle_state = self.toggle_state;
                    move |_click, window, cx| {
                        (on_click)(&toggle_state.inverse(), window, cx);
                    }
                })
            })
    }
}

impl Component for SwitchField {
    fn scope() -> ComponentScope {
        ComponentScope::Input
    }

    fn description() -> &'static str {
        "A field component that combines a label, description, and switch"
    }

    fn preview(_window: &mut Window, _cx: &mut App) -> AnyElement {
        v_flex()
            .gap_6()
            .children(vec![
                example_group_with_title(
                    "States",
                    vec![
                        single_example(
                            "Unselected",
                            SwitchField::new(
                                "switch_field_unselected",
                                Some("Enable notifications"),
                                Some("Receive notifications when new messages arrive.".into()),
                                ToggleState::Unselected,
                                |_, _, _| {},
                            )
                            .into_any_element(),
                        ),
                        single_example(
                            "Selected",
                            SwitchField::new(
                                "switch_field_selected",
                                Some("Enable notifications"),
                                Some("Receive notifications when new messages arrive.".into()),
                                ToggleState::Selected,
                                |_, _, _| {},
                            )
                            .into_any_element(),
                        ),
                    ],
                ),
                example_group_with_title(
                    "Colors",
                    vec![
                        single_example(
                            "Default",
                            SwitchField::new(
                                "switch_field_default",
                                Some("Default color"),
                                Some("This uses the default switch color.".into()),
                                ToggleState::Selected,
                                |_, _, _| {},
                            )
                            .into_any_element(),
                        ),
                        single_example(
                            "Accent",
                            SwitchField::new(
                                "switch_field_accent",
                                Some("Accent color"),
                                Some("This uses the accent color scheme.".into()),
                                ToggleState::Selected,
                                |_, _, _| {},
                            )
                            .color(SwitchColor::Accent)
                            .into_any_element(),
                        ),
                    ],
                ),
                example_group_with_title(
                    "Disabled",
                    vec![single_example(
                        "Disabled",
                        SwitchField::new(
                            "switch_field_disabled",
                            Some("Disabled field"),
                            Some("This field is disabled and cannot be toggled.".into()),
                            ToggleState::Selected,
                            |_, _, _| {},
                        )
                        .disabled(true)
                        .into_any_element(),
                    )],
                ),
                example_group_with_title(
                    "No Description",
                    vec![single_example(
                        "No Description",
                        SwitchField::new(
                            "switch_field_disabled",
                            Some("Disabled field"),
                            None,
                            ToggleState::Selected,
                            |_, _, _| {},
                        )
                        .into_any_element(),
                    )],
                ),
                example_group_with_title(
                    "With Tooltip",
                    vec![
                        single_example(
                            "Tooltip with Description",
                            SwitchField::new(
                                "switch_field_tooltip_with_desc",
                                Some("Nice Feature"),
                                Some("Enable advanced configuration options.".into()),
                                ToggleState::Unselected,
                                |_, _, _| {},
                            )
                            .tooltip(Tooltip::text("This is content for this tooltip!"))
                            .into_any_element(),
                        ),
                        single_example(
                            "Tooltip without Description",
                            SwitchField::new(
                                "switch_field_tooltip_no_desc",
                                Some("Nice Feature"),
                                None,
                                ToggleState::Selected,
                                |_, _, _| {},
                            )
                            .tooltip(Tooltip::text("This is content for this tooltip!"))
                            .into_any_element(),
                        ),
                    ],
                ),
            ])
            .into_any_element()
    }
}

impl Component for RadioButton {
    fn scope() -> ComponentScope {
        ComponentScope::Input
    }

    fn description() -> &'static str {
        "One option out of a mutually exclusive set, where choosing one clears the rest"
    }

    fn preview(_window: &mut Window, _cx: &mut App) -> AnyElement {
        v_flex()
            .gap_6()
            .children(vec![
                example_group_with_title(
                    "States",
                    vec![
                        single_example(
                            "Unselected",
                            RadioButton::new("radio_unselected", false).into_any_element(),
                        ),
                        single_example(
                            "Selected",
                            RadioButton::new("radio_selected", true).into_any_element(),
                        ),
                        single_example(
                            "Disabled",
                            RadioButton::new("radio_disabled", false)
                                .disabled(true)
                                .into_any_element(),
                        ),
                        single_example(
                            "Disabled and selected",
                            RadioButton::new("radio_disabled_selected", true)
                                .disabled(true)
                                .into_any_element(),
                        ),
                    ],
                ),
                example_group_with_title(
                    "A set",
                    vec![single_example(
                        "Three options, one chosen",
                        v_flex()
                            .gap_1()
                            .child(RadioButton::new("radio_set_a", true).label("Ask every time"))
                            .child(RadioButton::new("radio_set_b", false).label("Always allow"))
                            .child(RadioButton::new("radio_set_c", false).label("Never allow"))
                            .into_any_element(),
                    )],
                ),
            ])
            .into_any_element()
    }
}

impl Component for Checkbox {
    fn scope() -> ComponentScope {
        ComponentScope::Input
    }

    fn description() -> &'static str {
        "A checkbox component that can be used for multiple choice selections"
    }

    fn preview(_window: &mut Window, _cx: &mut App) -> AnyElement {
        v_flex()
            .gap_6()
            .children(vec![
                example_group_with_title(
                    "States",
                    vec![
                        single_example(
                            "Unselected",
                            Checkbox::new("checkbox_unselected", ToggleState::Unselected)
                                .into_any_element(),
                        ),
                        single_example(
                            "Placeholder",
                            Checkbox::new("checkbox_indeterminate", ToggleState::Selected)
                                .placeholder(true)
                                .into_any_element(),
                        ),
                        single_example(
                            "Indeterminate",
                            Checkbox::new("checkbox_indeterminate", ToggleState::Indeterminate)
                                .into_any_element(),
                        ),
                        single_example(
                            "Selected",
                            Checkbox::new("checkbox_selected", ToggleState::Selected)
                                .into_any_element(),
                        ),
                    ],
                ),
                example_group_with_title(
                    "Styles",
                    vec![
                        single_example(
                            "Default",
                            Checkbox::new("checkbox_default", ToggleState::Selected)
                                .into_any_element(),
                        ),
                        single_example(
                            "Filled",
                            Checkbox::new("checkbox_filled", ToggleState::Selected)
                                .fill()
                                .into_any_element(),
                        ),
                        single_example(
                            "ElevationBased",
                            Checkbox::new("checkbox_elevation", ToggleState::Selected)
                                .style(ToggleStyle::ElevationBased(ElevationIndex::EditorSurface))
                                .into_any_element(),
                        ),
                        single_example(
                            "Custom Color",
                            Checkbox::new("checkbox_custom", ToggleState::Selected)
                                .style(ToggleStyle::Custom(hsla(142.0 / 360., 0.68, 0.45, 0.7)))
                                .into_any_element(),
                        ),
                    ],
                ),
                example_group_with_title(
                    "Disabled",
                    vec![
                        single_example(
                            "Unselected",
                            Checkbox::new("checkbox_disabled_unselected", ToggleState::Unselected)
                                .disabled(true)
                                .into_any_element(),
                        ),
                        single_example(
                            "Selected",
                            Checkbox::new("checkbox_disabled_selected", ToggleState::Selected)
                                .disabled(true)
                                .into_any_element(),
                        ),
                    ],
                ),
                example_group_with_title(
                    "With Label",
                    vec![single_example(
                        "Default",
                        Checkbox::new("checkbox_with_label", ToggleState::Selected)
                            .label("Always save on quit")
                            .into_any_element(),
                    )],
                ),
                example_group_with_title(
                    "Extra",
                    vec![single_example(
                        "Visualization-Only",
                        Checkbox::new("viz_only", ToggleState::Selected)
                            .visualization_only(true)
                            .into_any_element(),
                    )],
                ),
            ])
            .into_any_element()
    }
}

impl Component for Switch {
    fn scope() -> ComponentScope {
        ComponentScope::Input
    }

    fn description() -> &'static str {
        "A switch component that represents binary states like on/off"
    }

    fn preview(_window: &mut Window, _cx: &mut App) -> AnyElement {
        v_flex()
            .gap_6()
            .children(vec![
                example_group_with_title(
                    "States",
                    vec![
                        single_example(
                            "Off",
                            Switch::new("switch_off", ToggleState::Unselected)
                                .on_click(|_, _, _cx| {})
                                .into_any_element(),
                        ),
                        single_example(
                            "On",
                            Switch::new("switch_on", ToggleState::Selected)
                                .on_click(|_, _, _cx| {})
                                .into_any_element(),
                        ),
                    ],
                ),
                example_group_with_title(
                    "Colors",
                    vec![
                        single_example(
                            "Accent (Default)",
                            Switch::new("switch_accent_style", ToggleState::Selected)
                                .on_click(|_, _, _cx| {})
                                .into_any_element(),
                        ),
                        single_example(
                            "Custom",
                            Switch::new("switch_custom_style", ToggleState::Selected)
                                .color(SwitchColor::Custom(hsla(300.0 / 360.0, 0.6, 0.6, 1.0)))
                                .on_click(|_, _, _cx| {})
                                .into_any_element(),
                        ),
                    ],
                ),
                example_group_with_title(
                    "Disabled",
                    vec![
                        single_example(
                            "Off",
                            Switch::new("switch_disabled_off", ToggleState::Unselected)
                                .disabled(true)
                                .into_any_element(),
                        ),
                        single_example(
                            "On",
                            Switch::new("switch_disabled_on", ToggleState::Selected)
                                .disabled(true)
                                .into_any_element(),
                        ),
                    ],
                ),
                example_group_with_title(
                    "With Label",
                    vec![
                        single_example(
                            "Start Label",
                            Switch::new("switch_with_label_start", ToggleState::Selected)
                                .label("Always save on quit")
                                .label_position(SwitchLabelPosition::Start)
                                .into_any_element(),
                        ),
                        single_example(
                            "End Label",
                            Switch::new("switch_with_label_end", ToggleState::Selected)
                                .label("Always save on quit")
                                .label_position(SwitchLabelPosition::End)
                                .into_any_element(),
                        ),
                        single_example(
                            "Default Size Label",
                            Switch::new("switch_with_label_default_size", ToggleState::Selected)
                                .label("Always save on quit")
                                .label_size(LabelSize::Default)
                                .into_any_element(),
                        ),
                        single_example(
                            "Small Size Label",
                            Switch::new("switch_with_label_small_size", ToggleState::Selected)
                                .label("Always save on quit")
                                .label_size(LabelSize::Small)
                                .into_any_element(),
                        ),
                    ],
                ),
                example_group_with_title(
                    "With Keybinding",
                    vec![single_example(
                        "Keybinding",
                        Switch::new("switch_with_keybinding", ToggleState::Selected)
                            .key_binding(Some(KeyBinding::from_keystrokes(
                                vec![KeybindingKeystroke::from_keystroke(
                                    Keystroke::parse("cmd-s").unwrap(),
                                )]
                                .into(),
                                false,
                            )))
                            .into_any_element(),
                    )],
                ),
            ])
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Context, IntoElement, Modifiers, Render, TestAppContext, Window, px};
    use std::cell::Cell;
    use std::rc::Rc;

    use crate::{Checkbox, RadioButton, ToggleState, prelude::*};

    struct ControlsHost;

    impl Render for ControlsHost {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            v_flex()
                .child(
                    Checkbox::new("probe-checkbox", ToggleState::Unselected)
                        .tab_index(0_isize)
                        .label("Run on start"),
                )
                .child(
                    RadioButton::new("probe-radio", true)
                        .tab_index(1_isize)
                        .label("Always allow"),
                )
        }
    }

    // The pointer target of a toggle is its whole container, and WCAG 2.5.8 puts
    // the floor at 24 x 24 for targets that sit next to each other. Asserting on
    // the painted bounds rather than on `container_size()` is deliberate: the
    // checkbox used to declare 20 while painting 24, so the constant and the
    // pixels disagreed.
    #[gpui::test]
    async fn a_checkbox_and_a_radio_are_big_enough_to_hit(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let (_host, cx) = cx.add_window_view(|_window, _cx| ControlsHost);
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        let checkbox = cx
            .debug_bounds("CHECKBOX-probe-checkbox")
            .expect("the checkbox is painted");
        let radio = cx
            .debug_bounds("RADIO-probe-radio")
            .expect("the radio button is painted");

        assert!(
            checkbox.size.width >= px(24.) && checkbox.size.height >= px(24.),
            "a checkbox target must be at least 24 x 24, painted {:?}",
            checkbox.size
        );
        assert!(
            radio.size.width >= px(24.) && radio.size.height >= px(24.),
            "a radio target must be at least 24 x 24, painted {:?}",
            radio.size
        );
    }

    // Hover has to make the outline easier to see, not fainter. The checkbox
    // used to hover to `border_color.alpha(0.7)`, which reads as the control
    // fading out from under the pointer.
    #[gpui::test]
    async fn hover_strengthens_the_outline(cx: &mut TestAppContext) {
        let (resting, hovered, radio_resting, radio_hovered) = cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);

            let checkbox = Checkbox::new("hover-probe", ToggleState::Unselected);
            let radio = RadioButton::new("hover-probe-radio", false);
            (
                checkbox.border_color(cx),
                checkbox.hover_border_color(cx),
                radio.border_color(cx),
                radio.hover_border_color(cx),
            )
        });

        let dark = resting.l < 0.5;
        if dark {
            assert!(
                hovered.l > resting.l,
                "on a dark theme the hovered checkbox outline must be lighter: {} -> {}",
                resting.l,
                hovered.l
            );
            assert!(
                radio_hovered.l > radio_resting.l,
                "on a dark theme the hovered radio outline must be lighter: {} -> {}",
                radio_resting.l,
                radio_hovered.l
            );
        } else {
            assert!(
                hovered.l < resting.l,
                "on a light theme the hovered checkbox outline must be darker: {} -> {}",
                resting.l,
                hovered.l
            );
            assert!(
                radio_hovered.l < radio_resting.l,
                "on a light theme the hovered radio outline must be darker: {} -> {}",
                radio_resting.l,
                radio_hovered.l
            );
        }
    }

    struct CountingHost {
        checkbox_clicks: Rc<Cell<usize>>,
        radio_clicks: Rc<Cell<usize>>,
    }

    impl Render for CountingHost {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            let checkbox_clicks = self.checkbox_clicks.clone();
            let radio_clicks = self.radio_clicks.clone();
            v_flex()
                .child(
                    Checkbox::new("click-probe", ToggleState::Unselected)
                        .tab_index(0_isize)
                        .on_click(move |_state, _window, _cx| {
                            checkbox_clicks.set(checkbox_clicks.get() + 1);
                        }),
                )
                .child(
                    RadioButton::new("click-probe-radio", false)
                        .tab_index(1_isize)
                        .on_click(move |_event, _window, _cx| {
                            radio_clicks.set(radio_clicks.get() + 1);
                        }),
                )
        }
    }

    // A toggle that is both focusable and clickable carries two click listeners:
    // one on the focusable indicator, for Enter and Space, and one on the row, so
    // that the label is clickable too. A pointer press on the indicator bubbles
    // through both, so without a guard one press counts as two.
    #[gpui::test]
    async fn one_pointer_click_counts_once(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let checkbox_clicks = Rc::new(Cell::new(0));
        let radio_clicks = Rc::new(Cell::new(0));
        let (_host, cx) = cx.add_window_view({
            let checkbox_clicks = checkbox_clicks.clone();
            let radio_clicks = radio_clicks.clone();
            move |_window, _cx| CountingHost {
                checkbox_clicks,
                radio_clicks,
            }
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            let _ = window.draw(cx);
        });
        cx.run_until_parked();

        let checkbox = cx
            .debug_bounds("CHECKBOX-click-probe")
            .expect("the checkbox is painted");
        cx.simulate_click(checkbox.center(), Modifiers::default());
        cx.run_until_parked();

        let radio = cx
            .debug_bounds("RADIO-click-probe-radio")
            .expect("the radio button is painted");
        cx.simulate_click(radio.center(), Modifiers::default());
        cx.run_until_parked();

        assert_eq!(
            checkbox_clicks.get(),
            1,
            "one press on the checkbox must call the handler once"
        );
        assert_eq!(
            radio_clicks.get(),
            1,
            "one press on the radio must call the handler once"
        );
    }
}
