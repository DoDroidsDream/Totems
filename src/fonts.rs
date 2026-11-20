//! Use the platform UI font (Segoe UI on Windows, like native apps) and add
//! fallbacks so file names in any script render.

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};
use eframe::epaint::text::{FallbackRequest, FontInsert, FontPriority, FontProvider, InsertFontFamily};
use std::sync::{Arc, OnceLock};

fn read(paths: &[&str]) -> Option<Vec<u8>> {
    paths.iter().find_map(|p| std::fs::read(p).ok())
}

/// A large fallback font (the CJK ones are 13–20 MB each), read from disk
/// the first time a character none of the installed fonts has is drawn.
/// Most libraries never show a CJK name, so this keeps ~47 MB off the heap.
struct LazyFont {
    name: String,
    path: &'static str,
    data: OnceLock<Option<FontData>>,
}

impl FontProvider for LazyFont {
    fn font_for(&self, _: &FallbackRequest<'_>) -> Option<FontInsert> {
        let data = self.data.get_or_init(|| std::fs::read(self.path).ok().map(FontData::from_owned)).clone()?;
        // egui checks the glyph and asks the next provider if it's missing,
        // so the fallback order is kept.
        let families = [FontFamily::Proportional, FontFamily::Name("bold".into()), FontFamily::Name("light".into())]
            .into_iter()
            .map(|family| InsertFontFamily { family, priority: FontPriority::Lowest })
            .collect();
        Some(FontInsert::new(&self.name, data, families))
    }
}

/// Installs fonts; returns true if a bold face is available.
pub fn install(ctx: &egui::Context) -> bool {
    let mut defs = FontDefinitions::default();

    // `fallbacks` load at startup; `lazy` ones (CJK) on first use, in order.
    #[cfg(windows)]
    let (regular, bold, fallbacks, lazy): (&[&str], &[&str], &[&str], &[&'static str]) = (
        &["C:\\Windows\\Fonts\\segoeui.ttf"],
        // Semibold, not Bold: Bold read too thick and bright for matches.
        &["C:\\Windows\\Fonts\\seguisb.ttf", "C:\\Windows\\Fonts\\segoeuib.ttf"],
        &["C:\\Windows\\Fonts\\seguisym.ttf"],
        &["C:\\Windows\\Fonts\\msyh.ttc", "C:\\Windows\\Fonts\\YuGothM.ttc", "C:\\Windows\\Fonts\\malgun.ttf"],
    );
    #[cfg(target_os = "macos")]
    let (regular, bold, fallbacks, lazy): (&[&str], &[&str], &[&str], &[&'static str]) = (
        &["/System/Library/Fonts/Supplemental/Arial.ttf"],
        &["/System/Library/Fonts/Supplemental/Arial Bold.ttf"],
        &[],
        &["/System/Library/Fonts/Hiragino Sans GB.ttc", "/System/Library/Fonts/AppleSDGothicNeo.ttc"],
    );
    #[cfg(all(unix, not(target_os = "macos")))]
    let (regular, bold, fallbacks, lazy): (&[&str], &[&str], &[&str], &[&'static str]) = (
        &[
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        ],
        &[
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans-Bold.ttf",
        ],
        &[],
        &[
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        ],
    );

    let mut prop: Vec<String> = Vec::new();
    if let Some(b) = read(regular) {
        defs.font_data.insert("ui".into(), Arc::new(FontData::from_owned(b)));
        prop.push("ui".into());
    }
    let defaults = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    for (i, p) in fallbacks.iter().enumerate() {
        if let Ok(b) = std::fs::read(p) {
            let key = format!("fallback{i}");
            defs.font_data.insert(key.clone(), Arc::new(FontData::from_owned(b)));
            prop.push(key);
        }
    }
    let mut family = prop.clone();
    family.extend(defaults.iter().cloned());
    // Keep the bundled font for symbols (▶ ⏸ ⏶) after the UI font.
    if !prop.is_empty() {
        family.retain(|f| f != "ui");
        family.insert(0, "ui".into());
    }
    defs.families.insert(FontFamily::Proportional, family.clone());

    let has_bold = if let Some(b) = read(bold) {
        defs.font_data.insert("ui-bold".into(), Arc::new(FontData::from_owned(b)));
        let mut bf = vec!["ui-bold".to_string()];
        bf.extend(family.iter().filter(|f| *f != "ui").cloned());
        defs.families.insert(FontFamily::Name("bold".into()), bf);
        true
    } else {
        false
    };
    // A lighter face for buttons and window titles: regular Segoe UI reads
    // too bold there, light on dark.
    #[cfg(windows)]
    let light: &[&str] = &["C:\\Windows\\Fonts\\segoeuisl.ttf"];
    #[cfg(not(windows))]
    let light: &[&str] = &[];
    let mut lf = family.clone();
    if let Some(b) = read(light) {
        defs.font_data.insert("ui-light".into(), Arc::new(FontData::from_owned(b)));
        lf.retain(|f| f != "ui");
        lf.insert(0, "ui-light".into());
    }
    defs.families.insert(FontFamily::Name("light".into()), lf);

    ctx.set_fonts(defs);
    for (i, path) in lazy.iter().enumerate() {
        ctx.add_font_provider(Arc::new(LazyFont { name: format!("lazy{i}"), path, data: OnceLock::new() }));
    }
    has_bold
}
