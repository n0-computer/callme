#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release
#![allow(rustdoc::missing_crate_level_docs)] // it's an example

use callme_egui::app::App;
use eframe::NativeOptions;

fn main() -> Result<(), eframe::Error> {
    tracing_subscriber::fmt::init();
    let mut options = NativeOptions::default();
    options.viewport = options
        .viewport
        .with_title("Callme")
        .with_resizable(true)
        .with_inner_size([500., 600.]);
    App::run(options)
}
