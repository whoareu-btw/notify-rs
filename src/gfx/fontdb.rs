//! Finding fonts on the system: fontconfig's `fc-match` when it exists (so we
//! follow the user's own font settings), otherwise a scan of the usual font
//! directories.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::font::{probe, FaceInfo};

/// Directories that commonly hold fonts, including NixOS profile locations.
pub fn font_dirs() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(h) = &home {
        v.push(h.join(".local/share/fonts"));
        v.push(h.join(".fonts"));
        v.push(h.join(".nix-profile/share/fonts"));
        v.push(h.join(".nix-profile/share/X11/fonts"));
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_DIRS") {
        for d in std::env::split_paths(&xdg) {
            v.push(d.join("fonts"));
        }
    }
    if let Some(user) = std::env::var_os("USER") {
        v.push(PathBuf::from("/etc/profiles/per-user").join(user).join("share/fonts"));
    }
    for d in [
        "/run/current-system/sw/share/X11/fonts",
        "/run/current-system/sw/share/fonts",
        "/nix/var/nix/profiles/default/share/fonts",
        "/usr/share/fonts",
        "/usr/local/share/fonts",
    ] {
        v.push(PathBuf::from(d));
    }
    let mut seen = std::collections::HashSet::new();
    v.retain(|p| p.is_dir() && seen.insert(p.canonicalize().unwrap_or_else(|_| p.clone())));
    v
}

fn is_font_file(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("ttf" | "otf" | "ttc" | "otc")
    )
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>, limit: usize) {
    if depth > 8 || out.len() >= limit {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        // metadata() follows symlinks, which NixOS font directories are full of
        let Ok(md) = std::fs::metadata(&p) else { continue };
        if md.is_dir() {
            walk(&p, depth + 1, out, limit);
        } else if md.is_file() && is_font_file(&p) {
            out.push(p);
        }
        if out.len() >= limit {
            return;
        }
    }
}

/// Describe every face in the given directories (header reads only).
pub fn scan(dirs: &[PathBuf]) -> Vec<FaceInfo> {
    let mut files = Vec::new();
    for d in dirs {
        walk(d, 0, &mut files, 8000);
    }
    files.sort();
    files.dedup();
    files.iter().flat_map(|f| probe(f)).collect()
}

/// `fc-match -s`: the first `n` best matches for a fontconfig pattern, in order.
pub fn fc_match(pattern: &str, n: usize) -> Vec<(PathBuf, u32)> {
    let out = Command::new("fc-match")
        .args(["-s", "-f", "%{file}|%{index}\\n", pattern])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(out) = out else { return vec![] };
    if !out.status.success() {
        return vec![];
    }
    parse_fc_lines(&String::from_utf8_lossy(&out.stdout), n)
}

/// The face fontconfig has for exactly this family, not a stand-in (fc-match
/// always answers something). `Ok(None)`: it is not installed. `Err(())`:
/// fontconfig cannot be run at all, so the caller should look at the files.
pub fn fc_family(family: &str, bold: bool) -> Result<Option<(PathBuf, u32)>, ()> {
    let mut pattern = String::new();
    for c in family.chars() {
        if "\\-:,=".contains(c) {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    if bold {
        pattern.push_str(":weight=bold");
    }
    let out = Command::new("fc-match")
        .args(["-f", "%{file}|%{index}|%{family}\\n", &pattern])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|_| ())?;
    if !out.status.success() {
        return Err(());
    }
    Ok(parse_fc_family(&String::from_utf8_lossy(&out.stdout), family))
}

/// One `file|index|family,family2` line from fc-match: the face if one of its
/// family names is `family`.
pub fn parse_fc_family(text: &str, family: &str) -> Option<(PathBuf, u32)> {
    let mut it = text.lines().next()?.splitn(3, '|');
    let (file, index, names) = (it.next()?, it.next()?, it.next()?);
    if !names.split(',').any(|n| n.trim().eq_ignore_ascii_case(family)) {
        return None;
    }
    let file = PathBuf::from(file.trim());
    is_font_file(&file).then(|| (file, index.trim().parse::<u32>().unwrap_or(0) & 0xffff))
}

pub fn parse_fc_lines(text: &str, n: usize) -> Vec<(PathBuf, u32)> {
    let mut v: Vec<(PathBuf, u32)> = Vec::new();
    for line in text.lines() {
        let Some((file, idx)) = line.rsplit_once('|') else { continue };
        let file = PathBuf::from(file.trim());
        if !is_font_file(&file) {
            continue; // bitmap fonts (.pcf, .pfb) are of no use to us
        }
        let idx = idx.trim().parse::<u32>().unwrap_or(0) & 0xffff; // high bits select variable instances
        if !v.iter().any(|(f, i)| *f == file && *i == idx) {
            v.push((file, idx));
        }
        if v.len() >= n {
            break;
        }
    }
    v
}

/// Best face for `family` at `weight` among scanned faces (exact family first,
/// then a substring match). Prefers upright over italic.
pub fn find<'a>(faces: &'a [FaceInfo], family: &str, weight: u16) -> Option<&'a FaceInfo> {
    let fam = family.to_lowercase();
    let score = |f: &FaceInfo| -> Option<i32> {
        let name = f.family.to_lowercase();
        let base = if name == fam {
            0
        } else if name.contains(&fam) {
            1000
        } else {
            return None;
        };
        // a variable font can be set to any weight, so its default weight says nothing
        let distance = if f.variable { 0 } else { (f.weight as i32 - weight as i32).abs() };
        Some(base + distance + if f.italic { 500 } else { 0 })
    };
    faces.iter().filter_map(|f| score(f).map(|s| (s, f))).min_by_key(|(s, _)| *s).map(|(_, f)| f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fc_match_output() {
        let t = "/usr/share/fonts/a.ttf|0\n/usr/share/fonts/b.ttc|2\n/usr/share/fonts/x.pcf.gz|0\n/usr/share/fonts/a.ttf|0\njunk\n/usr/share/fonts/c.otf|1048576\n";
        let v = parse_fc_lines(t, 10);
        assert_eq!(
            v,
            vec![
                (PathBuf::from("/usr/share/fonts/a.ttf"), 0),
                (PathBuf::from("/usr/share/fonts/b.ttc"), 2),
                (PathBuf::from("/usr/share/fonts/c.otf"), 0),
            ]
        );
        assert_eq!(parse_fc_lines(t, 1).len(), 1);
    }

    fn face(family: &str, weight: u16, italic: bool) -> FaceInfo {
        FaceInfo { path: PathBuf::from(format!("/{family}-{weight}-{italic}")), index: 0, family: family.into(), style: String::new(), weight, italic, variable: false }
    }

    #[test]
    fn exact_family_from_fc_match() {
        let t = "/run/current-system/sw/share/fonts/opentype/noto-cjk/NotoSansCJK-VF.otf.ttc|65536|Noto Sans CJK JP,Noto Sans CJK JP Thin\n";
        assert_eq!(
            parse_fc_family(t, "Noto Sans CJK JP"),
            Some((PathBuf::from("/run/current-system/sw/share/fonts/opentype/noto-cjk/NotoSansCJK-VF.otf.ttc"), 0)),
            "the variable-instance bits of the index are dropped"
        );
        assert!(parse_fc_family(t, "noto sans cjk jp").is_some(), "case does not matter");
        assert!(parse_fc_family(t, "Noto Sans CJK").is_none(), "a different family is a stand-in, not a match");
        let stand_in = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf|0|DejaVu Sans\n";
        assert!(parse_fc_family(stand_in, "JetBrainsMono Nerd Font Propo").is_none());
        assert!(parse_fc_family("/x/a.pcf.gz|0|Fixed\n", "Fixed").is_none(), "bitmap fonts are no use");
        assert!(parse_fc_family("", "x").is_none() && parse_fc_family("junk", "x").is_none());
    }

    #[test]
    fn variable_faces_match_any_weight() {
        let mut vf = face("Noto Sans CJK JP", 100, false);
        vf.variable = true;
        let faces = vec![vf, face("Noto Sans CJK JP", 400, false)];
        // a variable face beats a static 400 when 700 is wanted
        let got = find(&faces, "Noto Sans CJK JP", 700).unwrap();
        assert!(got.variable, "{got:?}");
    }

    #[test]
    fn family_and_weight_selection() {
        let faces = vec![
            face("Inter", 400, false),
            face("Inter", 400, true),
            face("Inter", 700, false),
            face("Inter Display", 400, false),
            face("DejaVu Sans", 400, false),
        ];
        assert_eq!(find(&faces, "inter", 400).unwrap().weight, 400);
        assert!(!find(&faces, "Inter", 400).unwrap().italic);
        assert_eq!(find(&faces, "Inter", 700).unwrap().weight, 700);
        assert_eq!(find(&faces, "Inter", 600).unwrap().weight, 700, "closest weight");
        assert_eq!(find(&faces, "Inter", 400).unwrap().family, "Inter", "exact family beats substring");
        assert!(find(&faces, "Comic Sans", 400).is_none());
    }

    #[test]
    fn scan_finds_system_fonts_if_any() {
        let dir = PathBuf::from("/usr/share/fonts/truetype/dejavu");
        if !dir.is_dir() {
            return;
        }
        let faces = scan(&[dir]);
        assert!(faces.iter().any(|f| f.family == "DejaVu Sans" && f.weight == 400 && !f.italic), "{faces:?}");
        assert!(faces.iter().any(|f| f.family == "DejaVu Sans" && f.weight >= 700));
    }
}
