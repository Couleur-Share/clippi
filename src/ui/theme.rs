//! Theme system for Clippi — color tokens matching the original Slint design.
//!
//! --- All colors are sourced from `ui/app.slint` and sub-components. ---
//! --- Tokens adapt between dark and light mode. ---

use gpui::{rgb, rgba, Rgba, WindowAppearance};

/// Semantic color palette for the Clippi UI.
/// Colors match the original Slint design pixel-for-pixel.
#[derive(Debug, Clone)]
pub struct ClippiTheme {
    // --- Window chrome ---
    pub bg: Rgba,
    pub surface: Rgba,
    pub surface_press: Rgba,
    pub titlebar_bg: Rgba,

    // --- Text ---
    pub text_1: Rgba,
    pub text_2: Rgba,
    pub text_3: Rgba,

    // --- Accent ---
    pub accent: Rgba,
    pub accent_soft: Rgba,

    // --- Semantic ---
    pub fav_color: Rgba,
    pub danger: Rgba,
    pub divider: Rgba,
    pub btn_hover: Rgba,
    /// Transfer-station pinned state accent (blue), also used by the pin
    /// toolbar button and the pinned card bar.
    pub transfer_pin_color: Rgba,

    // --- Tags ---
    pub tag_bg: Rgba,
    pub tag_text: Rgba,

    // --- Panels (floating) ---
    pub panel_surface: Rgba,
    pub panel_input_bg: Rgba,
    pub panel_sep_line: Rgba,

    // --- Toast ---
    pub toast_bg: Rgba,
}

impl ClippiTheme {
    pub fn dark() -> Self {
        Self {
            bg: rgb(0x191a1b),
            surface: rgb(0x232425),
            surface_press: rgb(0x3a3b3c),
            titlebar_bg: rgb(0x191a1b),

            text_1: rgb(0xeaebec),
            text_2: rgb(0x919496),
            text_3: rgb(0x5f6264),

            accent: rgb(0x7ecba3),
            accent_soft: rgb(0x1a2e24),

            fav_color: rgb(0xd8a155),
            danger: rgb(0xff5f57),
            divider: rgb(0x2b2c2d),
            btn_hover: rgb(0x2b2c2d),
            transfer_pin_color: rgb(0x3B82F6),

            tag_bg: rgb(0x2c2e2f),
            tag_text: rgb(0xe4e9e6),

            panel_surface: rgb(0x2c2d2e),
            panel_input_bg: rgb(0x1e1f20),
            panel_sep_line: rgba(0xffffff0d),

            toast_bg: rgba(0x3a3b3de8),
        }
    }

    pub fn light() -> Self {
        Self {
            bg: rgb(0xf2f3f8),
            surface: rgb(0xffffff),
            surface_press: rgb(0xebedf5),
            titlebar_bg: rgb(0xf2f3f8),

            text_1: rgb(0x1a1c2e),
            text_2: rgb(0x7c809a),
            text_3: rgb(0xb4b9ca),

            accent: rgb(0x6ab890),
            accent_soft: rgb(0xe8f5ee),

            fav_color: rgb(0xd8a155),
            danger: rgb(0xff5f57),
            divider: rgb(0xe6e8f0),
            btn_hover: rgb(0xf0f1f8),
            transfer_pin_color: rgb(0x3B82F6),

            tag_bg: rgb(0xe6e8f0),
            tag_text: rgb(0x767e7a),

            panel_surface: rgb(0xffffff),
            panel_input_bg: rgb(0xf5f6fc),
            panel_sep_line: rgba(0x00000010),

            toast_bg: rgba(0xfffffff0),
        }
    }

    pub fn from_settings(
        settings: &crate::core::settings::AppSettings,
        appearance: Option<WindowAppearance>,
    ) -> Self {
        let mut theme = Self::from_setting(&settings.theme, appearance);
        if settings.theme_color == "system" {
            if let Some(color) = *SYSTEM_ACCENT.lock().expect("system accent cache poisoned") {
                theme.accent = color;
                let weight = if theme.bg == rgb(0x191a1b) {
                    0.16
                } else {
                    0.12
                };
                theme.accent_soft = Rgba {
                    r: theme.bg.r * (1.0 - weight) + color.r * weight,
                    g: theme.bg.g * (1.0 - weight) + color.g * weight,
                    b: theme.bg.b * (1.0 - weight) + color.b * weight,
                    a: 1.0,
                };
            }
        }
        theme
    }

    pub fn sync_component_accent(&self, cx: &mut gpui::App) {
        let global = gpui_component::Theme::global_mut(cx);
        global.primary = self.accent.into();
        global.primary_hover = self.accent.into();
        global.primary_active = self.accent.into();
        global.selection = self.accent_overlay().into();
        global.ring = self.accent.into();
    }

    pub fn from_setting(theme: &str, appearance: Option<WindowAppearance>) -> Self {
        match theme {
            "light" => Self::light(),
            "dark" => Self::dark(),
            _ => match appearance {
                Some(WindowAppearance::Dark | WindowAppearance::VibrantDark) => Self::dark(),
                _ => Self::light(),
            },
        }
    }

    /// Accent color as a 13% opacity overlay (used for active filter button backgrounds).
    pub fn accent_overlay(&self) -> Rgba {
        Rgba {
            a: 0x22 as f32 / 255.0,
            ..self.accent
        }
    }

    /// Accent color used behind search hits. Stronger than the generic
    /// overlay so short highlighted terms remain visible in dense previews.
    pub fn accent_highlight(&self) -> Rgba {
        if self.bg == rgb(0x191a1b) {
            Rgba {
                a: 0.4,
                ..self.accent
            }
        } else {
            Rgba {
                a: 0x4d as f32 / 255.0,
                ..self.accent
            }
        }
    }

    pub fn accent_highlight_text(&self) -> Rgba {
        if self.bg == rgb(0x191a1b) {
            rgb(0xffffff)
        } else {
            rgb(0x000000)
        }
    }
}

static SYSTEM_ACCENT: std::sync::Mutex<Option<Rgba>> = std::sync::Mutex::new(None);

/// Called on the GPUI main thread at startup and by the existing poll loop.
pub fn refresh_system_accent() -> bool {
    static LAST_READ: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    let mut last = LAST_READ.lock().expect("system accent timer poisoned");
    if last.is_some_and(|time| time.elapsed() < std::time::Duration::from_secs(1)) {
        return false;
    }
    *last = Some(std::time::Instant::now());
    let color = crate::platform::accent::read_system_accent();
    let mut cached = SYSTEM_ACCENT.lock().expect("system accent cache poisoned");
    let changed = *cached != color;
    *cached = color;
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_color_old_settings_default_and_round_trip() {
        let settings = crate::core::settings::AppSettings::default();
        let mut value = serde_json::to_value(settings).unwrap();
        value.as_object_mut().unwrap().remove("theme_color");
        let mut loaded: crate::core::settings::AppSettings = serde_json::from_value(value).unwrap();
        assert_eq!(loaded.theme_color, "default");
        loaded.theme_color = "system".into();
        let saved = toml::to_string(&loaded).unwrap();
        let restored: crate::core::settings::AppSettings = toml::from_str(&saved).unwrap();
        assert_eq!(restored.theme_color, "system");
    }

    #[test]
    fn overlays_follow_accent_and_preserve_default_colors() {
        assert_eq!(ClippiTheme::dark().accent_overlay(), rgba(0x7ecba322));
        assert_eq!(ClippiTheme::light().accent_highlight(), rgba(0x6ab8904d));
        for mut theme in [ClippiTheme::light(), ClippiTheme::dark()] {
            theme.accent = rgb(0x0067c0);
            let overlay = theme.accent_overlay();
            let highlight = theme.accent_highlight();
            assert_eq!(
                (overlay.r, overlay.g, overlay.b),
                (theme.accent.r, theme.accent.g, theme.accent.b)
            );
            assert_eq!(
                (highlight.r, highlight.g, highlight.b),
                (theme.accent.r, theme.accent.g, theme.accent.b)
            );
        }
    }
}
