use db_client::DatabaseDriver;
use gpui::{Hsla, StyledImage, img, rgb};
use ui::{Color, Icon, IconName, IconSize, prelude::*};

/// Bundled asset path of the full-color brand logo SVG for a driver.
///
/// Returned as `&'static str` so this can be called from `render` without
/// allocating. The logo is rasterized in full color via `img()`; the monochrome
/// `svg()` icon path would flatten the brand colors into a single tint.
pub(crate) fn brand_icon_path(driver: DatabaseDriver) -> &'static str {
    match driver {
        DatabaseDriver::MySQL => "icons/db_brands/mysql.svg",
        DatabaseDriver::PostgreSQL => "icons/db_brands/postgresql.svg",
        DatabaseDriver::SQLite => "icons/db_brands/sqlite.svg",
        DatabaseDriver::ClickHouse => "icons/db_brands/clickhouse.svg",
        DatabaseDriver::Redis => "icons/db_brands/redis.svg",
        // No bundled asset yet (a real logo needs a verified CC0/CC-BY
        // source); `brand_icon`'s `with_fallback` renders the generic glyph
        // tinted with `brand_color` below until one is added.
        DatabaseDriver::MongoDB => "icons/db_brands/mongodb.svg",
    }
}

/// Signature brand color of a driver, used only to tint the fallback glyph when
/// the brand logo asset cannot be loaded.
pub(crate) fn brand_color(driver: DatabaseDriver) -> Hsla {
    let hex = match driver {
        DatabaseDriver::MySQL => 0x00618A,
        DatabaseDriver::PostgreSQL => 0x336791,
        DatabaseDriver::SQLite => 0x0B7FCC,
        DatabaseDriver::ClickHouse => 0xF9FF69,
        DatabaseDriver::Redis => 0xD82C20,
        DatabaseDriver::MongoDB => 0x47A248,
    };
    rgb(hex).into()
}

/// Full-color brand logo element for a driver. Falls back to the database glyph
/// tinted with the driver's brand color if the asset cannot be loaded, so the
/// row never shows a blank or broken icon.
pub(crate) fn brand_icon(driver: DatabaseDriver, size: IconSize) -> impl IntoElement {
    let rems = size.rems();
    img(brand_icon_path(driver))
        .size(rems)
        .flex_none()
        .with_fallback(move || {
            Icon::new(IconName::DatabaseZap)
                .size(size)
                .color(Color::Custom(brand_color(driver)))
                .into_any_element()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_icon_path_is_unique_and_bundled_per_driver() {
        let drivers = [
            DatabaseDriver::MySQL,
            DatabaseDriver::PostgreSQL,
            DatabaseDriver::SQLite,
            DatabaseDriver::ClickHouse,
            DatabaseDriver::Redis,
            DatabaseDriver::MongoDB,
        ];

        let mut seen = std::collections::HashSet::new();
        for driver in drivers {
            let path = brand_icon_path(driver);
            assert!(
                path.starts_with("icons/db_brands/") && path.ends_with(".svg"),
                "unexpected asset path for {driver}: {path}"
            );
            assert!(
                seen.insert(path),
                "duplicate asset path for {driver}: {path}"
            );
        }
    }

    #[test]
    fn brand_color_matches_signature_values() {
        let cases = [
            (DatabaseDriver::MySQL, 0x00618A),
            (DatabaseDriver::PostgreSQL, 0x336791),
            (DatabaseDriver::SQLite, 0x0B7FCC),
            (DatabaseDriver::ClickHouse, 0xF9FF69),
            (DatabaseDriver::Redis, 0xD82C20),
            (DatabaseDriver::MongoDB, 0x47A248),
        ];
        for (driver, hex) in cases {
            assert_eq!(
                brand_color(driver),
                Hsla::from(rgb(hex)),
                "unexpected brand color for {driver}"
            );
        }
    }
}
