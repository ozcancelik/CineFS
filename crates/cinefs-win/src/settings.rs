//! Persisted user preferences (`settings.ini` in the CineFS data directory).
//!
//! A tiny `key=value` file so the app keeps its choices across launches without pulling in a
//! serialization crate. Unknown keys and malformed lines are ignored; missing keys keep defaults.
//! UI-free and platform-neutral so it can be unit-tested anywhere.

use cinefs_core::DngPayloadMode;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub mode: DngPayloadMode,
    /// Open the mounted volume in Explorer right after mounting (off: the CineFS window is the hub).
    pub auto_open_explorer: bool,
    pub stretch_bake: bool,
    pub focus_fix: bool,
    /// 0 = Auto (from RAWC), 1 = off, 2 = horizontal x3, 3 = vertical x1.67, 4 = vertical x3
    pub stretch_mode: u8,
    /// Player output volume 0..1 and mute, shared by every player window.
    pub player_volume: f32,
    pub player_muted: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: DngPayloadMode::Auto,
            auto_open_explorer: false,
            stretch_bake: true,
            focus_fix: true,
            stretch_mode: 0,
            player_volume: 1.0,
            player_muted: false,
        }
    }
}

fn mode_key(m: DngPayloadMode) -> &'static str {
    match m {
        DngPayloadMode::Auto => "auto",
        DngPayloadMode::Lj92Passthrough => "passthrough",
        DngPayloadMode::NativePacked => "packed",
        DngPayloadMode::Unpacked16 => "unpacked16",
    }
}

fn parse_mode(s: &str) -> Option<DngPayloadMode> {
    match s {
        "auto" => Some(DngPayloadMode::Auto),
        "passthrough" => Some(DngPayloadMode::Lj92Passthrough),
        "packed" => Some(DngPayloadMode::NativePacked),
        "unpacked16" => Some(DngPayloadMode::Unpacked16),
        _ => None,
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "true" | "on" => Some(true),
        "0" | "false" | "off" => Some(false),
        _ => None,
    }
}

impl Settings {
    pub fn parse(text: &str) -> Self {
        let mut s = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "mode" => {
                    if let Some(m) = parse_mode(v) {
                        s.mode = m;
                    }
                }
                "auto_open_explorer" => {
                    if let Some(b) = parse_bool(v) {
                        s.auto_open_explorer = b;
                    }
                }
                "stretch_bake" => {
                    if let Some(b) = parse_bool(v) {
                        s.stretch_bake = b;
                    }
                }
                "focus_fix" => {
                    if let Some(b) = parse_bool(v) {
                        s.focus_fix = b;
                    }
                }
                "stretch_mode" => {
                    if let Ok(n) = v.parse::<u8>() {
                        if n <= 4 {
                            s.stretch_mode = n;
                        }
                    }
                }
                "player_volume" => {
                    if let Ok(v) = v.parse::<f32>() {
                        if v.is_finite() {
                            s.player_volume = v.clamp(0.0, 1.0);
                        }
                    }
                }
                "player_muted" => {
                    if let Some(b) = parse_bool(v) {
                        s.player_muted = b;
                    }
                }
                _ => {}
            }
        }
        s
    }

    pub fn to_text(&self) -> String {
        format!(
            "# CineFS preferences (defaults for clips mounted from now on)\n\
             mode={}\nauto_open_explorer={}\nstretch_bake={}\nfocus_fix={}\nstretch_mode={}\n\
             player_volume={:.3}\nplayer_muted={}\n",
            mode_key(self.mode),
            self.auto_open_explorer as u8,
            self.stretch_bake as u8,
            self.focus_fix as u8,
            self.stretch_mode,
            self.player_volume,
            self.player_muted as u8,
        )
    }

    /// Loads `path`; a missing or unreadable file yields the defaults.
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path).map(|t| Self::parse(&t)).unwrap_or_default()
    }

    /// Writes atomically (temp file + rename) so a crash never leaves a half-written file.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("ini.tmp");
        fs::write(&tmp, self.to_text())?;
        fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let s = Settings {
            mode: DngPayloadMode::Unpacked16,
            auto_open_explorer: true,
            stretch_bake: false,
            focus_fix: false,
            stretch_mode: 3,
            player_volume: 0.25,
            player_muted: true,
        };
        assert_eq!(Settings::parse(&s.to_text()), s);
    }

    #[test]
    fn missing_and_bad_values_keep_defaults() {
        let s = Settings::parse("mode=bogus\nstretch_mode=9\nfocus_fix=maybe\nnot a line\nauto_open_explorer=1\n");
        let d = Settings::default();
        assert_eq!(s.mode, d.mode);
        assert_eq!(s.stretch_mode, d.stretch_mode);
        assert_eq!(s.focus_fix, d.focus_fix);
        assert!(s.auto_open_explorer);
    }

    #[test]
    fn save_and_load_file() {
        let dir = std::env::temp_dir().join(format!("cinefs-settings-test-{}", std::process::id()));
        let path = dir.join("settings.ini");
        let s = Settings { focus_fix: false, stretch_mode: 2, ..Settings::default() };
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path), s);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_gives_defaults() {
        assert_eq!(Settings::load(Path::new("/definitely/not/here/settings.ini")), Settings::default());
    }
}
