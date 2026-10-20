// Every test file that includes this module uses some of these, not all.
#![allow(dead_code)]

use html_render::HtmlPage;

pub fn is_green([red, green, blue]: [u8; 3]) -> bool {
    red < 40 && green > 215 && blue < 40
}

pub fn is_red([red, green, blue]: [u8; 3]) -> bool {
    red > 215 && green < 40 && blue < 40
}

pub fn is_blue([red, green, blue]: [u8; 3]) -> bool {
    red < 40 && green < 40 && blue > 215
}

pub fn is_cyan([red, green, blue]: [u8; 3]) -> bool {
    red < 40 && green > 215 && blue > 215
}

pub fn is_magenta([red, green, blue]: [u8; 3]) -> bool {
    red > 215 && green < 40 && blue > 215
}

pub fn is_yellow([red, green, blue]: [u8; 3]) -> bool {
    red > 215 && green > 215 && blue < 40
}

pub fn is_black([red, green, blue]: [u8; 3]) -> bool {
    red < 40 && green < 40 && blue < 40
}

/// How many pixels of a band of the page, `row_height` tall, satisfy `wanted`.
pub fn pixels_in_row(
    page: &HtmlPage,
    row: usize,
    row_height: usize,
    wanted: impl Fn([u8; 3]) -> bool,
) -> usize {
    let Some(frame) = page.frame() else {
        return 0;
    };
    let width = u32::from(frame.size(0).width) as usize;
    let Some(bytes) = frame.as_bytes(0) else {
        return 0;
    };
    let first = row * row_height * width * 4;
    let last = (row + 1) * row_height * width * 4;
    let Some(pixels) = bytes.get(first..last) else {
        return 0;
    };
    pixels
        .chunks_exact(4)
        // The frames are premultiplied BGRA, which is what gpui draws.
        .filter(|pixel| wanted([pixel[2], pixel[1], pixel[0]]))
        .count()
}
