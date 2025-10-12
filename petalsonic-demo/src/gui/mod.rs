mod app;
pub mod profiling;

pub use app::SpatialAudioDemo;

/// Run the GUI demo
pub fn run() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([800.0, 600.0])
            .with_title("PetalSonic Spatial Audio Demo"),
        ..Default::default()
    };

    eframe::run_native(
        "PetalSonic Spatial Audio Demo",
        options,
        Box::new(|_cc| Ok(Box::new(SpatialAudioDemo::new()))),
    )
}
