//! Vector icons, drawn with the same rasteriser as everything else so they are
//! crisp at any size and need no icon font.

use super::canvas::{Canvas, Color};
use super::raster::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Play,
    Pause,
    Prev,
    Next,
    Volume,
    VolumeLow,
    Mute,
    Bell,
    BellOff,
    Close,
    Trash,
    Note,
}

const PI: f32 = std::f32::consts::PI;

/// Pieces of the icon in a unit box centred on the origin (y down).
pub fn pieces(icon: Icon) -> Vec<Path> {
    let rr = Path::rounded_rect;
    match icon {
        Icon::Play => vec![Path::polygon(&[(-0.27, -0.40), (0.42, 0.0), (-0.27, 0.40)])],
        Icon::Pause => vec![rr(-0.30, -0.38, 0.22, 0.76, 0.05), rr(0.08, -0.38, 0.22, 0.76, 0.05)],
        Icon::Prev => vec![rr(-0.40, -0.36, 0.10, 0.72, 0.03), Path::polygon(&[(0.40, -0.38), (-0.20, 0.0), (0.40, 0.38)])],
        Icon::Next => vec![rr(0.30, -0.36, 0.10, 0.72, 0.03), Path::polygon(&[(-0.40, -0.38), (0.20, 0.0), (-0.40, 0.38)])],
        Icon::Volume | Icon::VolumeLow | Icon::Mute => {
            let mut v = vec![Path::polygon(&[(-0.42, -0.13), (-0.24, -0.13), (0.0, -0.34), (0.0, 0.34), (-0.24, 0.13), (-0.42, 0.13)])];
            match icon {
                Icon::Volume => {
                    v.push(Path::arc_stroke(0.0, 0.0, 0.20, -0.75, 0.75, 0.075));
                    v.push(Path::arc_stroke(0.0, 0.0, 0.36, -0.75, 0.75, 0.075));
                }
                Icon::VolumeLow => v.push(Path::arc_stroke(0.0, 0.0, 0.20, -0.75, 0.75, 0.075)),
                _ => {
                    v.push(Path::line_stroke(0.14, -0.14, 0.40, 0.14, 0.08));
                    v.push(Path::line_stroke(0.14, 0.14, 0.40, -0.14, 0.08));
                }
            }
            v
        }
        Icon::Bell | Icon::BellOff => {
            let mut dome = Path::new();
            dome.move_to(-0.38, 0.22)
                .cubic_to(-0.22, 0.14, -0.26, -0.04, -0.26, -0.12)
                .cubic_to(-0.26, -0.28, -0.14, -0.38, 0.0, -0.38)
                .cubic_to(0.14, -0.38, 0.26, -0.28, 0.26, -0.12)
                .cubic_to(0.26, -0.04, 0.22, 0.14, 0.38, 0.22)
                .close();
            let mut v = vec![dome, Path::circle(0.0, 0.31, 0.085), Path::circle(0.0, -0.42, 0.05)];
            if icon == Icon::BellOff {
                v.push(Path::line_stroke(-0.40, -0.40, 0.40, 0.40, 0.085));
            }
            v
        }
        Icon::Close => vec![Path::line_stroke(-0.26, -0.26, 0.26, 0.26, 0.11), Path::line_stroke(-0.26, 0.26, 0.26, -0.26, 0.11)],
        Icon::Trash => vec![
            Path::polygon(&[(-0.25, -0.17), (0.25, -0.17), (0.20, 0.40), (-0.20, 0.40)]),
            rr(-0.34, -0.34, 0.68, 0.10, 0.04),
            rr(-0.11, -0.46, 0.22, 0.10, 0.04),
        ],
        Icon::Note => vec![
            Path::circle(-0.14, 0.24, 0.16),
            rr(0.0, -0.36, 0.075, 0.60, 0.02),
            Path::polygon(&[(0.0, -0.36), (0.30, -0.24), (0.30, -0.08), (0.0, -0.20)]),
        ],
    }
}

/// Draw `icon` centred at (cx, cy), `size` pixels tall.
pub fn draw(cv: &mut Canvas, icon: Icon, cx: f32, cy: f32, size: f32, color: Color) {
    let parts: Vec<Path> = pieces(icon)
        .iter()
        .map(|p| {
            let mut q = Path::new();
            q.append_transformed(p, [size, 0.0, 0.0, size, 0.0, 0.0]);
            q
        })
        .collect();
    cv.fill_paths(&parts, cx, cy, color);
}

#[allow(dead_code)]
fn _unused() -> f32 {
    PI
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_draws_something_inside_its_box() {
        for icon in [
            Icon::Play, Icon::Pause, Icon::Prev, Icon::Next, Icon::Volume, Icon::VolumeLow, Icon::Mute, Icon::Bell, Icon::BellOff,
            Icon::Close, Icon::Trash, Icon::Note,
        ] {
            let mut cv = Canvas::new(40, 40);
            draw(&mut cv, icon, 20.0, 20.0, 24.0, Color::rgb(255, 255, 255));
            let lit: Vec<(usize, usize)> = (0..40).flat_map(|y| (0..40).map(move |x| (x, y))).filter(|&(x, y)| cv.pixel(x, y) != 0).collect();
            assert!(lit.len() > 40, "{icon:?} drew {} pixels", lit.len());
            // within the 24px box (plus a pixel of anti-aliasing)
            assert!(lit.iter().all(|&(x, y)| (7..=33).contains(&x) && (7..=33).contains(&y)), "{icon:?} leaks outside its box");
        }
    }

    #[test]
    fn play_and_pause_are_different_shapes() {
        let ink = |i: Icon| {
            let mut cv = Canvas::new(30, 30);
            draw(&mut cv, i, 15.0, 15.0, 20.0, Color::rgb(255, 255, 255));
            cv.pixels().to_vec()
        };
        assert_ne!(ink(Icon::Play), ink(Icon::Pause));
        assert_ne!(ink(Icon::Bell), ink(Icon::BellOff));
        assert_ne!(ink(Icon::Volume), ink(Icon::Mute));
    }
}
