//! Lucide icons (ISC, assets/icons/LICENSE-lucide.txt): embedded SVGs recoloured to white,
//! rasterized by egui_extras at the exact on-screen size and tinted per use.
//! Never Unicode glyphs as icons: they look different on every platform.

use std::collections::HashMap;
use std::sync::OnceLock;

use egui::{Color32, Rect, Vec2};

macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        &[$(($name, include_str!(concat!("../assets/icons/", $name, ".svg")))),*]
    };
}

static SOURCES: &[(&str, &str)] = icons![
    "pin",
    "x",
    "plus",
    "ellipsis",
    "type",
    "eye",
    "eye-off",
    "files",
    "download",
    "trash-2",
    "copy",
    "save",
    "lock",
    "lock-open",
    "screen-share",
    "screen-share-off",
    "triangle-alert"
];

fn white() -> &'static HashMap<&'static str, &'static [u8]> {
    static MAP: OnceLock<HashMap<&'static str, &'static [u8]>> = OnceLock::new();
    MAP.get_or_init(|| {
        SOURCES
            .iter()
            .map(|(name, svg)| {
                let bytes: &'static [u8] = Box::leak(svg.replace("currentColor", "#ffffff").into_bytes().into_boxed_slice());
                (*name, bytes)
            })
            .chain(std::iter::once({
                // The pinned state uses a solid pin, like a pushed-in thumbtack.
                let pin = SOURCES.iter().find(|(n, _)| *n == "pin").unwrap().1;
                let solid = pin.replace("currentColor", "#ffffff").replace("fill=\"none\"", "fill=\"#ffffff\"");
                ("pin-filled", &*Box::leak(solid.into_bytes().into_boxed_slice()))
            }))
            .collect()
    })
}

pub fn image(name: &str, size: f32, tint: Color32) -> egui::Image<'static> {
    let bytes = white().get(name).copied().unwrap_or_else(|| panic!("unknown icon {name}"));
    egui::Image::from_bytes(format!("bytes://icon/{name}.svg"), bytes).fit_to_exact_size(Vec2::splat(size)).tint(tint)
}

pub fn paint(ui: &egui::Ui, rect: Rect, name: &str, size: f32, tint: Color32) {
    image(name, size, tint).paint_at(ui, Rect::from_center_size(rect.center(), Vec2::splat(size)));
}
