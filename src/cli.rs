//! Command line: waybar feed and a tiny remote for MPRIS players.

use std::io::Write;
use std::process::ExitCode;
use std::thread::sleep;
use std::time::Duration;

use crate::art;
use crate::model::{fmt_time, waybar_json, Snapshot, Status};
use crate::mpris::{self, Client};
use crate::scale::image_dims;

pub const HELP: &str = "\
notify-rs - notification center and media card for Wayland, no dependencies

USAGE:
    notify-rs waybar [--max-len N] [--icon-only]
                                   stream waybar JSON for the music module, one line per
                                   change (default max-len 45; --icon-only: just the icon)
    notify-rs status               one line: state, artist - title, position, volume
    notify-rs art                  where the cover comes from and its real pixel size
    notify-rs toggle | play | pause | stop | next | prev
    notify-rs vol +5 | -5 | 40     change volume by percent, or set it
";

fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let i = args.iter().position(|a| a == flag)?;
    args.get(i + 1).map(String::as_str)
}

/// Returns None when `cmd` is not one of the player commands.
pub fn run(cmd: &str, args: &[String]) -> Option<ExitCode> {
    Some(match cmd {
        "waybar" => {
            let max_len = flag_value(args, "--max-len").and_then(|v| v.parse().ok()).unwrap_or(45);
            let icon_only = args.iter().any(|a| a == "--icon-only");
            waybar(max_len, icon_only)
        }
        "status" => with_current(|_, s| {
            println!("{}", status_line(&s));
            Ok(())
        }),
        "art" => with_current(|_, s| {
            print_art(&s);
            Ok(())
        }),
        "toggle" => with_current(|c, s| mpris::play_pause(&c.conn, &s.bus_name)),
        "play" => with_current(|c, s| mpris::play(&c.conn, &s.bus_name)),
        "pause" => with_current(|c, s| mpris::pause(&c.conn, &s.bus_name)),
        "stop" => with_current(|c, s| mpris::stop(&c.conn, &s.bus_name)),
        "next" => with_current(|c, s| mpris::next(&c.conn, &s.bus_name)),
        "prev" | "previous" => with_current(|c, s| mpris::previous(&c.conn, &s.bus_name)),
        "vol" | "volume" => match args.get(1) {
            Some(a) => volume(a),
            None => {
                eprintln!("usage: notify-rs vol +5 | -5 | 40");
                ExitCode::from(2)
            }
        },
        _ => return None,
    })
}

/// Run `f` against the player that would be shown right now.
fn with_current<F>(f: F) -> ExitCode
where
    F: FnOnce(&Client, Snapshot) -> mpris::Result<()>,
{
    let mut client = match Client::new() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("notify-rs: cannot reach the session bus: {e}");
            return ExitCode::FAILURE;
        }
    };
    match client.current() {
        Ok(Some(snap)) => match f(&client, snap) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("notify-rs: {e}");
                ExitCode::FAILURE
            }
        },
        Ok(None) => {
            eprintln!("notify-rs: no player running");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("notify-rs: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `+5` / `-5` are steps, a bare number is an absolute percent.
pub fn parse_volume_arg(arg: &str) -> Option<(bool, f64)> {
    let relative = arg.starts_with('+') || arg.starts_with('-');
    let pct: f64 = arg.trim_start_matches('+').parse().ok()?;
    pct.is_finite().then_some((relative, pct))
}

fn volume(arg: &str) -> ExitCode {
    let Some((relative, pct)) = parse_volume_arg(arg) else {
        eprintln!("notify-rs: not a number: {arg}");
        return ExitCode::from(2);
    };
    with_current(|c, s| {
        let new = if relative {
            mpris::volume_step(&c.conn, &s.bus_name, pct / 100.0)?
        } else {
            let v = (pct / 100.0).clamp(0.0, 1.0);
            mpris::set_volume(&c.conn, &s.bus_name, v)?;
            v
        };
        println!("{}", (new * 100.0).round() as i64);
        Ok(())
    })
}

fn print_art(s: &Snapshot) {
    println!("player:   {}", s.identity);
    let Some(url) = &s.track.art_url else {
        println!("artwork:  none reported by this player");
        return;
    };
    println!("art url:  {url}");
    match art::local_path(url).or_else(|| art::fetch(url)) {
        None if url.starts_with("http") => {
            println!("file:     could not be downloaded (https covers need `curl` installed; file:// covers need nothing)")
        }
        None => println!("file:     missing on disk"),
        Some(path) => match std::fs::read(&path) {
            Ok(b) => match image_dims(&b) {
                Some((w, h)) => {
                    println!("file:     {}\nsize:     {w} x {h} px ({} bytes)", path.display(), b.len())
                }
                None => println!("file:     {}\nsize:     unknown image format ({} bytes)", path.display(), b.len()),
            },
            Err(e) => println!("file:     {} (cannot read: {e})", path.display()),
        },
    }
}

pub fn status_line(s: &Snapshot) -> String {
    let state = match s.status {
        Status::Playing => "playing",
        Status::Paused => "paused",
        Status::Stopped => "stopped",
    };
    let t = &s.track;
    let what = match (t.artists.is_empty(), t.title.is_empty()) {
        (false, false) => format!("{} - {}", t.artist_line(), t.title),
        (true, false) => t.title.clone(),
        (false, true) => t.artist_line(),
        (true, true) => s.identity.clone(),
    };
    format!(
        "[{state}] {what} ({} / {}) vol {}%",
        fmt_time(s.position_us),
        fmt_time(t.length_us),
        (s.volume.clamp(0.0, 1.0) * 100.0).round() as i64
    )
}

/// Long-running waybar feed. Survives the session bus not being up yet,
/// browsers starting and quitting, and the bus connection dropping.
fn waybar(max_len: usize, icon_only: bool) -> ExitCode {
    let stdout = std::io::stdout();
    let mut last = String::new();

    let mut emit = |line: String| -> bool {
        if line == last {
            return true;
        }
        let mut out = stdout.lock();
        let ok = writeln!(out, "{line}").is_ok() && out.flush().is_ok();
        last = line;
        ok // false = waybar closed the pipe, so we exit
    };

    loop {
        let mut client = match Client::new().and_then(|c| c.subscribe().map(|_| c)) {
            Ok(c) => c,
            Err(_) => {
                if !emit(waybar_json(None, max_len, icon_only)) {
                    return ExitCode::SUCCESS;
                }
                sleep(Duration::from_secs(2));
                continue;
            }
        };

        loop {
            let snap = match client.current() {
                Ok(s) => s,
                Err(_) => break, // reconnect
            };
            if !emit(waybar_json(snap.as_ref(), max_len, icon_only)) {
                return ExitCode::SUCCESS;
            }
            // Sleeps in the kernel until a signal arrives; the long timeout
            // only exists to resync once in a while.
            if client.wait_for_change(Duration::from_secs(300)).is_err() {
                break;
            }
        }
        sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_args() {
        assert_eq!(parse_volume_arg("+5"), Some((true, 5.0)));
        assert_eq!(parse_volume_arg("-5"), Some((true, -5.0)));
        assert_eq!(parse_volume_arg("40"), Some((false, 40.0)));
        assert_eq!(parse_volume_arg("abc"), None);
        assert_eq!(parse_volume_arg("nan"), None);
        assert_eq!(parse_volume_arg("inf"), None);
    }
}
