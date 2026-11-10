//! Renders sample text, icons and shapes to a PNG: `cargo run --example gfx_preview -- out.png`
use notify_rs::gfx::canvas::{Canvas, Color};
use notify_rs::gfx::icons::{self, Icon};
use notify_rs::gfx::png_write::canvas_png;
use notify_rs::gfx::text::{Fonts, Style};

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "preview.png".into());
    let mut fonts = Fonts::system().expect("fonts");
    eprintln!("fonts: {:?}", fonts.describe());
    let mut cv = Canvas::new(520, 400);
    cv.clear(Color::rgb(0x1b, 0x1b, 0x1d));
    cv.fill_rrect(20.0, 20.0, 480.0, 110.0, 14.0, Color::rgb(0x2a, 0x2a, 0x2e));
    cv.stroke_rrect(20.0, 20.0, 480.0, 110.0, 14.0, 1.0, Color::rgba(255, 255, 255, 30));
    cv.fill_rrect(34.0, 34.0, 56.0, 56.0, 10.0, Color::rgb(0x44, 0x44, 0x4a));
    icons::draw(&mut cv, Icon::Note, 62.0, 62.0, 30.0, Color::rgb(0xa0, 0xa0, 0xa8));
    fonts.draw(&mut cv, Style::Bold, 104.0, 52.0, "夜に駆ける — YOASOBI", 15.0, Color::rgb(0xf2, 0xf2, 0xf4));
    fonts.draw(&mut cv, Style::Regular, 104.0, 74.0, "Now playing · THE BOOK · 4:19", 13.0, Color::rgb(0xb4, 0xb4, 0xbc));
    let lines = fonts.wrap(Style::Regular, "沈んだ夕日の輪郭を辿るように、a long line that must wrap properly 日本語もここで折り返す。", 13.0, 380.0, 3);
    for (i, l) in lines.iter().enumerate() {
        fonts.draw(&mut cv, Style::Regular, 104.0, 96.0 + 18.0 * i as f32, l, 13.0, Color::rgb(0xb4, 0xb4, 0xbc));
    }
    // icon row
    let icons_row = [Icon::Prev, Icon::Play, Icon::Pause, Icon::Next, Icon::Volume, Icon::VolumeLow, Icon::Mute, Icon::Bell, Icon::BellOff, Icon::Close, Icon::Trash, Icon::Note];
    for (i, ic) in icons_row.iter().enumerate() {
        icons::draw(&mut cv, *ic, 40.0 + 40.0 * i as f32, 170.0, 24.0, Color::rgb(0xd0, 0xd0, 0xd4));
    }
    // sizes
    for (i, px) in [10.0, 11.0, 12.0, 13.0, 14.0, 16.0, 20.0, 28.0].iter().enumerate() {
        fonts.draw(&mut cv, Style::Regular, 24.0, 215.0 + 24.0 * i as f32, &format!("{px}px Hamburgefonstiv 0123456789 日本語 かな"), *px, Color::rgb(0xe0, 0xe0, 0xe4));
    }
    // slider look
    cv.fill_rrect(24.0, 380.0, 200.0, 4.0, 2.0, Color::rgb(0x3a, 0x3a, 0x3a));
    cv.fill_rrect(24.0, 380.0, 120.0, 4.0, 2.0, Color::rgb(0x8a, 0x8a, 0x8a));
    cv.fill_rrect(138.0, 376.0, 12.0, 12.0, 6.0, Color::rgb(0xd0, 0xd0, 0xd0));
    std::fs::write(&out, canvas_png(&cv, Some(Color::rgb(0, 0, 0)))).unwrap();
    eprintln!("wrote {out}");
}
