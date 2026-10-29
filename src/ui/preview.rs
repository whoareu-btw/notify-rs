//! `notify-rs preview out.png`: the panel (or toasts) with sample content, no
//! compositor needed. Handy for checking fonts and scaling.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Instant;

use super::paint::Painter;
use super::panel::{self, Art, Frame, MediaView, PanelState};
use crate::gfx::canvas::{Canvas, Color};
use crate::gfx::png_write::canvas_png;
use crate::gfx::text::Fonts;
use crate::notify::{Notification, Toast, Urgency};

pub const HELP: &str = "    notify-rs preview OUT.png [--scale N] [--empty] [--no-media] [--toasts] [--hover ID]
                                   draw the panel with sample content to a PNG
    notify-rs fonts                which font files are used, and which face draws what
";

/// `notify-rs fonts`: the font chain and what a few characters resolve to.
pub fn fonts() -> ExitCode {
    let (mut fonts, report) = match Fonts::system_with_report() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("notify-rs: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("fonts, in order of preference (the first one that has a character draws it):");
    for line in &report {
        println!("  {line}");
    }
    println!("\nwho draws what:");
    let samples = [
        ('A', "Latin"),
        ('1', "digit"),
        ('あ', "hiragana"),
        ('カ', "katakana"),
        ('透', "kanji"),
        ('한', "hangul"),
        ('\u{f009a}', "bell icon"),
        ('\u{f116b}', "bell with badge icon"),
        ('\u{f009b}', "bell off icon"),
    ];
    for style in [crate::gfx::text::Style::Regular, crate::gfx::text::Style::Bold] {
        for (ch, what) in samples {
            let who = fonts.which(style, ch).unwrap_or_else(|| "NOTHING: this character will not be drawn".into());
            println!("  {:<7} {ch:?} {what:<22} {who}", format!("{style:?}").to_lowercase());
        }
    }
    ExitCode::SUCCESS
}

pub fn sample(id: u32, app: &str, summary: &str, body: &str, secs_ago: i64) -> Notification {
    Notification {
        id,
        app_name: app.into(),
        app_icon: String::new(),
        summary: summary.into(),
        body: body.into(),
        actions: vec![],
        urgency: Urgency::Normal,
        category: String::new(),
        desktop_entry: String::new(),
        image: None,
        image_path: None,
        resident: false,
        transient: false,
        progress: None,
        sync_tag: None,
        timeout: None,
        created_unix: crate::service::now_unix() - secs_ago,
        toast: Toast::Hidden,
    }
}

pub fn sample_items() -> Vec<Notification> {
    let mut v = vec![
        {
            let mut n = sample(5, "Signal", "Mika", "夜ご飯、何にする？ I was thinking ramen near the station, but the one on the corner has a queue again.", 40);
            n.actions = vec![("default".into(), "Open".into()), ("reply".into(), "Reply".into()), ("mark".into(), "Mark as read".into())];
            n
        },
        {
            let mut n = sample(4, "Updates", "Download finished", "nixos-unstable-small.iso", 7 * 60);
            n.progress = Some(100);
            n
        },
        {
            let mut n = sample(3, "Battery", "Battery low", "12% remaining. Plug in the charger.", 3 * 3600);
            n.urgency = Urgency::Critical;
            n
        },
        sample(2, "calendar", "Standup in 10 minutes", "", 26 * 3600),
        sample(1, "Firefox", "A very long notification title that cannot possibly fit on a single line of this card", "short body", 3 * 86400),
    ];
    v.sort_by_key(|n| std::cmp::Reverse(n.id));
    v
}

pub fn sample_media(fonts_scale: f32) -> MediaView {
    let dev = (76.0 * fonts_scale).round() as usize;
    let mut px = vec![0u8; dev * dev * 4];
    for y in 0..dev {
        for x in 0..dev {
            let (fx, fy) = (x as f32 / dev as f32, y as f32 / dev as f32);
            let i = (y * dev + x) * 4;
            px[i] = (60.0 + 150.0 * fx) as u8;
            px[i + 1] = (40.0 + 90.0 * fy) as u8;
            px[i + 2] = (120.0 + 100.0 * (1.0 - fx)) as u8;
            px[i + 3] = 255;
        }
    }
    MediaView {
        identity: "LibreWolf".into(),
        playing: true,
        title: "夜に駆ける".into(),
        artist: "YOASOBI".into(),
        album: "THE BOOK".into(),
        position_us: 83_000_000,
        length_us: 259_000_000,
        volume: 0.64,
        can_seek: true,
        art: Some(Art { dev, px: Arc::new(px) }),
    }
}

pub fn render_panel(fonts: &mut Fonts, st: &PanelState, scale: f32) -> (Canvas, Frame) {
    let measure = {
        let mut p = Painter::new(None, scale, fonts);
        panel::draw_panel(&mut p, st)
    };
    let mut cv = Canvas::new((measure.w * scale).ceil() as usize, (measure.h * scale).ceil() as usize);
    let f = {
        let mut p = Painter::new(Some(&mut cv), scale, fonts);
        panel::draw_panel(&mut p, st)
    };
    (cv, f)
}

pub fn run(args: &[String]) -> ExitCode {
    let Some(out) = args.get(1).filter(|a| !a.starts_with("--")) else {
        eprintln!("notify-rs: preview needs an output file, e.g. `notify-rs preview panel.png`");
        return ExitCode::from(2);
    };
    let has = |f: &str| args.iter().any(|a| a == f);
    let value = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1));
    let scale: f32 = value("--scale").and_then(|v| v.parse().ok()).unwrap_or(1.0);
    let mut fonts = match Fonts::system() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("notify-rs: {e}");
            return ExitCode::FAILURE;
        }
    };
    let items = if has("--empty") { Vec::new() } else { sample_items() };
    let refs: Vec<&Notification> = items.iter().collect();
    let media = sample_media(scale);
    let images = |_: &Notification, _: usize| None;
    let hover = match value("--hover").map(String::as_str) {
        Some("play") => Some(panel::HitId::PlayPause),
        Some("seek") => Some(panel::HitId::Seek),
        Some("dnd") => Some(panel::HitId::Dnd),
        Some("item") => Some(panel::HitId::Item(5)),
        Some("close") => Some(panel::HitId::Close(5)),
        Some("action") => Some(panel::HitId::Action(5, "reply".into())),
        _ => None,
    };
    let cv = if has("--toasts") {
        let toasts: Vec<&Notification> = refs.iter().take(3).copied().collect();
        let measure = panel::draw_toasts(&mut Painter::new(None, scale, &mut fonts), &toasts, crate::service::now_unix(), hover.as_ref(), None, &images);
        let mut cv = Canvas::new((measure.w * scale).ceil() as usize, (measure.h * scale).ceil() as usize);
        panel::draw_toasts(&mut Painter::new(Some(&mut cv), scale, &mut fonts), &toasts, crate::service::now_unix(), hover.as_ref(), None, &images);
        cv
    } else {
        let st = PanelState {
            media: if has("--no-media") { None } else { Some(&media) },
            items: &refs,
            dnd: has("--dnd"),
            now_unix: crate::service::now_unix(),
            hover: hover.as_ref(),
            pressed: None,
            drag: None,
            scroll: value("--scroll").and_then(|v| v.parse().ok()).unwrap_or(0.0),
            max_card_h: value("--max-height").and_then(|v| v.parse().ok()).unwrap_or(10_000.0),
            images: &images,
        };
        render_panel(&mut fonts, &st, scale).0
    };
    let t = Instant::now();
    let png = canvas_png(&cv, Some(Color::rgb(0x45, 0x4b, 0x5c)));
    match std::fs::write(out, png) {
        Ok(()) => {
            eprintln!("wrote {out} ({}x{}, {} ms to encode)", cv.w, cv.h, t.elapsed().as_millis());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("notify-rs: cannot write {out}: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything outside the card must stay fully transparent: no drop shadow.
    fn assert_no_shadow(cv: &Canvas, card: super::super::paint::R, scale: f32, what: &str) {
        let (x0, y0) = ((card.x * scale).floor() as usize, (card.y * scale).floor() as usize);
        let (x1, y1) = ((card.right() * scale).ceil() as usize, (card.bottom() * scale).ceil() as usize);
        let mut dirty = 0;
        for y in 0..cv.h {
            for x in 0..cv.w {
                let outside = x < x0 || x >= x1 || y < y0 || y >= y1;
                if outside && cv.pixel(x, y) >> 24 != 0 {
                    dirty += 1;
                }
            }
        }
        assert_eq!(dirty, 0, "{what} at scale {scale}: {dirty} pixels outside the card are not transparent");
        // and the card itself is drawn: its centre is opaque-ish
        let (cx, cy) = ((x0 + x1) / 2, (y0 + y1) / 2);
        assert!(cv.pixel(cx, cy) >> 24 > 200, "{what}: card body missing");
    }

    #[test]
    fn card_and_toasts_have_no_shadow() {
        let Ok(mut fonts) = Fonts::system() else { return };
        let items = sample_items();
        let refs: Vec<&Notification> = items.iter().collect();
        let media = sample_media(1.0);
        let images = |_: &Notification, _: usize| None;
        for scale in [1.0f32, 2.0] {
            let st = PanelState {
                media: Some(&media),
                items: &refs,
                dnd: false,
                now_unix: crate::service::now_unix(),
                hover: None,
                pressed: None,
                drag: None,
                scroll: 0.0,
                max_card_h: 10_000.0,
                images: &images,
            };
            let (cv, f) = render_panel(&mut fonts, &st, scale);
            assert_no_shadow(&cv, f.card.expect("card rect"), scale, "panel");

            let toasts: Vec<&Notification> = refs.iter().take(1).copied().collect();
            let m = panel::draw_toasts(&mut Painter::new(None, scale, &mut fonts), &toasts, crate::service::now_unix(), None, None, &images);
            let mut cv = Canvas::new((m.w * scale).ceil() as usize, (m.h * scale).ceil() as usize);
            let f = panel::draw_toasts(&mut Painter::new(Some(&mut cv), scale, &mut fonts), &toasts, crate::service::now_unix(), None, None, &images);
            assert_no_shadow(&cv, f.card.expect("toast rect"), scale, "toast");
        }
    }
}
