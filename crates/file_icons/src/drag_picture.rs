use std::path::Path;
use std::sync::Arc;

use gpui::{App, DevicePixels, RenderImage, Size, size};
use theme::ActiveTheme;

use crate::FileIcons;

/// How wide and tall the picture is, in its own pixels. It is carried at half
/// that, so it stays sharp on a screen that doubles everything.
const SIDE: i32 = 128;
const SCALE: i32 = 2;

/// Where the card sits inside the picture, and how round its corners are.
const CARD: (i32, i32, i32, i32) = (22, 14, 114, 106);
const RADIUS: i32 = 14;

/// How far the card behind is offset, when more than one file is carried.
const BEHIND: (i32, i32) = (-12, 12);

/// How big the icon is drawn inside the card.
const GLYPH: i32 = 52;

/// The picture the desktop carries under the pointer while these files are
/// dragged out of the window: the file's own icon on a card, with a second card
/// behind it when more than one file is being carried.
///
/// The icon is the one the project panel shows for that file, so what is under
/// the pointer is what the reader picked up rather than a page that stands for
/// any file at all. Nothing comes back when the icon cannot be drawn, and the
/// desktop then draws whatever it draws for a drag it was told nothing about.
pub fn drag_picture(paths: &[impl AsRef<Path>], cx: &App) -> Option<Arc<RenderImage>> {
    let first = paths.first()?.as_ref();
    let icon = FileIcons::get_icon(first, cx)?;
    let glyph = cx
        .svg_renderer()
        .render_shape(&icon, size(DevicePixels(GLYPH), DevicePixels(GLYPH)))
        .ok()
        .flatten();
    let (glyph_size, coverage) = glyph?;

    let colors = cx.theme().colors();
    let paint = Paint {
        card: premultiplied(colors.elevated_surface_background, 1.),
        behind: premultiplied(colors.elevated_surface_background, 0.85),
        edge: premultiplied(colors.border, 1.),
        ink: premultiplied(colors.text, 1.),
    };

    let bytes = drawn(glyph_size, &coverage, paint, paths.len())
        .into_iter()
        .flat_map(|pixel| pixel.to_ne_bytes())
        .collect::<Vec<_>>();
    let buffer = image::ImageBuffer::from_raw(SIDE as u32, SIDE as u32, bytes)?;
    let picture = RenderImage::new([image::Frame::new(buffer)]).drawn_at_scale(SCALE as f32);
    Some(Arc::new(picture))
}

/// The four colours the picture is drawn in, each already in the byte order the
/// platforms want.
#[derive(Clone, Copy)]
struct Paint {
    card: u32,
    behind: u32,
    edge: u32,
    ink: u32,
}

/// The picture itself: a card with the icon on it, and a second card behind
/// when more than one file is carried.
///
/// Kept apart from where the icon comes from, so that what is drawn can be
/// looked at without an editor around it.
fn drawn(glyph_size: Size<DevicePixels>, coverage: &[u8], paint: Paint, count: usize) -> Vec<u32> {
    let mut pixels = vec![0u32; (SIDE * SIDE) as usize];
    if count > 1 {
        fill_card(
            &mut pixels,
            (
                CARD.0 + BEHIND.0,
                CARD.1 + BEHIND.1,
                CARD.2 + BEHIND.0,
                CARD.3 + BEHIND.1,
            ),
            paint.behind,
            paint.edge,
        );
    }
    fill_card(&mut pixels, CARD, paint.card, paint.edge);

    let left = CARD.0 + (CARD.2 - CARD.0 - glyph_size.width.0) / 2;
    let top = CARD.1 + (CARD.3 - CARD.1 - glyph_size.height.0) / 2;
    draw_shape(&mut pixels, coverage, glyph_size, left, top, paint.ink);
    pixels
}

/// A colour as the byte order the platforms want it in: blue first, alpha last,
/// every channel already multiplied by that alpha.
fn premultiplied(color: gpui::Hsla, opacity: f32) -> u32 {
    let rgba = gpui::Rgba::from(color);
    let alpha = (rgba.a * opacity).clamp(0., 1.);
    let channel = |value: f32| (value.clamp(0., 1.) * alpha * 255.).round() as u32;
    (((alpha * 255.).round() as u32) << 24)
        | (channel(rgba.r) << 16)
        | (channel(rgba.g) << 8)
        | channel(rgba.b)
}

/// One pixel of `over` laid on `under`, both premultiplied.
fn over(under: u32, over: u32) -> u32 {
    let alpha = (over >> 24) & 0xff;
    if alpha == 255 {
        return over;
    }
    if alpha == 0 {
        return under;
    }
    let keep = 255 - alpha;
    let channel = |shift: u32| {
        let a = (over >> shift) & 0xff;
        let b = (under >> shift) & 0xff;
        ((a + b * keep / 255) & 0xff) << shift
    };
    channel(24) | channel(16) | channel(8) | channel(0)
}

/// A card with rounded corners, filled and outlined.
fn fill_card(pixels: &mut [u32], card: (i32, i32, i32, i32), fill: u32, edge: u32) {
    let (left, top, right, bottom) = card;
    for y in top.max(0)..bottom.min(SIDE) {
        for x in left.max(0)..right.min(SIDE) {
            let Some(distance) = corner_distance(x, y, card) else {
                continue;
            };
            let at = (y * SIDE + x) as usize;
            // Two pixels in from the edge is the outline; everything inside it
            // is the card. A corner is outside when it is further from the
            // corner's own centre than the radius.
            let on_edge = distance > (RADIUS - 2) as f32
                || x < left + 2
                || x >= right - 2
                || y < top + 2
                || y >= bottom - 2;
            pixels[at] = over(pixels[at], if on_edge { edge } else { fill });
        }
    }
}

/// How far a pixel is from the centre of the corner it belongs to, or nothing
/// when it falls outside the rounded corner entirely.
fn corner_distance(x: i32, y: i32, card: (i32, i32, i32, i32)) -> Option<f32> {
    let (left, top, right, bottom) = card;
    let centre_x = match (x < left + RADIUS, x >= right - RADIUS) {
        (true, _) => left + RADIUS,
        (_, true) => right - RADIUS - 1,
        _ => return Some(0.),
    };
    let centre_y = match (y < top + RADIUS, y >= bottom - RADIUS) {
        (true, _) => top + RADIUS,
        (_, true) => bottom - RADIUS - 1,
        _ => return Some(0.),
    };
    let dx = (x - centre_x) as f32;
    let dy = (y - centre_y) as f32;
    let distance = (dx * dx + dy * dy).sqrt();
    (distance <= RADIUS as f32).then_some(distance)
}

/// The icon's shape, drawn in `ink`, at `left`/`top`.
fn draw_shape(
    pixels: &mut [u32],
    coverage: &[u8],
    shape: Size<DevicePixels>,
    left: i32,
    top: i32,
    ink: u32,
) {
    for y in 0..shape.height.0 {
        for x in 0..shape.width.0 {
            let covered = coverage[(y * shape.width.0 + x) as usize] as u32;
            if covered == 0 {
                continue;
            }
            let (at_x, at_y) = (left + x, top + y);
            if !(0..SIDE).contains(&at_x) || !(0..SIDE).contains(&at_y) {
                continue;
            }
            let scaled = |shift: u32| (((ink >> shift) & 0xff) * covered / 255) << shift;
            let tinted = scaled(24) | scaled(16) | scaled(8) | scaled(0);
            let at = (at_y * SIDE + at_x) as usize;
            pixels[at] = over(pixels[at], tinted);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAINT: Paint = Paint {
        card: 0xff20_2430,
        behind: 0xd918_1c26,
        edge: 0xff3a_4152,
        ink: 0xffd8_dee9,
    };

    /// A glyph of `side` pixels, `covered` of them at full strength down the
    /// middle: enough to tell one shape from another.
    fn a_glyph(side: i32, covered: i32) -> (Size<DevicePixels>, Vec<u8>) {
        let mut coverage = vec![0u8; (side * side) as usize];
        for y in 0..side {
            for x in 0..covered.min(side) {
                coverage[(y * side + x) as usize] = 255;
            }
        }
        (size(DevicePixels(side), DevicePixels(side)), coverage)
    }

    /// What the reader sees is the file's own icon. Two files the editor draws
    /// differently have to be carried differently, or the picture says nothing
    /// about what is being carried.
    #[test]
    fn two_shapes_are_carried_differently() {
        let (size_a, thin) = a_glyph(GLYPH, 8);
        let (size_b, wide) = a_glyph(GLYPH, 40);

        let one = drawn(size_a, &thin, PAINT, 1);
        let other = drawn(size_b, &wide, PAINT, 1);

        assert_eq!(one.len(), (SIDE * SIDE) as usize);
        assert_ne!(one, other, "two icons are not drawn as the same picture");
    }

    /// Carrying several files says so: a second card stands behind the first.
    #[test]
    fn several_files_are_carried_as_a_stack() {
        let (glyph_size, coverage) = a_glyph(GLYPH, 20);

        let one = drawn(glyph_size, &coverage, PAINT, 1);
        let several = drawn(glyph_size, &coverage, PAINT, 3);

        assert_ne!(one, several, "a stack of files does not look like one file");
        // The card behind stands to the left of the one in front. Halfway down,
        // where neither card is rounded, that strip belongs to the second card
        // and to nothing else.
        let beside = (SIDE / 2 * SIDE + CARD.0 + BEHIND.0 + 4) as usize;
        assert_eq!(one[beside], 0, "nothing is there when one file is carried");
        assert_ne!(several[beside], 0, "and something is when several are");
    }

    /// Nothing is drawn outside the picture, whatever size the icon comes back.
    #[test]
    fn an_icon_larger_than_its_card_is_not_drawn_past_the_edge() {
        let (glyph_size, coverage) = a_glyph(SIDE * 2, SIDE * 2);

        let pixels = drawn(glyph_size, &coverage, PAINT, 1);

        assert_eq!(pixels.len(), (SIDE * SIDE) as usize);
    }
}
