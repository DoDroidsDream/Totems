#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod decode;
mod dsp;
mod fonts;
mod index;
mod indexer;
mod player;
mod playlist;
mod query;
mod region;
mod workers;

use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(app::APP_NAME)
            .with_inner_size([1100.0, 680.0])
            .with_min_inner_size([420.0, 240.0]),
        ..Default::default()
    };
    eframe::run_native(app::APP_NAME, options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}
