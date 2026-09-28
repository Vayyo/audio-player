//! Local audio player: native controls and a live default-device output stream.

pub mod core;
mod ui;

fn main() -> eframe::Result {
    let initial = std::env::args_os().nth(1).map(std::path::PathBuf::from);
    eframe::run_native(
        "Rap",
        eframe::NativeOptions {
            viewport: eframe::egui::ViewportBuilder::default()
                .with_app_id("rap")
                .with_inner_size([806.0, 580.0])
                .with_min_inner_size([800.0, 550.0]),
            ..Default::default()
        },
        Box::new(move |context| {
            let mut app = ui::AudioApp::new(initial);
            #[cfg(target_os = "linux")]
            app.start_tray(&context.egui_ctx);
            Ok(Box::new(app))
        }),
    )
}
