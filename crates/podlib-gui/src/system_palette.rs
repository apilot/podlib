//! System palette: map the ACTIVE desktop GTK theme onto egui visuals.
//!
//! WHY: the app must follow whatever theme is enabled right now
//! (today Catppuccin Mocha, tomorrow Tokyonight) without a single
//! theme color literal in app code. We resolve the GTK theme name
//! (`$GTK_THEME` → gsettings → settings.ini), read its `@define-color`
//! CSS variables (GTK protocol names, incl. legacy and KDE `*_breeze`
//! variants), map them onto egui visual slots and apply to the
//! matching dark/light slot. Anything unresolved → quiet no-op, stock
//! egui visuals remain.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use egui::Color32;

// Variable-name chains: first hit wins. These are NAMES of the GTK
// theming protocol (GNOME/libadwaita → legacy → KDE breeze suffixes) —
// never color values; the values come from the active theme only.
const WINDOW_FILL_CHAIN: &[&str] = &[
    "window_bg_color",
    "theme_bg_color",
    "bg_color",
    "theme_bg_color_breeze",
];
const EXTREME_CHAIN: &[&str] = &[
    "view_bg_color",
    "theme_base_color",
    "base_color",
    "content_view_bg",
    "theme_base_color_breeze",
    "content_view_bg_breeze",
];
const FAINT_CHAIN: &[&str] = &["insensitive_bg_color", "theme_unfocused_bg_color"];
const TEXT_CHAIN: &[&str] = &[
    "window_fg_color",
    "theme_fg_color",
    "fg_color",
    "theme_text_color",
    "text_color",
    "theme_fg_color_breeze",
];
const WEAK_TEXT_CHAIN: &[&str] = &[
    "insensitive_fg_color",
    "theme_unfocused_fg_color",
    "placeholder_text_color",
    "insensitive_fg_color_breeze",
];
const ACCENT_BG_CHAIN: &[&str] = &[
    "accent_bg_color",
    "theme_selected_bg_color",
    "selected_bg_color",
    "theme_button_decoration_focus_breeze",
    "theme_selected_bg_color_breeze",
];
const ACCENT_FG_CHAIN: &[&str] = &[
    "accent_fg_color",
    "theme_selected_fg_color",
    "selected_fg_color",
    "theme_selected_fg_color_breeze",
];
const BUTTON_BG_CHAIN: &[&str] = &["theme_button_background_normal_breeze"];
const HOVER_BG_CHAIN: &[&str] = &["theme_button_decoration_hover_breeze"];
const ERROR_CHAIN: &[&str] = &["error_color", "error_color_breeze"];
const SUCCESS_CHAIN: &[&str] = &["success_color", "success_color_breeze"];
const WARNING_CHAIN: &[&str] = &["warning_color", "warning_color_breeze"];
const BORDERS_CHAIN: &[&str] = &["borders", "unfocused_borders", "borders_breeze"];

/// Apply the resolved system palette to the matching dark/light theme
/// slot. Quiet no-op when the theme or its variables don't resolve.
pub(crate) fn apply(ctx: &egui::Context) {
    let Some(resolved) = resolve() else { return };
    // WHY only the matching slot: ThemePreference::System picks which
    // slot egui uses; we paint just that one and leave the other stock.
    let theme = if resolved.dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    };
    ctx.set_visuals_of(theme, map_visuals(&resolved.vars, resolved.dark));
}

/// Success accent (device status dot) from the active palette; None
/// when nothing resolved. Callers fall back to
/// `visuals.selection.bg_fill` — never a green literal. WHY success
/// only: it's the one accent consumed outside egui slots today; error
/// and warning already land via `visuals.*_fg_color` in `map_text`.
pub(crate) fn accent_success() -> Option<Color32> {
    let resolved = resolve()?;
    accents_from(&resolved.vars)
}

/// Pure resolution of the success accent from a variable map.
fn accents_from(vars: &HashMap<String, Color32>) -> Option<Color32> {
    pick(vars, SUCCESS_CHAIN).or_else(|| pick(vars, ACCENT_BG_CHAIN))
}

struct Resolved {
    dark: bool,
    vars: HashMap<String, Color32>,
}

/// Resolve the active GTK theme: name → merged `@define-color` map.
fn resolve() -> Option<Resolved> {
    let name = theme_name()?;
    let dark = !is_light_theme(&name);
    let vars = collect_vars(&css_paths(&name, dark));
    if vars.is_empty() {
        // Nothing resolved → stock visuals (caller already no-ops).
        return None;
    }
    Some(Resolved { dark, vars })
}

/// Active GTK theme name. Order: `$GTK_THEME` (may carry a `:variant`)
/// → `gsettings get org.gnome.desktop.interface gtk-theme` (quotes
/// stripped) → user `settings.ini` files (`gtk-theme-name=`).
fn theme_name() -> Option<String> {
    if let Ok(value) = std::env::var("GTK_THEME") {
        let name = strip_quotes(&value);
        if !name.is_empty() {
            return Some(name.to_owned());
        }
    }
    if let Ok(output) = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "gtk-theme"])
        .output()
    {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let name = strip_quotes(stdout.trim());
            if !name.is_empty() {
                return Some(name.to_owned());
            }
        }
    }
    for ini in ["gtk-3.0/settings.ini", "gtk-4.0/settings.ini"] {
        let path = config_dir().join(ini);
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Some(name) = gtk_theme_from_ini(&content) {
                return Some(name);
            }
        }
    }
    None
}

/// Strip one pair of surrounding single/double quotes (gsettings and
/// settings.ini quote their values).
fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    for q in ['\'', '"'] {
        if let Some(inner) = s.strip_prefix(q).and_then(|rest| rest.strip_suffix(q)) {
            return inner;
        }
    }
    s
}

/// Extract `gtk-theme-name=` from a settings.ini body (pure).
fn gtk_theme_from_ini(content: &str) -> Option<String> {
    for line in content.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        if key.trim() == "gtk-theme-name" {
            let name = strip_quotes(value);
            if !name.is_empty() {
                return Some(name.to_owned());
            }
        }
    }
    None
}

/// Light slot detection: the name says "light" and not "dark"
/// (case-insensitive; GTK_THEME variants like `Yaru:light` included).
fn is_light_theme(name: &str) -> bool {
    // WHY name-scan: the GTK protocol exposes no portable lightness
    // variable; the theme name (+ variant) is the only signal.
    let lower = name.to_lowercase();
    lower.contains("light") && !lower.contains("dark")
}

/// Candidate CSS files in merge order (later overrides): theme
/// gtk-4.0 → gtk-3.0, then user gtk-4.0/gtk.css → gtk-3.0/gtk.css
/// (the user layer overrides the theme, with its own @imports).
fn css_paths(theme: &str, dark: bool) -> Vec<PathBuf> {
    // WHY split at ':': $GTK_THEME may carry a variant ("Name:variant");
    // the theme directory is named after the bare name.
    let base = theme.split(':').next().unwrap_or(theme);
    let mut files = Vec::new();
    for root in theme_roots() {
        for toolkit in ["gtk-4.0", "gtk-3.0"] {
            let dir = root.join(base).join(toolkit);
            files.push(dir.join("gtk.css"));
            // WHY gtk-dark.css only when dark: for a light theme the
            // dark variant file would inject dark colors.
            if dark {
                files.push(dir.join("gtk-dark.css"));
            }
        }
    }
    files.push(config_dir().join("gtk-4.0/gtk.css"));
    files.push(config_dir().join("gtk-3.0/gtk.css"));
    let mut seen = std::collections::HashSet::new();
    files.retain(|p| p.is_file() && seen.insert(p.clone()));
    files
}

/// Theme search roots (GTK order): ~/.themes, XDG data dirs, system.
fn theme_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        roots.push(PathBuf::from(home).join(".themes"));
    }
    roots.push(data_dir().join("themes"));
    roots.push(PathBuf::from("/usr/share/themes"));
    roots
}

/// `$XDG_DATA_HOME` or the `$HOME` default.
fn data_dir() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(home_dir()).join(".local/share"))
}

/// `$XDG_CONFIG_HOME` or the `$HOME` default.
fn config_dir() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(home_dir()).join(".config"))
}

/// `$HOME` or empty (derived paths then simply don't exist).
fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// Read every candidate CSS in order into one variable map; later
/// files override earlier, and `@name` references may point across
/// files. Filesystem+parse glue isolated here (tests use tempdirs).
fn collect_vars(paths: &[PathBuf]) -> HashMap<String, Color32> {
    let mut raw = Vec::new();
    for path in paths {
        let Ok(css) = std::fs::read_to_string(path) else {
            continue;
        };
        let dir = path.parent().unwrap_or(Path::new(""));
        raw.extend(raw_defines(&expand_local_imports(&css, dir)));
    }
    resolve_defines(&raw).into_iter().collect()
}

/// Only `@define-color <name> <color>;` declarations of one CSS body
/// (still raw strings), resolved separately by `resolve_defines`.
fn raw_defines(css: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in css.lines() {
        let Some(rest) = line.trim().strip_prefix("@define-color") else {
            continue;
        };
        let Some((name, value)) = rest.trim_start().split_once(char::is_whitespace) else {
            continue;
        };
        let value = value.trim().trim_end_matches(';').trim();
        if !name.is_empty() && !value.is_empty() {
            out.push((name.to_owned(), value.to_owned()));
        }
    }
    out
}

/// Pass 2: resolve raw values into colors; `@name` references may
/// point at any entry (also across merged files). WHY silent early
/// keep: a re-defined name whose later value fails to parse stays on
/// the earlier color — loyal to themes; GTK in that case simply
/// doesn't set the variable.
fn resolve_defines(raw: &[(String, String)]) -> Vec<(String, Color32)> {
    let vars: HashMap<&str, &str> = raw.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    raw.iter()
        .filter_map(|(k, v)| parse_color(v, &vars, 0).map(|c| (k.clone(), c)))
        .collect()
}

/// Parse one GTK-CSS color: `#rgb/#rrggbb/#rrggbbaa`, `rgb()/rgba()`,
/// `@name` reference into `vars` (depth-bounded), a small CSS name
/// vocabulary, `none`/unknown → None. `depth` guards reference cycles.
fn parse_color(s: &str, vars: &HashMap<&str, &str>, depth: u8) -> Option<Color32> {
    let s = s.trim();
    // WHY depth cap: tolerate hand-written reference cycles in themes.
    if depth > 8 || s.is_empty() || s.eq_ignore_ascii_case("none") {
        return None;
    }
    if let Some(name) = s.strip_prefix('@') {
        return parse_color(vars.get(name)?, vars, depth + 1);
    }
    parse_hex(s)
        .or_else(|| parse_functional(s))
        .or_else(|| parse_named(s))
}

/// `#rgb` (nibbles doubled), `#rrggbb`, `#rrggbbaa`.
fn parse_hex(s: &str) -> Option<Color32> {
    let hex = s.strip_prefix('#')?;
    if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    match hex.len() {
        3 => {
            let b = hex.as_bytes();
            Some(Color32::from_rgb(
                nibble(b[0]) * 17,
                nibble(b[1]) * 17,
                nibble(b[2]) * 17,
            ))
        }
        6 | 8 => {
            let r = hex2(&hex[0..2])?;
            let g = hex2(&hex[2..4])?;
            let b = hex2(&hex[4..6])?;
            let a = if hex.len() == 8 {
                hex2(&hex[6..8])?
            } else {
                255
            };
            Some(Color32::from_rgba_unmultiplied(r, g, b, a))
        }
        _ => None,
    }
}

fn hex2(s: &str) -> Option<u8> {
    u8::from_str_radix(s, 16).ok()
}

/// ASCII hex digit (already validated) → value; `| 0x20` lowercases.
fn nibble(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        _ => (b | 0x20) - b'a' + 10,
    }
}

/// `rgb(...)` / `rgba(...)`: numbers split on `,` `/` spaces; optional
/// `%` per component (scaled ×255/100 at parse); 3 → opaque, the 4th
/// is alpha: CSS 0..1 float or raw byte (≤1.0 heuristic). WHY percent
/// tracking: a scaled percent alpha (e.g. 0.3% → 0.765) must not
/// re-enter the float heuristic and get scaled a second time.
fn parse_functional(s: &str) -> Option<Color32> {
    let (name, inner) = s.split_once('(')?;
    let inner = inner.strip_suffix(')')?;
    if !matches!(name.trim().to_ascii_lowercase().as_str(), "rgb" | "rgba") {
        return None;
    }
    let mut nums: Vec<(f32, bool)> = Vec::new();
    for part in inner.split([',', ' ', '/']) {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some(pct) = part.strip_suffix('%') {
            nums.push((pct.trim().parse::<f32>().ok()? * 255.0 / 100.0, true));
        } else {
            nums.push((part.parse::<f32>().ok()?, false));
        }
    }
    match nums.as_slice() {
        [(r, _), (g, _), (b, _)] => Some(Color32::from_rgb(channel(*r), channel(*g), channel(*b))),
        [(r, _), (g, _), (b, _), (a, is_pct)] => {
            // WHY ≤1.0 heuristic: CSS alpha is a 0..1 float (themes
            // write rgba(...,0.5) = half opacity); raw bytes also
            // accepted. Percent components are already scaled.
            let a = if *is_pct {
                *a
            } else if *a <= 1.0 {
                a * 255.0
            } else {
                *a
            };
            Some(Color32::from_rgba_unmultiplied(
                channel(*r),
                channel(*g),
                channel(*b),
                channel(a),
            ))
        }
        _ => None,
    }
}

fn channel(v: f32) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}

/// Small CSS named-color vocabulary. WHY literals: this is the CSS
/// protocol's own keyword table (themes reference these names inside
/// @define-color values) — parser vocabulary, not app styling.
fn parse_named(s: &str) -> Option<Color32> {
    match s.trim().to_ascii_lowercase().as_str() {
        "white" => Some(Color32::WHITE),
        "black" => Some(Color32::BLACK),
        "gray" | "grey" => Some(Color32::from_rgb(0x80, 0x80, 0x80)),
        "silver" => Some(Color32::from_rgb(0xC0, 0xC0, 0xC0)),
        "red" => Some(Color32::from_rgb(255, 0, 0)),
        "green" => Some(Color32::from_rgb(0, 128, 0)),
        "blue" => Some(Color32::from_rgb(0, 0, 255)),
        "yellow" => Some(Color32::from_rgb(255, 255, 0)),
        "orange" => Some(Color32::from_rgb(255, 165, 0)),
        "purple" => Some(Color32::from_rgb(128, 0, 128)),
        "pink" => Some(Color32::from_rgb(255, 192, 203)),
        "brown" => Some(Color32::from_rgb(165, 42, 42)),
        _ => None,
    }
}

/// Inline one level of local `@import 'x.css'` / `@import "x.css"` /
/// `@import url("x.css")` relative to `dir`; missing files are
/// dropped (noctalia.css case), other lines pass through unchanged.
fn expand_local_imports(css: &str, dir: &Path) -> String {
    let mut out = String::with_capacity(css.len());
    for line in css.lines() {
        match local_import_target(line) {
            // Missing file → drop the line (nothing to inline).
            Some(name) => {
                if let Ok(imported) = std::fs::read_to_string(dir.join(name)) {
                    out.push_str(&imported);
                    // WHY the newline guard: a file without a trailing
                    // newline must not merge with the next line.
                    if !imported.ends_with('\n') {
                        out.push('\n');
                    }
                }
            }
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

/// File name of a single-level local @import line, else None.
fn local_import_target(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("@import")?.trim_start();
    let rest = match rest.strip_prefix("url(") {
        // WHY the ';' strip first: the import ends with ");" — the
        // semicolon trails the closing paren.
        Some(u) => u.trim_end_matches(';').trim().strip_suffix(')')?.trim(),
        None => rest,
    };
    let quoted = rest.strip_prefix(['"', '\''])?;
    let end = quoted.find(['"', '\''])?;
    let name = &quoted[..end];
    // Only local relative files inline; URLs and absolute paths don't.
    (!name.is_empty() && !name.contains("://") && !name.starts_with('/')).then_some(name.to_owned())
}

/// Map resolved variables onto egui slots (pure). Slots without a
/// variable keep the stock dark/light base.
fn map_visuals(vars: &HashMap<String, Color32>, dark: bool) -> egui::Visuals {
    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    map_backgrounds(&mut v, vars);
    map_text(&mut v, vars);
    map_accent(&mut v, vars);
    map_widgets(&mut v, vars);
    v
}

/// Panel/window fill (one surface), extreme (inputs/tables) and faint
/// (disabled) backgrounds.
fn map_backgrounds(v: &mut egui::Visuals, vars: &HashMap<String, Color32>) {
    let first = |chain: &[&str]| pick(vars, chain);
    if let Some(fill) = first(WINDOW_FILL_CHAIN) {
        v.panel_fill = fill;
        v.window_fill = fill;
    }
    if let Some(fill) = first(EXTREME_CHAIN) {
        v.extreme_bg_color = fill;
    }
    // WHY fallback to window fill: themes without insensitive vars
    // still need a "disabled" background from the same palette.
    v.faint_bg_color = first(FAINT_CHAIN)
        .or_else(|| first(WINDOW_FILL_CHAIN))
        .unwrap_or(v.faint_bg_color);
}

/// Text strokes: normal, weak, error and warning foregrounds.
fn map_text(v: &mut egui::Visuals, vars: &HashMap<String, Color32>) {
    let first = |chain: &[&str]| pick(vars, chain);
    if let Some(text) = first(TEXT_CHAIN) {
        // WHY fg_stroke of widgets: egui 0.37 has no text_color field —
        // Visuals::text_color()/strong_text_color() read these strokes.
        for w in [
            &mut v.widgets.noninteractive,
            &mut v.widgets.inactive,
            &mut v.widgets.hovered,
            &mut v.widgets.active,
        ] {
            w.fg_stroke.color = text;
        }
    }
    v.weak_text_color = first(WEAK_TEXT_CHAIN).or(v.weak_text_color);
    if let Some(e) = first(ERROR_CHAIN) {
        v.error_fg_color = e;
    }
    if let Some(w) = first(WARNING_CHAIN) {
        v.warn_fg_color = w;
    }
}

/// Accent: selection fill + selected-text stroke, hyperlink.
fn map_accent(v: &mut egui::Visuals, vars: &HashMap<String, Color32>) {
    if let Some(accent) = pick(vars, ACCENT_BG_CHAIN) {
        v.selection.bg_fill = accent;
        v.hyperlink_color = accent;
    }
    if let Some(fg) = pick(vars, ACCENT_FG_CHAIN) {
        v.selection.stroke.color = fg;
    }
}

/// Widget fills: button at rest (extreme fallback), hover/active
/// (button fallback), widget borders stroke.
fn map_widgets(v: &mut egui::Visuals, vars: &HashMap<String, Color32>) {
    let first = |chain: &[&str]| pick(vars, chain);
    // WHY extreme fallback: only breeze themes name the button fill;
    // elsewhere buttons share the base surface color.
    let button = first(BUTTON_BG_CHAIN).or_else(|| first(EXTREME_CHAIN));
    if let Some(btn) = button {
        v.widgets.inactive.bg_fill = btn;
        v.widgets.inactive.weak_bg_fill = btn;
    }
    if let Some(hover) = first(HOVER_BG_CHAIN).or(button) {
        v.widgets.hovered.bg_fill = hover;
        v.widgets.hovered.weak_bg_fill = hover;
        v.widgets.active.bg_fill = hover;
        v.widgets.active.weak_bg_fill = hover;
    }
    if let Some(border) = first(BORDERS_CHAIN) {
        v.widgets.inactive.bg_stroke.color = border;
    }
}

/// First variable of `chain` present in the map.
fn pick(vars: &HashMap<String, Color32>, chain: &[&str]) -> Option<Color32> {
    chain.iter().find_map(|name| vars.get(*name).copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    // WHY literals here: test ASSERTS only — the app itself carries no
    // theme color values (they come from the system at runtime).

    const LAVENDER: Color32 = Color32::from_rgb(0xB4, 0xBE, 0xFE);
    const BASE: Color32 = Color32::from_rgb(0x1E, 0x1E, 0x2E);

    fn raw(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn map(pairs: &[(&str, u8, u8, u8)]) -> HashMap<String, Color32> {
        pairs
            .iter()
            .map(|(n, r, g, b)| (n.to_string(), Color32::from_rgb(*r, *g, *b)))
            .collect()
    }

    #[test]
    fn parse_color_hex_forms() {
        // #rgb: nibbles are doubled (a → 0xAA).
        assert_eq!(
            parse_color("#abc", &HashMap::new(), 0),
            Some(Color32::from_rgb(0xAA, 0xBB, 0xCC))
        );
        assert_eq!(parse_color("#1e1e2e", &HashMap::new(), 0), Some(BASE));
        assert_eq!(
            parse_color("#1e1e2eff", &HashMap::new(), 0),
            Some(Color32::from_rgba_unmultiplied(0x1E, 0x1E, 0x2E, 255))
        );
        assert_eq!(
            parse_color("#1e1e2e80", &HashMap::new(), 0),
            Some(Color32::from_rgba_unmultiplied(0x1E, 0x1E, 0x2E, 128))
        );
        assert_eq!(parse_color("#12345", &HashMap::new(), 0), None);
        assert_eq!(parse_color("#zzz", &HashMap::new(), 0), None);
    }

    #[test]
    fn parse_color_rgb_and_rgba() {
        assert_eq!(
            parse_color("rgb(30, 30, 46)", &HashMap::new(), 0),
            Some(BASE)
        );
        // Modern space-separated syntax too.
        assert_eq!(parse_color("rgb(30 30 46)", &HashMap::new(), 0), Some(BASE));
        // WHY 0.5 → 128: CSS alpha is a 0..1 float, not a byte.
        assert_eq!(
            parse_color("rgba(255, 255, 255, 0.5)", &HashMap::new(), 0),
            Some(Color32::from_rgba_unmultiplied(255, 255, 255, 128))
        );
        // Percent forms.
        assert_eq!(
            parse_color("rgb(25%, 50%, 75%)", &HashMap::new(), 0),
            Some(Color32::from_rgb(64, 128, 191))
        );
    }

    #[test]
    fn parse_color_references_chain_and_cycle_guard() {
        let vars = raw(&[("a", "@b"), ("b", "@c"), ("c", "#1e1e2e")]);
        let vars: HashMap<&str, &str> =
            vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(parse_color("@a", &vars, 0), Some(BASE));
        assert_eq!(parse_color("@missing", &vars, 0), None);
        let cycle: HashMap<&str, &str> = [("loop", "@loop")].into_iter().collect();
        assert_eq!(parse_color("@loop", &cycle, 0), None);
    }

    #[test]
    fn parse_color_percent_alpha() {
        // 50% opacity → half alpha (50 × 2.55 = 127.5 → 128).
        assert_eq!(
            parse_color("rgba(255, 255, 255, 50%)", &HashMap::new(), 0),
            Some(Color32::from_rgba_unmultiplied(255, 255, 255, 128))
        );
        // WHY this case: a tiny percent alpha scales below the ≤1.0
        // float heuristic — it must not be re-scaled a second time
        // (0.3% of 255 ≈ 1, not 77% opacity).
        assert_eq!(
            parse_color("rgba(255, 255, 255, 0.3%)", &HashMap::new(), 0),
            Some(Color32::from_rgba_unmultiplied(255, 255, 255, 1))
        );
        // Float and byte alpha forms keep their semantics.
        assert_eq!(
            parse_color("rgba(0, 0, 0, 0.5)", &HashMap::new(), 0),
            Some(Color32::from_rgba_unmultiplied(0, 0, 0, 128))
        );
        assert_eq!(
            parse_color("rgba(0, 0, 0, 100%)", &HashMap::new(), 0),
            Some(Color32::from_rgba_unmultiplied(0, 0, 0, 255))
        );
    }

    #[test]
    fn parse_color_css_names_and_none() {
        assert_eq!(
            parse_color("white", &HashMap::new(), 0),
            Some(Color32::WHITE)
        );
        assert_eq!(
            parse_color("BLACK", &HashMap::new(), 0),
            Some(Color32::BLACK)
        );
        assert_eq!(
            parse_color("silver", &HashMap::new(), 0),
            Some(Color32::from_rgb(0xC0, 0xC0, 0xC0))
        );
        assert_eq!(
            parse_color("gray", &HashMap::new(), 0),
            Some(Color32::from_rgb(0x80, 0x80, 0x80))
        );
        assert_eq!(parse_color("none", &HashMap::new(), 0), None);
        assert_eq!(parse_color("", &HashMap::new(), 0), None);
        assert_eq!(parse_color("notacolor", &HashMap::new(), 0), None);
    }

    #[test]
    fn raw_defines_multiline_and_garbage() {
        let css = "\
@define-color theme_bg_color #1e1e2e;
garbage line
@define-color   insensitive_fg_color   rgba(255, 255, 255, 0.5);
@define-color broken
@define-color linked_color @theme_selected_bg_color;
.window { background: #000; }
@define-color empty ;";
        let got = raw_defines(css);
        assert_eq!(got.len(), 3);
        assert_eq!(
            got[0],
            ("theme_bg_color".to_string(), "#1e1e2e".to_string())
        );
        assert_eq!(
            got[1],
            (
                "insensitive_fg_color".to_string(),
                "rgba(255, 255, 255, 0.5)".to_string()
            )
        );
        assert_eq!(
            got[2],
            (
                "linked_color".to_string(),
                "@theme_selected_bg_color".to_string()
            )
        );
    }

    #[test]
    fn parse_define_colors_resolves_references() {
        let css = "@define-color theme_selected_bg_color #b4befe;\n\
                   @define-color accent_color @theme_selected_bg_color;\n";
        let got = resolve_defines(&raw_defines(css));
        assert_eq!(got.len(), 2);
        assert_eq!(got[1], ("accent_color".to_string(), LAVENDER));
    }

    #[test]
    fn map_visuals_background_and_text_slots() {
        let vars = map(&[
            ("window_bg_color", 0x18, 0x18, 0x25),
            ("view_bg_color", 0x1E, 0x1E, 0x2E),
            ("insensitive_bg_color", 0x11, 0x11, 0x11),
            ("theme_fg_color", 0xC0, 0xC0, 0xC0),
            ("insensitive_fg_color", 0x60, 0x60, 0x60),
        ]);
        let v = map_visuals(&vars, true);
        assert_eq!(v.panel_fill, Color32::from_rgb(0x18, 0x18, 0x25));
        assert_eq!(v.window_fill, v.panel_fill);
        assert_eq!(v.extreme_bg_color, BASE);
        assert_eq!(v.faint_bg_color, Color32::from_rgb(0x11, 0x11, 0x11));
        assert_eq!(
            v.widgets.noninteractive.fg_stroke.color,
            Color32::from_rgb(0xC0, 0xC0, 0xC0)
        );
        assert_eq!(v.weak_text_color, Some(Color32::from_rgb(0x60, 0x60, 0x60)));
        assert!(v.dark_mode);
    }

    #[test]
    fn map_visuals_widgets_accent_and_status_slots() {
        let vars = map(&[
            ("accent_bg_color", 0xB4, 0xBE, 0xFE),
            ("accent_fg_color", 0x11, 0x11, 0x18),
            ("theme_button_background_normal_breeze", 0x31, 0x32, 0x44),
            ("theme_button_decoration_hover_breeze", 0x45, 0x46, 0x58),
            ("borders", 0x20, 0x20, 0x20),
            ("error_color", 0xF3, 0x8B, 0xA8),
            ("warning_color", 0xFA, 0xB3, 0x87),
        ]);
        let v = map_visuals(&vars, true);
        assert_eq!(v.selection.bg_fill, LAVENDER);
        assert_eq!(
            v.selection.stroke.color,
            Color32::from_rgb(0x11, 0x11, 0x18)
        );
        assert_eq!(v.hyperlink_color, LAVENDER);
        assert_eq!(
            v.widgets.inactive.bg_fill,
            Color32::from_rgb(0x31, 0x32, 0x44)
        );
        assert_eq!(
            v.widgets.hovered.bg_fill,
            Color32::from_rgb(0x45, 0x46, 0x58)
        );
        assert_eq!(
            v.widgets.active.bg_fill,
            Color32::from_rgb(0x45, 0x46, 0x58)
        );
        assert_eq!(
            v.widgets.inactive.bg_stroke.color,
            Color32::from_rgb(0x20, 0x20, 0x20)
        );
        assert_eq!(v.error_fg_color, Color32::from_rgb(0xF3, 0x8B, 0xA8));
        assert_eq!(v.warn_fg_color, Color32::from_rgb(0xFA, 0xB3, 0x87));
    }

    #[test]
    fn map_visuals_fallbacks_and_light_base() {
        // No breeze button/hover, no faint, no weak text, no accents:
        // button → extreme chain; hover → button; faint → window fill;
        // weak text stays None (stock alpha fallback); selection stays
        // stock egui (no fake accent invented from nothing).
        let vars = map(&[
            ("window_bg_color", 0x18, 0x18, 0x25),
            ("base_color", 0x1E, 0x1E, 0x2E),
        ]);
        let stock_light_selection = egui::Visuals::light().selection;
        let v = map_visuals(&vars, false);
        assert!(!v.dark_mode);
        assert_eq!(v.widgets.inactive.bg_fill, BASE);
        assert_eq!(v.widgets.hovered.bg_fill, BASE);
        assert_eq!(v.faint_bg_color, Color32::from_rgb(0x18, 0x18, 0x25));
        assert_eq!(v.weak_text_color, None);
        assert_eq!(v.selection.bg_fill, stock_light_selection.bg_fill);
    }

    #[test]
    fn pick_first_hit_wins() {
        let vars = map(&[("bg_color", 1, 2, 3), ("theme_bg_color", 4, 5, 6)]);
        // Chain order: theme_bg_color precedes bg_color.
        assert_eq!(
            pick(&vars, WINDOW_FILL_CHAIN),
            Some(Color32::from_rgb(4, 5, 6))
        );
        assert_eq!(pick(&vars, SUCCESS_CHAIN), None);
    }

    #[test]
    fn accent_success_fallback_chain() {
        let vars = map(&[("success_color", 5, 6, 7)]);
        assert_eq!(accents_from(&vars), Some(Color32::from_rgb(5, 6, 7)));
        // No success var → accent bg fallback (selection fill semantics).
        let vars = map(&[("accent_bg_color", 9, 9, 9)]);
        assert_eq!(accents_from(&vars), Some(Color32::from_rgb(9, 9, 9)));
        assert_eq!(accents_from(&HashMap::new()), None);
    }

    #[test]
    fn is_light_theme_by_name() {
        assert!(!is_light_theme("Catppuccin-Mocha-Standard-Lavender-Dark"));
        assert!(!is_light_theme("Tokyonight-Dark"));
        assert!(!is_light_theme("Adwaita"));
        assert!(is_light_theme("Adwaita-light"));
        assert!(is_light_theme("Yaru:light"));
        assert!(is_light_theme("Lightly"));
    }

    #[test]
    fn strip_quotes_removes_one_pair() {
        assert_eq!(strip_quotes("'Catppuccin-Mocha'"), "Catppuccin-Mocha");
        assert_eq!(strip_quotes("\"Tokyonight\""), "Tokyonight");
        assert_eq!(strip_quotes("Adwaita"), "Adwaita");
        assert_eq!(strip_quotes("  'X'  "), "X");
        assert_eq!(strip_quotes("''"), "");
    }

    #[test]
    fn ini_theme_name_parsing() {
        assert_eq!(
            gtk_theme_from_ini(
                "[Settings]\ngtk-theme-name=Catppuccin-Mocha-Standard-Lavender-Dark\n"
            ),
            Some("Catppuccin-Mocha-Standard-Lavender-Dark".to_string())
        );
        assert_eq!(
            gtk_theme_from_ini("gtk-theme-name = 'Adwaita'\n"),
            Some("Adwaita".to_string())
        );
        assert_eq!(
            gtk_theme_from_ini("[Settings]\ngtk-icon-theme-name=Papirus\n"),
            None
        );
        assert_eq!(gtk_theme_from_ini("gtk-theme-name=\n"), None);
    }

    #[test]
    fn local_import_targets() {
        assert_eq!(
            local_import_target("@import 'colors.css';"),
            Some("colors.css".to_string())
        );
        assert_eq!(
            local_import_target("@import url(\"noctalia.css\");"),
            Some("noctalia.css".to_string())
        );
        assert_eq!(
            local_import_target("@import url('colors.css');"),
            Some("colors.css".to_string())
        );
        assert_eq!(
            local_import_target("@import url(https://example.com/x.css);"),
            None
        );
        assert_eq!(local_import_target("@import '/abs/path.css';"), None);
        assert_eq!(local_import_target("background: red;"), None);
    }

    #[test]
    fn expand_imports_inlines_and_skips_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("colors.css"), "@define-color x #101010;\n").unwrap();
        let css =
            "@import 'colors.css';\n@import url(\"noctalia.css\");\n@define-color y #020203;\n";
        let out = expand_local_imports(css, dir.path());
        assert!(out.contains("@define-color x #101010;"));
        assert!(out.contains("@define-color y #020203;"));
        assert!(!out.contains("noctalia.css"));
    }

    #[test]
    fn collect_vars_later_file_overrides() {
        let dir = tempfile::TempDir::new().unwrap();
        let a = dir.path().join("a.css");
        let b = dir.path().join("b.css");
        std::fs::write(
            &a,
            "@define-color theme_bg_color #111111;\n@define-color only_a #222222;\n",
        )
        .unwrap();
        std::fs::write(&b, "@define-color theme_bg_color #333333;\n").unwrap();
        let vars = collect_vars(&[a, b]);
        assert_eq!(
            vars.get("theme_bg_color"),
            Some(&Color32::from_rgb(0x33, 0x33, 0x33))
        );
        assert_eq!(
            vars.get("only_a"),
            Some(&Color32::from_rgb(0x22, 0x22, 0x22))
        );
    }
}
