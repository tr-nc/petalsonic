use egui::{Color32, Pos2, Rect, Stroke, Vec2};
use petalsonic_core::{
    SourceConfig,
    audio_data::PetalSonicAudioData,
    config::PetalSonicWorldDesc,
    engine::PetalSonicEngine,
    math::{Pose, Quat, Vec3},
    world::{PetalSonicWorld, SourceId},
};
use std::sync::Arc;

pub struct SpatialAudioDemo {
    world: Arc<PetalSonicWorld>,
    engine: PetalSonicEngine,
    source_id: SourceId,
    source_position: Vec3,
    grid_size: f32,
}

impl SpatialAudioDemo {
    pub fn new() -> Self {
        // Initialize logger
        env_logger::Builder::from_default_env()
            .filter_level(log::LevelFilter::Info)
            .init();

        // Create world description
        let world_desc = PetalSonicWorldDesc {
            sample_rate: 48000,
            block_size: 1024,
            ..Default::default()
        };

        // Create world
        let mut world =
            PetalSonicWorld::new(world_desc.clone()).expect("Failed to create PetalSonicWorld");

        // Set up listener pose at origin (0, 0, 0) with identity rotation
        let listener_pose = Pose::new(Vec3::new(0.0, 0.0, 0.0), Quat::IDENTITY);
        world.set_listener_pose(listener_pose);
        log::info!("Listener pose set to origin");

        // Load audio file and create a spatial source
        let wav_path = "res/cicada_test_96k.wav";
        let audio_data = PetalSonicAudioData::from_path(wav_path)
            .expect("Failed to load audio file. Make sure res/cicada_test_96k.wav exists.");

        let initial_position = Vec3::new(0.0, 0.0, -1.0); // 1 meter in front
        let source_id = world
            .add_source(
                audio_data,
                SourceConfig::spatial_with_volume(initial_position, 1.0),
            )
            .expect("Failed to add audio source");

        log::info!(
            "Spatial audio source added at position {:?}",
            initial_position
        );

        // Create engine
        let world_arc = Arc::new(world);
        let mut engine =
            PetalSonicEngine::new(world_desc, world_arc.clone()).expect("Failed to create engine");

        // Start the engine
        engine.start().expect("Failed to start audio engine");
        log::info!("Audio engine started");

        // Start playback
        world_arc.play(source_id).expect("Failed to start playback");
        log::info!("Playback started");

        Self {
            world: world_arc,
            engine,
            source_id,
            source_position: initial_position,
            grid_size: 5.0, // 5 meter grid
        }
    }

    fn world_to_screen(&self, world_pos: Vec3, rect: Rect) -> Pos2 {
        // Convert world coordinates to screen coordinates
        // World: X right, Z forward (up on screen), origin at center
        // Screen: origin at top-left
        let center = rect.center();
        let scale = rect.width().min(rect.height()) / (self.grid_size * 2.0);

        Pos2::new(
            center.x + world_pos.x * scale,
            center.y - world_pos.z * scale, // Negative because screen Y goes down
        )
    }

    fn screen_to_world(&self, screen_pos: Pos2, rect: Rect) -> Vec3 {
        // Convert screen coordinates to world coordinates
        let center = rect.center();
        let scale = rect.width().min(rect.height()) / (self.grid_size * 2.0);

        Vec3::new(
            (screen_pos.x - center.x) / scale,
            0.0,                                // Keep Y at 0 (top-down view)
            -(screen_pos.y - center.y) / scale, // Negative because screen Y goes down
        )
    }

    fn draw_grid(&self, ui: &mut egui::Ui, rect: Rect) {
        let painter = ui.painter();

        // Draw grid lines
        let grid_step = 1.0; // 1 meter per grid line
        let num_lines = (self.grid_size / grid_step) as i32;

        for i in -num_lines..=num_lines {
            let offset = i as f32 * grid_step;

            // Vertical lines (constant X)
            let top = self.world_to_screen(Vec3::new(offset, 0.0, self.grid_size), rect);
            let bottom = self.world_to_screen(Vec3::new(offset, 0.0, -self.grid_size), rect);
            painter.line_segment([top, bottom], Stroke::new(1.0, Color32::from_gray(80)));

            // Horizontal lines (constant Z)
            let left = self.world_to_screen(Vec3::new(-self.grid_size, 0.0, offset), rect);
            let right = self.world_to_screen(Vec3::new(self.grid_size, 0.0, offset), rect);
            painter.line_segment([left, right], Stroke::new(1.0, Color32::from_gray(80)));
        }

        // Draw axes (thicker, colored)
        let origin = self.world_to_screen(Vec3::ZERO, rect);
        let x_axis_end = self.world_to_screen(Vec3::new(self.grid_size, 0.0, 0.0), rect);
        let z_axis_end = self.world_to_screen(Vec3::new(0.0, 0.0, self.grid_size), rect);

        painter.line_segment(
            [origin, x_axis_end],
            Stroke::new(2.0, Color32::from_rgb(255, 100, 100)), // X axis - red
        );
        painter.line_segment(
            [origin, z_axis_end],
            Stroke::new(2.0, Color32::from_rgb(100, 100, 255)), // Z axis - blue
        );
    }

    fn draw_listener(&self, ui: &mut egui::Ui, rect: Rect) {
        let painter = ui.painter();
        let listener_pos = self.world_to_screen(Vec3::ZERO, rect);

        // Draw red circle for listener
        painter.circle_filled(listener_pos, 8.0, Color32::from_rgb(255, 50, 50));
        painter.circle_stroke(listener_pos, 8.0, Stroke::new(2.0, Color32::WHITE));

        // Draw label
        painter.text(
            listener_pos + Vec2::new(0.0, -15.0),
            egui::Align2::CENTER_BOTTOM,
            "Listener",
            egui::FontId::proportional(14.0),
            Color32::WHITE,
        );
    }

    fn draw_source(&self, ui: &mut egui::Ui, rect: Rect) {
        let painter = ui.painter();
        let source_pos = self.world_to_screen(self.source_position, rect);

        // Draw blue circle for source
        painter.circle_filled(source_pos, 8.0, Color32::from_rgb(50, 150, 255));
        painter.circle_stroke(source_pos, 8.0, Stroke::new(2.0, Color32::WHITE));

        // Draw label with distance
        let distance = self.source_position.length();
        let label = format!("Source ({:.1}m)", distance);
        painter.text(
            source_pos + Vec2::new(0.0, 15.0),
            egui::Align2::CENTER_TOP,
            label,
            egui::FontId::proportional(14.0),
            Color32::WHITE,
        );
    }

    fn handle_mouse_click(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let response = ui.allocate_rect(rect, egui::Sense::click());

        if response.clicked() {
            if let Some(pos) = response.interact_pointer_pos() {
                // Convert screen position to world position
                let new_world_pos = self.screen_to_world(pos, rect);

                // Clamp to grid bounds
                let clamped_pos = Vec3::new(
                    new_world_pos.x.clamp(-self.grid_size, self.grid_size),
                    0.0,
                    new_world_pos.z.clamp(-self.grid_size, self.grid_size),
                );

                // Update source position
                self.source_position = clamped_pos;

                // Update config in world (which sends command to audio engine)
                let new_config = SourceConfig::spatial_with_volume(clamped_pos, 1.0);
                if let Err(e) = self.world.update_source_config(self.source_id, new_config) {
                    log::error!("Failed to update source config: {}", e);
                }

                log::info!("Source position updated to {:?}", clamped_pos);
            }
        }
    }
}

impl eframe::App for SpatialAudioDemo {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("PetalSonic Spatial Audio Demo");
            ui.label("Click anywhere on the grid to move the audio source");
            ui.separator();

            // Allocate space for the visualization
            let available_size = ui.available_size();
            let size = available_size.x.min(available_size.y) - 20.0;
            let rect =
                Rect::from_center_size(ui.available_rect_before_wrap().center(), Vec2::splat(size));

            // Draw the grid and elements
            self.draw_grid(ui, rect);
            self.draw_listener(ui, rect);
            self.draw_source(ui, rect);

            // Handle mouse input
            self.handle_mouse_click(ui, rect);

            ui.separator();
            ui.label(format!(
                "Source Position: ({:.2}, {:.2}, {:.2})",
                self.source_position.x, self.source_position.y, self.source_position.z
            ));
        });

        // Request continuous repaint for smooth interaction
        ctx.request_repaint();
    }
}

impl Drop for SpatialAudioDemo {
    fn drop(&mut self) {
        log::info!("Shutting down audio engine");
        let _ = self.engine.stop();
    }
}
