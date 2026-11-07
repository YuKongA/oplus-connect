use gpui_kit::component::{Theme as KitTheme, ThemeMode};
use gpui_kit::{App, Hsla, rgb, rgba};
use std::sync::atomic::{AtomicBool, Ordering};

/// Neutral palette aligned with GPUI Kit's default theme. Kit owns component
/// styling; the application only exposes the same neutral surfaces and text
/// colors for its custom layout.
#[allow(dead_code)]
pub struct Theme;

static THEME_IS_DARK: AtomicBool = AtomicBool::new(false);

impl Theme {
    pub fn init(cx: &mut App) {
        Self::install_ui_font(cx);
        Self::set_dark(false, cx);
    }

    /// Prefer one family covering Latin and CJK so fallback metrics cannot
    /// shift baselines inside a centred line box.
    fn install_ui_font(cx: &mut App) {
        const PREFERRED: [&str; 7] = [
            "MiSans VF",
            "MiSans",
            "OPPO Sans",
            "HarmonyOS Sans SC",
            "Noto Sans CJK SC",
            "Source Han Sans SC",
            "Microsoft YaHei UI",
        ];
        let available = cx.text_system().all_font_names();
        let Some(family) = PREFERRED
            .iter()
            .find(|family| available.iter().any(|name| name == *family))
        else {
            return;
        };
        KitTheme::global_mut(cx).font_family = (*family).into();
    }

    /// Switches Kit's built-in Light/Dark theme without overriding any token.
    pub fn set_dark(dark: bool, cx: &mut App) {
        THEME_IS_DARK.store(dark, Ordering::Relaxed);
        KitTheme::change(
            if dark {
                ThemeMode::Dark
            } else {
                ThemeMode::Light
            },
            None,
            cx,
        );
    }

    pub fn is_dark() -> bool {
        THEME_IS_DARK.load(Ordering::Relaxed)
    }

    pub fn bg_base() -> Hsla {
        if Self::is_dark() {
            rgb(0x0a0a0a).into()
        } else {
            rgb(0xffffff).into()
        }
    }

    pub fn bg_sidebar() -> Hsla {
        if Self::is_dark() {
            rgb(0x0a0a0a).into()
        } else {
            rgb(0xfafafa).into()
        }
    }

    pub fn bg_header() -> Hsla {
        if Self::is_dark() {
            rgb(0x0a0a0a).into()
        } else {
            rgb(0xf8f8f8).into()
        }
    }

    pub fn bg_card() -> Hsla {
        if Self::is_dark() {
            rgb(0x171717).into()
        } else {
            rgb(0xffffff).into()
        }
    }

    pub fn bg_card_hover() -> Hsla {
        if Self::is_dark() {
            rgb(0x262626).into()
        } else {
            rgb(0xf5f5f5).into()
        }
    }

    pub fn bg_card_selected() -> Hsla {
        if Self::is_dark() {
            rgb(0x262626).into()
        } else {
            rgb(0xf5f5f5).into()
        }
    }

    pub fn bg_input() -> Hsla {
        if Self::is_dark() {
            rgb(0x171717).into()
        } else {
            rgb(0xffffff).into()
        }
    }

    pub fn bg_modal_backdrop() -> Hsla {
        if Self::is_dark() {
            rgba(0x000000b0).into()
        } else {
            rgba(0x00000066).into()
        }
    }

    pub fn bg_modal() -> Hsla {
        if Self::is_dark() {
            rgb(0x0a0a0a).into()
        } else {
            rgb(0xffffff).into()
        }
    }

    pub fn primary() -> Hsla {
        if Self::is_dark() {
            rgb(0xfafafa).into()
        } else {
            rgb(0x171717).into()
        }
    }

    pub fn primary_hover() -> Hsla {
        if Self::is_dark() {
            rgb(0xf5f5f5).into()
        } else {
            rgb(0x262626).into()
        }
    }

    pub fn primary_active() -> Hsla {
        if Self::is_dark() {
            rgb(0xe5e5e5).into()
        } else {
            rgb(0x0a0a0a).into()
        }
    }

    pub fn primary_subtle() -> Hsla {
        if Self::is_dark() {
            rgba(0xffffff14).into()
        } else {
            rgba(0x17171714).into()
        }
    }

    pub fn brand_oneplus() -> Hsla {
        rgb(0xef4444).into()
    }

    pub fn brand_blue() -> Hsla {
        rgb(0x3b82f6).into()
    }

    pub fn brand_blue_subtle() -> Hsla {
        if Self::is_dark() {
            rgba(0x3b82f626).into()
        } else {
            rgba(0x3b82f61a).into()
        }
    }

    pub fn success() -> Hsla {
        if Self::is_dark() {
            rgb(0x4ade80).into()
        } else {
            rgb(0x16a34a).into()
        }
    }

    pub fn warning() -> Hsla {
        if Self::is_dark() {
            rgb(0xfacc15).into()
        } else {
            rgb(0xca8a04).into()
        }
    }

    pub fn warning_subtle() -> Hsla {
        rgba(0xeab30826).into()
    }

    pub fn danger() -> Hsla {
        if Self::is_dark() {
            rgb(0xf87171).into()
        } else {
            rgb(0xef4444).into()
        }
    }

    pub fn danger_subtle() -> Hsla {
        rgba(0xef444426).into()
    }

    pub fn text_primary() -> Hsla {
        if Self::is_dark() {
            rgb(0xfafafa).into()
        } else {
            rgb(0x0a0a0a).into()
        }
    }

    pub fn text_secondary() -> Hsla {
        if Self::is_dark() {
            rgb(0xa3a3a3).into()
        } else {
            rgb(0x525252).into()
        }
    }

    pub fn text_muted() -> Hsla {
        if Self::is_dark() {
            rgb(0x737373).into()
        } else {
            rgb(0x737373).into()
        }
    }

    pub fn text_on_accent() -> Hsla {
        if Self::is_dark() {
            rgb(0x171717).into()
        } else {
            rgb(0xfafafa).into()
        }
    }

    pub fn border_subtle() -> Hsla {
        if Self::is_dark() {
            rgba(0xffffff14).into()
        } else {
            rgba(0x0000000f).into()
        }
    }

    pub fn border_card() -> Hsla {
        if Self::is_dark() {
            rgb(0x262626).into()
        } else {
            rgb(0xe5e5e5).into()
        }
    }

    pub fn border_focused() -> Hsla {
        if Self::is_dark() {
            rgb(0x737373).into()
        } else {
            rgb(0xa3a3a3).into()
        }
    }
}
