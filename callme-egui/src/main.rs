#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")] // hide console window on Windows in release
#![allow(rustdoc::missing_crate_level_docs)] // it's an example

// use eframe::egui;

// fn main() -> eframe::Result {
//     env_logger::init(); // Log to stderr (if you run with `RUST_LOG=debug`).
//     let options = eframe::NativeOptions {
//         viewport: egui::ViewportBuilder::default().with_inner_size([320.0, 240.0]),
//         ..Default::default()
//     };
//     eframe::run_native(
//         "My egui App",
//         options,
//         Box::new(|cc| {
//             // This gives us image support:
//             egui_extras::install_image_loaders(&cc.egui_ctx);

//             Ok(Box::<MyApp>::default())
//         }),
//     )
// }

use callme_egui::app::App;
use eframe::NativeOptions;

fn main() -> Result<(), eframe::Error> {
    tracing_subscriber::fmt::init();
    let options = NativeOptions::default();
    App::run(options)
}
