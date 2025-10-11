use egui::{Color32, Pos2, Rect, Stroke, Vec2};
use petalsonic_core::{
    SourceConfig,
    audio_data::PetalSonicAudioData,
    config::PetalSonicWorldDesc,
    engine::PetalSonicEngine,
    math::{Pose, Quat, Vec3},
    playback::LoopMode,
    world::{PetalSonicWorld, SourceId},
};
use std::sync::Arc;

#[derive(Clone)]
struct AudioSource {
    id: SourceId,
    position: Vec3,
    file_name: String,
    loop_mode: LoopMode,
}

pub struct SpatialAudioDemo {
    world: Arc<PetalSonicWorld>,
    engine: PetalSonicEngine,
    sources: Vec<AudioSource>,
    grid_size: f32,

    // UI state
    available_audio_files: Vec<String>,
    selected_audio_file_index: usize,
    selected_loop_mode_index: usize,
    add_source_mode: bool,
    dragging_source_index: Option<usize>,
}

impl SpatialAudioDemo {
    pub fn new() -> Self {
        // Initialize logger
        env_logger::Builder::from_default_env()
            .filter_level(log::LevelFilter::Info)
            .init();

        // Scan available audio files
        let available_audio_files = Self::scan_audio_files();
        if available_audio_files.is_empty() {
            log::warn!("No audio files found in petalsonic-demo/asset/sound/");
        }

        // Create world description
        let world_desc = PetalSonicWorldDesc {
            sample_rate: 48000,
            block_size: 1024,
            hrtf_path: Some("petalsonic-demo/asset/hrtf/hrtf_b_nh172.sofa".to_string()),
            ..Default::default()
        };

        // Create world
        let world =
            PetalSonicWorld::new(world_desc.clone()).expect("Failed to create PetalSonicWorld");

        // Set up listener pose at origin (0, 0, 0) with identity rotation
        let listener_pose = Pose::new(Vec3::new(0.0, 0.0, 0.0), Quat::IDENTITY);
        world.set_listener_pose(listener_pose);
        log::info!("Listener pose set to origin");

        // Create engine
        let world_arc = Arc::new(world);
        let mut engine =
            PetalSonicEngine::new(world_desc, world_arc.clone()).expect("Failed to create engine");

        // Start the engine
        engine.start().expect("Failed to start audio engine");
        log::info!("Audio engine started");

        Self {
            world: world_arc,
            engine,
            sources: Vec::new(),
            grid_size: 5.0,
            available_audio_files,
            selected_audio_file_index: 0,
            selected_loop_mode_index: 0, // Once
            add_source_mode: false,
            dragging_source_index: None,
        }
    }

    fn scan_audio_files() -> Vec<String> {
        let audio_dir = "petalsonic-demo/asset/sound";
        let mut files = Vec::new();

        if let Ok(entries) = std::fs::read_dir(audio_dir) {
            for entry in entries.flatten() {
                if let Some(file_name) = entry.file_name().to_str() {
                    if file_name.ends_with(".wav")
                        || file_name.ends_with(".mp3")
                        || file_name.ends_with(".ogg")
                    {
                        files.push(file_name.to_string());
                    }
                }
            }
        }

        files.sort();
        files
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

    fn draw_sources(&self, ui: &mut egui::Ui, rect: Rect) {
        let painter = ui.painter();

        for (_idx, source) in self.sources.iter().enumerate() {
            let source_pos = self.world_to_screen(source.position, rect);

            // Draw blue circle for source
            painter.circle_filled(source_pos, 8.0, Color32::from_rgb(50, 150, 255));
            painter.circle_stroke(source_pos, 8.0, Stroke::new(2.0, Color32::WHITE));

            // Draw label with file name and distance
            let distance = source.position.length();
            let label = format!(
                "{} ({:.1}m)",
                source.file_name.trim_end_matches(".wav"),
                distance
            );
            painter.text(
                source_pos + Vec2::new(0.0, 15.0),
                egui::Align2::CENTER_TOP,
                label,
                egui::FontId::proportional(12.0),
                Color32::WHITE,
            );
        }
    }

    fn handle_mouse_interaction(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());

        // Handle click to add source
        if self.add_source_mode && response.clicked() {
            if let Some(pos) = response.interact_pointer_pos() {
                let world_pos = self.screen_to_world(pos, rect);
                let clamped_pos = Vec3::new(
                    world_pos.x.clamp(-self.grid_size, self.grid_size),
                    0.0,
                    world_pos.z.clamp(-self.grid_size, self.grid_size),
                );

                if let Err(e) = self.add_source_at_position(clamped_pos) {
                    log::error!("Failed to add source: {}", e);
                }

                self.add_source_mode = false;
            }
            return;
        }

        // Handle dragging existing sources
        if response.drag_started() {
            if let Some(pos) = response.interact_pointer_pos() {
                // Find which source was clicked
                for (idx, source) in self.sources.iter().enumerate() {
                    let source_screen_pos = self.world_to_screen(source.position, rect);
                    let dist = ((pos.x - source_screen_pos.x).powi(2)
                        + (pos.y - source_screen_pos.y).powi(2))
                    .sqrt();
                    if dist < 15.0 {
                        // Click tolerance
                        self.dragging_source_index = Some(idx);
                        log::info!("Started dragging source {}", idx);
                        break;
                    }
                }
            }
        }

        if response.dragged() {
            if let Some(idx) = self.dragging_source_index {
                if let Some(pos) = response.interact_pointer_pos() {
                    let new_world_pos = self.screen_to_world(pos, rect);
                    let clamped_pos = Vec3::new(
                        new_world_pos.x.clamp(-self.grid_size, self.grid_size),
                        0.0,
                        new_world_pos.z.clamp(-self.grid_size, self.grid_size),
                    );

                    if let Some(source) = self.sources.get_mut(idx) {
                        source.position = clamped_pos;
                        let new_config = SourceConfig::spatial_with_volume(clamped_pos, 1.0);
                        if let Err(e) = self.world.update_source_config(source.id, new_config) {
                            log::error!("Failed to update source config: {}", e);
                        }
                    }
                }
            }
        }

        if response.drag_stopped() {
            if let Some(idx) = self.dragging_source_index {
                log::info!("Stopped dragging source {}", idx);
                self.dragging_source_index = None;
            }
        }
    }

    fn add_source_at_position(&mut self, position: Vec3) -> Result<(), String> {
        if self.available_audio_files.is_empty() {
            return Err("No audio files available".to_string());
        }

        let file_name = &self.available_audio_files[self.selected_audio_file_index];
        let file_path = format!("petalsonic-demo/asset/sound/{}", file_name);

        log::info!("GUI: Loading audio file: {}", file_path);

        let audio_data = PetalSonicAudioData::from_path(&file_path)
            .map_err(|e| format!("Failed to load audio file: {}", e))?;

        log::debug!(
            "GUI: Audio loaded - {} samples at {} Hz",
            audio_data.samples().len(),
            audio_data.sample_rate()
        );

        let source_id = self
            .world
            .register_audio(audio_data, SourceConfig::spatial_with_volume(position, 1.0))
            .map_err(|e| format!("Failed to register audio in world: {}", e))?;

        log::debug!("GUI: Audio registered with source ID: {}", source_id);

        let loop_mode = match self.selected_loop_mode_index {
            0 => LoopMode::Once,
            1 => LoopMode::Infinite,
            _ => LoopMode::Once,
        };

        log::info!(
            "GUI: Starting playback for source {} at position {:?} with loop mode {:?}",
            source_id,
            position,
            loop_mode
        );

        self.world
            .play(source_id, loop_mode)
            .map_err(|e| format!("Failed to start playback: {}", e))?;

        self.sources.push(AudioSource {
            id: source_id,
            position,
            file_name: file_name.clone(),
            loop_mode,
        });

        log::info!(
            "GUI: Added source '{}' at position ({:.1}, {:.1}, {:.1}) - total sources: {}",
            file_name,
            position.x,
            position.y,
            position.z,
            self.sources.len()
        );

        Ok(())
    }
}

impl eframe::App for SpatialAudioDemo {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Poll for audio events and handle them
        // This checks for completed sources and removes them from the UI
        let events = self.engine.poll_events();
        if !events.is_empty() {
            log::debug!("GUI: Received {} event(s)", events.len());
        }

        for event in events {
            match event {
                petalsonic_core::PetalSonicEvent::SourceCompleted { source_id } => {
                    log::info!(
                        "GUI: Source {} completed, removing from UI and world storage",
                        source_id
                    );

                    // Remove from UI sources list
                    if let Some(pos) = self.sources.iter().position(|s| s.id == source_id) {
                        let removed = self.sources.remove(pos);
                        log::info!("GUI: Removed source '{}' from UI list", removed.file_name);
                    } else {
                        log::warn!(
                            "GUI: Source {} completed but not found in UI list",
                            source_id
                        );
                    }

                    // Remove from world storage to free memory
                    // Note: You could also keep it in world storage if you want to replay it later
                    if let Some(audio_data) = self.world.remove_audio_data(source_id) {
                        log::info!(
                            "GUI: Freed audio data for source {}: {} samples",
                            source_id,
                            audio_data.samples().len()
                        );
                    } else {
                        log::warn!(
                            "GUI: Source {} already removed from world storage",
                            source_id
                        );
                    }
                }
                petalsonic_core::PetalSonicEvent::SourceLooped {
                    source_id,
                    loop_count,
                } => {
                    // Infinite looping sources emit this event each time they loop
                    // They continue playing, so we don't remove them
                    log::info!(
                        "GUI: Source {} looped (count: {}), continuing playback",
                        source_id,
                        loop_count
                    );
                }
                _ => {
                    // Handle other events if needed
                    log::debug!("GUI: Received event: {:?}", event);
                }
            }
        }

        // Right panel for controls
        egui::SidePanel::right("control_panel")
            .default_width(250.0)
            .show(ctx, |ui| {
                ui.heading("Control Panel");
                ui.separator();

                // Audio file selection
                ui.label("Select Audio File:");
                if !self.available_audio_files.is_empty() {
                    egui::ComboBox::from_label("")
                        .selected_text(&self.available_audio_files[self.selected_audio_file_index])
                        .show_ui(ui, |ui| {
                            for (idx, file) in self.available_audio_files.iter().enumerate() {
                                ui.selectable_value(&mut self.selected_audio_file_index, idx, file);
                            }
                        });
                } else {
                    ui.label("No audio files found");
                }

                ui.add_space(10.0);

                // Loop mode selection
                ui.label("Loop Mode:");
                let loop_modes = ["Once", "Infinite"];
                egui::ComboBox::from_label(" ")
                    .selected_text(loop_modes[self.selected_loop_mode_index])
                    .show_ui(ui, |ui| {
                        for (idx, mode) in loop_modes.iter().enumerate() {
                            ui.selectable_value(&mut self.selected_loop_mode_index, idx, *mode);
                        }
                    });

                ui.add_space(10.0);

                // Add source button
                let button_text = if self.add_source_mode {
                    "Click on grid to place..."
                } else {
                    "Add Source"
                };

                if ui.button(button_text).clicked() {
                    self.add_source_mode = !self.add_source_mode;
                }

                ui.add_space(20.0);
                ui.separator();

                // Source list
                ui.label(format!("Active Sources: {}", self.sources.len()));
                ui.add_space(5.0);

                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (idx, source) in self.sources.iter().enumerate() {
                        ui.group(|ui| {
                            ui.label(format!("#{}: {}", idx + 1, source.file_name));
                            ui.label(format!(
                                "  Pos: ({:.1}, {:.1})",
                                source.position.x, source.position.z
                            ));
                            ui.label(format!("  Loop: {:?}", source.loop_mode));
                        });
                    }
                });
            });

        // Central panel for visualization
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("PetalSonic Spatial Audio Demo");

            let instruction = if self.add_source_mode {
                "Click anywhere on the grid to add a new audio source"
            } else {
                "Drag existing sources to move them around"
            };
            ui.label(instruction);
            ui.separator();

            // Allocate space for the visualization
            let available_size = ui.available_size();
            let size = available_size.x.min(available_size.y) - 20.0;
            let rect =
                Rect::from_center_size(ui.available_rect_before_wrap().center(), Vec2::splat(size));

            // Draw the grid and elements
            self.draw_grid(ui, rect);
            self.draw_listener(ui, rect);
            self.draw_sources(ui, rect);

            // Handle mouse input
            self.handle_mouse_interaction(ui, rect);
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
