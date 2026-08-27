use gpui::{App, AppContext as _, Bounds, Pixels, Size, WindowBounds, point, px, size};
use serde::{Deserialize, Serialize};
use util::ResultExt as _;

/// A window of the fork's own -- the run configurations, a connection form --
/// opens where the reader left it, and at the size the reader gave it. Both
/// survive a restart, because a window that has to be resized on every launch is
/// a window that is never the right size.
#[derive(Serialize, Deserialize)]
struct Placement {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    display: String,
}

fn storage_key(window: &str) -> String {
    format!("window-placement:{window}")
}

/// The size a window opens at the first time, before the reader has given it
/// one: nearly the whole screen. These are forms with two columns and a dozen
/// fields, and opening them small means opening them wrong -- the reader would
/// resize every one of them before reading anything.
///
/// The floor is there for a small screen, where nearly the whole screen is
/// already less than the form needs; the display's own size is the ceiling, so
/// nothing opens off-screen.
pub fn opening_size(cx: &App) -> Size<Pixels> {
    let Some(screen) = cx.primary_display().map(|display| display.bounds().size) else {
        return size(px(1200.), px(820.));
    };
    let generous = size(screen.width * 0.9, screen.height * 0.9);
    size(
        generous.width.max(px(900.)).min(screen.width),
        generous.height.max(px(620.)).min(screen.height),
    )
}

/// Where the named window should open: exactly where it was left, if that screen
/// is still there and still holds it, and otherwise the middle of the screen at
/// [`opening_size`].
pub fn where_to_open(window: &str, cx: &mut App) -> WindowBounds {
    let left = read(window, cx).and_then(|placement| {
        let bounds = Bounds {
            origin: point(px(placement.x), px(placement.y)),
            size: size(px(placement.width), px(placement.height)),
        };
        let wanted = placement.display;
        let screen = cx.displays().into_iter().find(|display| {
            display.uuid().ok().map(|it| it.to_string()).as_deref() == Some(wanted.as_str())
        })?;
        screen.bounds().intersects(&bounds).then_some(bounds)
    });

    match left {
        Some(bounds) => WindowBounds::Windowed(bounds),
        None => WindowBounds::centered(opening_size(cx), cx),
    }
}

/// Remembers where and how large the reader left the named window.
pub fn remember(window: &str, bounds: Bounds<Pixels>, display: String, cx: &mut App) {
    let placement = Placement {
        x: bounds.origin.x.into(),
        y: bounds.origin.y.into(),
        width: bounds.size.width.into(),
        height: bounds.size.height.into(),
        display,
    };
    let Some(json) = serde_json::to_string(&placement).log_err() else {
        return;
    };
    let key = storage_key(window);
    let store = db::kvp::KeyValueStore::global(cx);
    cx.background_spawn(async move {
        store.write_kvp(key, json).await.log_err();
    })
    .detach();
}

fn read(window: &str, cx: &App) -> Option<Placement> {
    let json = db::kvp::KeyValueStore::global(cx)
        .read_kvp(&storage_key(window))
        .log_err()
        .flatten()?;
    serde_json::from_str(&json).log_err()
}
