mod decorated_icon;
mod icon_decoration;

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use decorated_icon::*;
use gpui::{AnimationElement, AnyElement, Hsla, IntoElement, Rems, Transformation, img, svg};
pub use icon_decoration::*;
pub use icons::*;

use crate::traits::transformable::Transformable;
use crate::{Indicator, prelude::*};

#[derive(IntoElement)]
pub enum AnyIcon {
    Icon(Icon),
    AnimatedIcon(AnimationElement<Icon>),
}

impl AnyIcon {
    /// Returns a new [`AnyIcon`] after applying the given mapping function
    /// to the contained [`Icon`].
    pub fn map(self, f: impl FnOnce(Icon) -> Icon) -> Self {
        match self {
            Self::Icon(icon) => Self::Icon(f(icon)),
            Self::AnimatedIcon(animated_icon) => Self::AnimatedIcon(animated_icon.map_element(f)),
        }
    }
}

impl From<Icon> for AnyIcon {
    fn from(value: Icon) -> Self {
        Self::Icon(value)
    }
}

impl From<AnimationElement<Icon>> for AnyIcon {
    fn from(value: AnimationElement<Icon>) -> Self {
        Self::AnimatedIcon(value)
    }
}

impl RenderOnce for AnyIcon {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        match self {
            Self::Icon(icon) => icon.into_any_element(),
            Self::AnimatedIcon(animated_icon) => animated_icon.into_any_element(),
        }
    }
}

#[derive(Default, PartialEq, Copy, Clone)]
pub enum IconSize {
    /// 12px
    Indicator,
    /// 14px
    XSmall,
    /// 17px
    Small,
    #[default]
    /// 20px
    Medium,
    /// 24px
    Large,
    /// 48px
    XLarge,
    Custom(Rems),
}

impl IconSize {
    pub fn rems(self) -> Rems {
        match self {
            // The whole scale sits one step up from where it was: 14px was the
            // "normal" icon and 16 the largest before a jump to 48, which is the
            // metric of an early-2010s desktop. Raising the steps rather than the
            // call sites moves every icon in the editor at once.
            IconSize::Indicator => rems_from_px(12.),
            IconSize::XSmall => rems_from_px(14.),
            IconSize::Small => rems_from_px(17.),
            IconSize::Medium => rems_from_px(20.),
            IconSize::Large => rems_from_px(24.),
            IconSize::XLarge => rems_from_px(48.),
            IconSize::Custom(size) => size,
        }
    }

    /// Returns the individual components of the square that contains this [`IconSize`].
    ///
    /// The returned tuple contains:
    ///   1. The length of one side of the square
    ///   2. The padding of one side of the square
    pub fn square_components(&self, window: &mut Window, cx: &mut App) -> (Pixels, Pixels) {
        let icon_size = self.rems() * window.rem_size();
        let padding = match self {
            IconSize::Indicator => DynamicSpacing::Base00.px(cx),
            IconSize::XSmall => DynamicSpacing::Base02.px(cx),
            IconSize::Small => DynamicSpacing::Base02.px(cx),
            IconSize::Medium => DynamicSpacing::Base02.px(cx),
            IconSize::Large => DynamicSpacing::Base04.px(cx),
            IconSize::XLarge => DynamicSpacing::Base02.px(cx),
            // TODO: Wire into dynamic spacing
            IconSize::Custom(size) => size.to_pixels(window.rem_size()),
        };

        (icon_size, padding)
    }

    /// Returns the length of a side of the square that contains this [`IconSize`], with padding.
    pub fn square(&self, window: &mut Window, cx: &mut App) -> Pixels {
        let (icon_size, padding) = self.square_components(window, cx);

        icon_size + padding * 2.
    }
}

impl From<IconName> for Icon {
    fn from(icon: IconName) -> Self {
        Icon::new(icon)
    }
}

/// The source of an icon.
#[derive(Clone)]
enum IconSource {
    /// An SVG embedded in the Zed binary.
    Embedded(SharedString),
    /// An image file located at the specified path.
    ///
    /// Currently our SVG renderer is missing support for rendering polychrome SVGs.
    ///
    /// In order to support icon themes, we render the icons as images instead.
    External(Arc<Path>),
    /// An SVG not embedded in the Zed binary.
    ExternalSvg(SharedString),
}

#[derive(Clone, IntoElement, RegisterComponent)]
pub struct Icon {
    source: IconSource,
    color: Color,
    size: Rems,
    transformation: Transformation,
}

impl Icon {
    pub fn new(icon: IconName) -> Self {
        Self {
            source: IconSource::Embedded(icon.path().into()),
            color: Color::default(),
            size: IconSize::default().rems(),
            transformation: Transformation::default(),
        }
    }

    /// Create an icon from a path. Uses a heuristic to determine if it's embedded or external:
    /// - Paths starting with "icons/" are treated as embedded SVGs
    /// - Other paths are treated as external raster images (from icon themes)
    pub fn from_path(path: impl Into<SharedString>) -> Self {
        let path = path.into();
        let source = if path.starts_with("icons/") {
            IconSource::Embedded(path)
        } else {
            IconSource::External(Arc::from(PathBuf::from(path.as_ref())))
        };
        Self {
            source,
            color: Color::default(),
            size: IconSize::default().rems(),
            transformation: Transformation::default(),
        }
    }

    pub fn from_external_svg(svg: SharedString) -> Self {
        Self {
            source: IconSource::ExternalSvg(svg),
            color: Color::default(),
            size: IconSize::default().rems(),
            transformation: Transformation::default(),
        }
    }

    pub fn color(mut self, color: Color) -> Self {
        self.color = color;
        self
    }

    pub fn size(mut self, size: IconSize) -> Self {
        self.size = size.rems();
        self
    }

    /// Sets a custom size for the icon, in [`Rems`].
    ///
    /// Not to be exposed outside of the `ui` crate.
    pub(crate) fn custom_size(mut self, size: Rems) -> Self {
        self.size = size;
        self
    }
}

impl Transformable for Icon {
    fn transform(mut self, transformation: Transformation) -> Self {
        self.transformation = transformation;
        self
    }
}

impl RenderOnce for Icon {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        match self.source {
            IconSource::Embedded(path) => svg()
                .with_transformation(self.transformation)
                .size(self.size)
                .flex_none()
                .path(path)
                .text_color(self.color.color(cx))
                .into_any_element(),
            IconSource::ExternalSvg(path) => svg()
                .external_path(path)
                .with_transformation(self.transformation)
                .size(self.size)
                .flex_none()
                .text_color(self.color.color(cx))
                .into_any_element(),
            IconSource::External(path) => img(path)
                .size(self.size)
                .flex_none()
                .text_color(self.color.color(cx))
                .into_any_element(),
        }
    }
}

#[derive(IntoElement)]
pub struct IconWithIndicator {
    icon: Icon,
    indicator: Option<Indicator>,
    indicator_border_color: Option<Hsla>,
}

impl IconWithIndicator {
    pub fn new(icon: Icon, indicator: Option<Indicator>) -> Self {
        Self {
            icon,
            indicator,
            indicator_border_color: None,
        }
    }

    pub fn indicator(mut self, indicator: Option<Indicator>) -> Self {
        self.indicator = indicator;
        self
    }

    pub fn indicator_color(mut self, color: Color) -> Self {
        if let Some(indicator) = self.indicator.as_mut() {
            indicator.color = color;
        }
        self
    }

    pub fn indicator_border_color(mut self, color: Option<Hsla>) -> Self {
        self.indicator_border_color = color;
        self
    }
}

impl RenderOnce for IconWithIndicator {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let indicator_border_color = self
            .indicator_border_color
            .unwrap_or_else(|| cx.theme().colors().elevated_surface_background);

        div()
            .relative()
            .child(self.icon)
            .when_some(self.indicator, |this, indicator| {
                this.child(
                    div()
                        .absolute()
                        .size_2p5()
                        .border_2()
                        .border_color(indicator_border_color)
                        .rounded_full()
                        .bottom_neg_0p5()
                        .right_neg_0p5()
                        .child(indicator),
                )
            })
    }
}

impl Component for Icon {
    fn scope() -> ComponentScope {
        ComponentScope::Images
    }

    fn description() -> &'static str {
        "A versatile icon component that supports SVG and image-based icons \
        with customizable size, color, and transformations."
    }

    fn preview(_window: &mut Window, cx: &mut App) -> AnyElement {
        v_flex()
            .gap_6()
            .children(vec![
                example_group_with_title(
                    "Sizes",
                    vec![single_example(
                        "XSmall, Small, Default, Large",
                        h_flex()
                            .gap_1()
                            .child(Icon::new(IconName::Star).size(IconSize::XSmall))
                            .child(Icon::new(IconName::Star).size(IconSize::Small))
                            .child(Icon::new(IconName::Star))
                            .child(Icon::new(IconName::Star).size(IconSize::XLarge))
                            .into_any_element(),
                    )],
                ),
                example_group(vec![single_example(
                    "All Icons",
                    h_flex()
                        .image_cache(gpui::retain_all("all icons"))
                        .flex_wrap()
                        .gap_2()
                        .children(<IconName as strum::IntoEnumIterator>::iter().map(
                            |icon_name: IconName| {
                                let name: SharedString = format!("{icon_name:?}").into();
                                v_flex()
                                    .min_w_0()
                                    .w_24()
                                    .p_1p5()
                                    .gap_2()
                                    .border_1()
                                    .border_color(cx.theme().colors().border_variant)
                                    .bg(cx.theme().colors().element_disabled)
                                    .rounded_sm()
                                    .items_center()
                                    .child(Icon::new(icon_name))
                                    .child(Label::new(name).size(LabelSize::XSmall).truncate())
                            },
                        ))
                        .into_any_element(),
                )]),
            ])
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::IconSize;
    use crate::rems_from_px;
    use gpui::{TestAppContext, px};

    // The scale used to run 10, 12, 14, 16, 48: anything that needed a normal
    // icon got 14 or 16, and anything larger had to jump straight to 48. Both
    // problems are fixed at once -- a step was added and the whole scale moved
    // up, because 14px was the "normal" icon and that is what dated the chrome.
    #[test]
    fn the_scale_is_not_stuck_in_the_early_twenty_tens() {
        assert!(
            IconSize::Medium.rems().0 >= rems_from_px(20.).0,
            "the default icon is what most call sites get: {:?}",
            IconSize::Medium.rems()
        );
        assert!(
            IconSize::Small.rems().0 >= rems_from_px(16.).0,
            "even the small icon has to stay legible: {:?}",
            IconSize::Small.rems()
        );
        let steps = [
            IconSize::Indicator.rems().0,
            IconSize::XSmall.rems().0,
            IconSize::Small.rems().0,
            IconSize::Medium.rems().0,
            IconSize::Large.rems().0,
            IconSize::XLarge.rems().0,
        ];
        assert!(
            steps.windows(2).all(|pair| pair[0] < pair[1]),
            "the scale has to stay ordered: {steps:?}"
        );
    }

    // A square icon button is its icon *plus twice the padding*, so a container
    // sized by eye at 16px clips it -- and where the parent hides overflow, the
    // clipped strip takes part of the hitbox with it. That is exactly what
    // happened to the close button in the system window tabs when the icon scale
    // moved. Containers have to ask `square()` rather than assume a number.
    #[gpui::test]
    async fn a_square_icon_button_no_longer_fits_a_sixteen_pixel_box(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| SizeHost);

        let (extra_small, small) = cx.update(|window, cx| {
            (
                IconSize::XSmall.square(window, cx),
                IconSize::Small.square(window, cx),
            )
        });

        assert!(
            extra_small > px(16.),
            "an XSmall square button is larger than the 16px boxes that used to hold it: {extra_small:?}"
        );
        assert!(
            small > extra_small,
            "the squares stay ordered with the icon scale"
        );
    }

    struct SizeHost;

    impl gpui::Render for SizeHost {
        fn render(
            &mut self,
            _window: &mut gpui::Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            gpui::div()
        }
    }
}
