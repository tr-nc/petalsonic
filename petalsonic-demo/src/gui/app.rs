use egui::{Color32, Pos2, Rect, Stroke, Vec2};
use petalsonic::{
    RenderTimingEvent, SourceConfig,
    audio_data::PetalSonicAudioData,
    config::PetalSonicWorldDesc,
    engine::PetalSonicEngine,
    math::{Pose, Quat, Vec3},
    playback::LoopMode,
    world::{PetalSonicWorld, SourceId},
};
use std::collections::VecDeque;
use std::sync::Arc;

use super::profiling;

/// Fixed-size grid for scene geometry (walls)
/// Each cell represents a 1m x 1m area in world space
struct SceneGrid {
    width: usize,
    height: usize,
    cell_size: f32,
    cells: Vec<bool>, // Row-major: cells[y * width + x]
}

impl SceneGrid {
    fn new(width: usize, height: usize, cell_size: f32) -> Self {
        Self {
            width,
            height,
            cell_size,
            cells: vec![false; width * height],
        }
    }

    fn get(&self, x: usize, y: usize) -> bool {
        if x < self.width && y < self.height {
            self.cells[y * self.width + x]
        } else {
            false
        }
    }

    fn set(&mut self, x: usize, y: usize, occupied: bool) {
        if x < self.width && y < self.height {
            self.cells[y * self.width + x] = occupied;
        }
    }

    /// Clear all walls
    fn clear(&mut self) {
        self.cells.fill(false);
    }

    /// Convert world coordinates to grid cell indices
    /// Returns None if out of bounds
    fn world_to_cell(&self, world_pos: Vec3) -> Option<(usize, usize)> {
        // World origin is at center of grid
        let half_world_size = (self.width as f32 * self.cell_size) / 2.0;

        let cell_x_float = (world_pos.x + half_world_size) / self.cell_size;
        let cell_y_float = (world_pos.z + half_world_size) / self.cell_size;

        let cell_x = cell_x_float.floor() as i32;
        let cell_y = cell_y_float.floor() as i32;

        // Clamp to valid range (handle edge cases where we're exactly at the boundary)
        if cell_x >= 0 && cell_y >= 0 {
            let clamped_x = (cell_x as usize).min(self.width - 1);
            let clamped_y = (cell_y as usize).min(self.height - 1);

            // Only accept if original values were within reasonable bounds
            if cell_x <= self.width as i32 && cell_y <= self.height as i32 {
                Some((clamped_x, clamped_y))
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Convert grid cell indices to world coordinates (cell center)
    fn cell_to_world(&self, x: usize, y: usize) -> Vec3 {
        let half_world_size = (self.width as f32 * self.cell_size) / 2.0;

        Vec3::new(
            (x as f32 + 0.5) * self.cell_size - half_world_size,
            0.0,
            (y as f32 + 0.5) * self.cell_size - half_world_size,
        )
    }
}

#[derive(Clone, Copy, PartialEq)]
enum SourceType {
    Spatial,
    NonSpatial,
}

#[derive(Clone)]
struct SpatialAudioSource {
    id: SourceId,
    position: Vec3,
    file_name: String,
    loop_mode: LoopMode,
    volume: f32,
}

#[derive(Clone)]
struct NonSpatialAudioSource {
    id: SourceId,
    file_name: String,
    loop_mode: LoopMode,
    volume: f32,
}

pub struct SpatialAudioDemo {
    world: Arc<PetalSonicWorld>,
    engine: PetalSonicEngine,
    spatial_sources: Vec<SpatialAudioSource>,
    non_spatial_sources: Vec<NonSpatialAudioSource>,
    grid_size: f32,

    // Scene geometry
    scene_grid: SceneGrid,

    // UI state
    available_audio_files: Vec<String>,
    selected_audio_file_index: usize,
    selected_loop_mode_index: usize,
    selected_source_type: SourceType,
    add_source_mode: bool,
    brush_mode: bool,
    brush_thickness: usize, // Brush thickness in cells (1 = single cell, 2 = 2x2, etc.)
    brush_last_cell: Option<(usize, usize)>, // Track last painted cell for continuous lines
    dragging_source_index: Option<usize>,
    dragging_listener: bool,
    listener_position: Vec3,

    // Layout configuration
    control_panel_width_ratio: f32, // Ratio of screen width for control panel (0.0-1.0)

    // Performance profiling
    timing_history: VecDeque<RenderTimingEvent>,
    max_history_size: usize,
    max_frame_time_us: u64, // Hard constraint: block_size / sample_rate in microseconds
}

impl SpatialAudioDemo {
    pub fn new() -> Self {
        // Initialize logger
        env_logger::Builder::from_default_env()
            .filter_level(log::LevelFilter::Info)
            .filter_module("symphonia_core::probe", log::LevelFilter::Warn)
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

        // Create engine
        let world_arc = Arc::new(world);
        let mut engine = PetalSonicEngine::new(world_desc.clone(), world_arc.clone())
            .expect("Failed to create engine");

        // Start the engine
        engine.start().expect("Failed to start audio engine");

        // Calculate maximum frame time constraint (block_size / sample_rate)
        //
        // This is the hard real-time constraint for audio processing:
        //
        // 1. block_size is the number of samples per audio buffer (e.g., 1024 samples)
        // 2. sample_rate is samples per second (e.g., 48000 Hz)
        // 3. block_size / sample_rate = time in seconds to process one buffer
        //    Example: 1024 / 48000 = 0.021333... seconds (~21.33 ms)
        //
        // 4. Multiply by 1,000,000 to convert seconds to microseconds
        //    Example: 0.021333 * 1,000,000 = 21,333 µs
        //
        // This represents the absolute deadline: if audio processing takes longer than
        // this time, the next buffer won't be ready when the audio hardware needs it,
        // causing audible glitches, clicks, or dropouts. The profiler uses this value
        // to calculate CPU utilization % and warn when approaching the limit.
        let max_frame_time_us =
            (world_desc.block_size as f64 / world_desc.sample_rate as f64 * 1_000_000.0) as u64;
        log::info!(
            "Performance constraint: {} µs ({:.2} ms) per render iteration",
            max_frame_time_us,
            max_frame_time_us as f64 / 1000.0
        );

        Self {
            world: world_arc,
            engine,
            spatial_sources: Vec::new(),
            non_spatial_sources: Vec::new(),
            grid_size: 2.0,                             // Show 4m x 4m area (-2 to +2)
            scene_grid: SceneGrid::new(100, 100, 0.04), // 100x100 grid, 0.04m (4cm) per cell
            available_audio_files,
            selected_audio_file_index: 0,
            selected_loop_mode_index: 0, // Once
            selected_source_type: SourceType::Spatial,
            add_source_mode: false,
            brush_mode: false,
            brush_thickness: 1,
            brush_last_cell: None,
            dragging_source_index: None,
            dragging_listener: false,
            listener_position: Vec3::new(0.0, 0.0, 0.0),
            control_panel_width_ratio: 0.4, // 40% of screen width for control panel
            timing_history: VecDeque::with_capacity(100),
            max_history_size: 100,
            max_frame_time_us,
        }
    }

    fn scan_audio_files() -> Vec<String> {
        let audio_dir = "petalsonic-demo/asset/sound";
        let mut files = Vec::new();

        if let Ok(entries) = std::fs::read_dir(audio_dir) {
            for entry in entries.flatten() {
                if let Some(file_name) = entry.file_name().to_str()
                    && (file_name.ends_with(".wav")
                        || file_name.ends_with(".mp3")
                        || file_name.ends_with(".ogg"))
                {
                    files.push(file_name.to_string());
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

    /// Draw a line of cells between two grid positions using Bresenham's algorithm
    fn draw_cell_line(&mut self, from: (usize, usize), to: (usize, usize), occupied: bool) {
        let (x0, y0) = (from.0 as i32, from.1 as i32);
        let (x1, y1) = (to.0 as i32, to.1 as i32);

        let dx = (x1 - x0).abs();
        let dy = (y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx - dy;

        let mut x = x0;
        let mut y = y0;

        loop {
            // Paint with thickness
            self.paint_cell_with_thickness(x as usize, y as usize, occupied);

            // Check if we've reached the end
            if x == x1 && y == y1 {
                break;
            }

            let e2 = 2 * err;
            if e2 > -dy {
                err -= dy;
                x += sx;
            }
            if e2 < dx {
                err += dx;
                y += sy;
            }
        }
    }

    /// Paint a cell with the current brush thickness
    /// Paints a square of cells centered on (cx, cy)
    fn paint_cell_with_thickness(&mut self, cx: usize, cy: usize, occupied: bool) {
        let thickness = self.brush_thickness as i32;
        let radius = (thickness - 1) / 2;

        for dy in -radius..=radius {
            for dx in -radius..=radius {
                let x = cx as i32 + dx;
                let y = cy as i32 + dy;

                if x >= 0 && y >= 0 {
                    self.scene_grid.set(x as usize, y as usize, occupied);
                }
            }
        }
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
            painter.line_segment([top, bottom], Stroke::new(1.0, Color32::from_gray(60)));

            // Horizontal lines (constant Z)
            let left = self.world_to_screen(Vec3::new(-self.grid_size, 0.0, offset), rect);
            let right = self.world_to_screen(Vec3::new(self.grid_size, 0.0, offset), rect);
            painter.line_segment([left, right], Stroke::new(1.0, Color32::from_gray(60)));
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

    fn draw_wall_cells(&self, ui: &mut egui::Ui, rect: Rect) {
        let painter = ui.painter();

        // Only draw cells that are in the visible area
        // Calculate visible cell range based on grid_size
        let visible_world_min = Vec3::new(-self.grid_size, 0.0, -self.grid_size);
        let visible_world_max = Vec3::new(self.grid_size, 0.0, self.grid_size);

        // Convert to cell coordinates
        let min_cell = self.scene_grid.world_to_cell(visible_world_min);
        let max_cell = self.scene_grid.world_to_cell(visible_world_max);

        if let (Some((min_x, min_y)), Some((max_x, max_y))) = (min_cell, max_cell) {
            for y in min_y..=max_y {
                for x in min_x..=max_x {
                    if self.scene_grid.get(x, y) {
                        // Draw occupied cell as filled rectangle
                        let cell_world_pos = self.scene_grid.cell_to_world(x, y);
                        let half_cell = self.scene_grid.cell_size / 2.0;

                        // Get the four corners of the cell in world coordinates
                        let corner1 = self.world_to_screen(
                            Vec3::new(
                                cell_world_pos.x - half_cell,
                                0.0,
                                cell_world_pos.z - half_cell,
                            ),
                            rect,
                        );
                        let corner2 = self.world_to_screen(
                            Vec3::new(
                                cell_world_pos.x + half_cell,
                                0.0,
                                cell_world_pos.z + half_cell,
                            ),
                            rect,
                        );

                        let cell_rect = Rect::from_two_pos(corner1, corner2);
                        painter.rect_filled(
                            cell_rect,
                            0.0,
                            Color32::from_rgba_unmultiplied(200, 200, 200, 150),
                        );
                    }
                }
            }
        }
    }

    fn draw_listener(&self, ui: &mut egui::Ui, rect: Rect) {
        let painter = ui.painter();
        let listener_pos = self.world_to_screen(self.listener_position, rect);

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

        for source in self.spatial_sources.iter() {
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

        // Handle brush mode painting/erasing
        if self.brush_mode {
            if let Some(pos) = response.interact_pointer_pos() {
                let world_pos = self.screen_to_world(pos, rect);

                // Check if we should paint or erase
                let is_primary_down = ui.input(|i| i.pointer.primary_down());
                let is_secondary_down = ui.input(|i| i.pointer.secondary_down());
                let is_shift = ui.input(|i| i.modifiers.shift);

                let is_painting = is_primary_down && !is_shift;
                let is_erasing = is_secondary_down || (is_primary_down && is_shift);

                if (is_painting || is_erasing)
                    && let Some(current_cell) = self.scene_grid.world_to_cell(world_pos)
                {
                    // Draw line from last cell to current cell for continuous strokes
                    if let Some(last_cell) = self.brush_last_cell {
                        self.draw_cell_line(last_cell, current_cell, is_painting);
                    } else {
                        // First cell in stroke, paint with thickness
                        self.paint_cell_with_thickness(current_cell.0, current_cell.1, is_painting);
                    }
                    self.brush_last_cell = Some(current_cell);
                } else {
                    // Mouse button released or out of bounds, reset last cell
                    self.brush_last_cell = None;
                }
            } else {
                // No pointer position, reset last cell
                self.brush_last_cell = None;
            }

            // Reset last cell when buttons are released
            if !ui.input(|i| i.pointer.primary_down() || i.pointer.secondary_down()) {
                self.brush_last_cell = None;
            }

            return; // Don't process other interactions in brush mode
        }

        // Handle click to add source
        if self.add_source_mode
            && response.clicked()
            && let Some(pos) = response.interact_pointer_pos()
        {
            match self.selected_source_type {
                SourceType::Spatial => {
                    let world_pos = self.screen_to_world(pos, rect);
                    let clamped_pos = Vec3::new(
                        world_pos.x.clamp(-self.grid_size, self.grid_size),
                        0.0,
                        world_pos.z.clamp(-self.grid_size, self.grid_size),
                    );

                    if let Err(e) = self.add_spatial_source_at_position(clamped_pos) {
                        log::error!("Failed to add spatial source: {}", e);
                    }
                }
                SourceType::NonSpatial => {
                    if let Err(e) = self.add_non_spatial_source() {
                        log::error!("Failed to add non-spatial source: {}", e);
                    }
                }
            }
            return;
        }

        // Handle dragging - check what was clicked when drag starts
        if response.drag_started()
            && let Some(pos) = response.interact_pointer_pos()
        {
            // First check if listener was clicked
            let listener_screen_pos = self.world_to_screen(self.listener_position, rect);
            let listener_dist = ((pos.x - listener_screen_pos.x).powi(2)
                + (pos.y - listener_screen_pos.y).powi(2))
            .sqrt();

            if listener_dist < 15.0 {
                // Click tolerance
                self.dragging_listener = true;
                log::info!("Started dragging listener");
            } else {
                // Check if a source was clicked
                for (idx, source) in self.spatial_sources.iter().enumerate() {
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

        // Handle dragging listener
        if response.dragged()
            && self.dragging_listener
            && let Some(pos) = response.interact_pointer_pos()
        {
            let new_world_pos = self.screen_to_world(pos, rect);
            let clamped_pos = Vec3::new(
                new_world_pos.x.clamp(-self.grid_size, self.grid_size),
                0.0,
                new_world_pos.z.clamp(-self.grid_size, self.grid_size),
            );

            self.listener_position = clamped_pos;
            let listener_pose = Pose::new(clamped_pos, Quat::IDENTITY);
            self.world.set_listener_pose(listener_pose);
        }

        // Handle dragging sources
        if response.dragged()
            && let Some(idx) = self.dragging_source_index
            && let Some(pos) = response.interact_pointer_pos()
        {
            let new_world_pos = self.screen_to_world(pos, rect);
            let clamped_pos = Vec3::new(
                new_world_pos.x.clamp(-self.grid_size, self.grid_size),
                0.0,
                new_world_pos.z.clamp(-self.grid_size, self.grid_size),
            );

            if let Some(source) = self.spatial_sources.get_mut(idx) {
                source.position = clamped_pos;
                let new_config = SourceConfig::spatial_from_position_with_volume(clamped_pos, 1.0);
                if let Err(e) = self.world.update_source_config(source.id, new_config) {
                    log::error!("Failed to update source config: {}", e);
                }
            }
        }

        // Handle drag stopped
        if response.drag_stopped() {
            if self.dragging_listener {
                log::info!("Stopped dragging listener");
                self.dragging_listener = false;
            }
            if let Some(idx) = self.dragging_source_index {
                log::info!("Stopped dragging source {}", idx);
                self.dragging_source_index = None;
            }
        }
    }

    fn add_spatial_source_at_position(&mut self, position: Vec3) -> Result<(), String> {
        if self.available_audio_files.is_empty() {
            return Err("No audio files available".to_string());
        }

        let file_name = &self.available_audio_files[self.selected_audio_file_index];
        let file_path = format!("petalsonic-demo/asset/sound/{}", file_name);

        log::info!("GUI: Loading spatial audio file: {}", file_path);

        let audio_data = PetalSonicAudioData::from_path(&file_path)
            .map_err(|e| format!("Failed to load audio file: {}", e))?;

        log::debug!(
            "GUI: Audio loaded - {} samples at {} Hz",
            audio_data.samples().len(),
            audio_data.sample_rate()
        );

        let source_id = self
            .world
            .register_audio(
                audio_data,
                SourceConfig::spatial_from_position_with_volume(position, 1.0),
            )
            .map_err(|e| format!("Failed to register audio in world: {}", e))?;

        log::debug!("GUI: Audio registered with source ID: {}", source_id);

        let loop_mode = match self.selected_loop_mode_index {
            0 => LoopMode::Once,
            1 => LoopMode::Infinite,
            _ => LoopMode::Once,
        };

        log::info!(
            "GUI: Starting playback for spatial source {} at position {:?} with loop mode {:?}",
            source_id,
            position,
            loop_mode
        );

        self.world
            .play(source_id, loop_mode)
            .map_err(|e| format!("Failed to start playback: {}", e))?;

        self.spatial_sources.push(SpatialAudioSource {
            id: source_id,
            position,
            file_name: file_name.clone(),
            loop_mode,
            volume: 1.0,
        });

        log::info!(
            "GUI: Added spatial source '{}' at position ({:.1}, {:.1}, {:.1}) - total spatial sources: {}",
            file_name,
            position.x,
            position.y,
            position.z,
            self.spatial_sources.len()
        );

        Ok(())
    }

    fn add_non_spatial_source(&mut self) -> Result<(), String> {
        if self.available_audio_files.is_empty() {
            return Err("No audio files available".to_string());
        }

        let file_name = &self.available_audio_files[self.selected_audio_file_index];
        let file_path = format!("petalsonic-demo/asset/sound/{}", file_name);

        log::info!("GUI: Loading non-spatial audio file: {}", file_path);

        let audio_data = PetalSonicAudioData::from_path(&file_path)
            .map_err(|e| format!("Failed to load audio file: {}", e))?;

        log::debug!(
            "GUI: Audio loaded - {} samples at {} Hz",
            audio_data.samples().len(),
            audio_data.sample_rate()
        );

        let source_id = self
            .world
            .register_audio(audio_data, SourceConfig::non_spatial())
            .map_err(|e| format!("Failed to register audio in world: {}", e))?;

        log::debug!("GUI: Audio registered with source ID: {}", source_id);

        let loop_mode = match self.selected_loop_mode_index {
            0 => LoopMode::Once,
            1 => LoopMode::Infinite,
            _ => LoopMode::Once,
        };

        log::info!(
            "GUI: Starting playback for non-spatial source {} with loop mode {:?}",
            source_id,
            loop_mode
        );

        self.world
            .play(source_id, loop_mode)
            .map_err(|e| format!("Failed to start playback: {}", e))?;

        self.non_spatial_sources.push(NonSpatialAudioSource {
            id: source_id,
            file_name: file_name.clone(),
            loop_mode,
            volume: 1.0,
        });

        log::info!(
            "GUI: Added non-spatial source '{}' - total non-spatial sources: {}",
            file_name,
            self.non_spatial_sources.len()
        );

        Ok(())
    }

    /// Unified delete logic for both spatial and non-spatial sources
    fn delete_source(&mut self, source_id: SourceId) {
        log::info!("GUI: Deleting source {}", source_id);

        // Remove from spatial sources list if present
        if let Some(pos) = self.spatial_sources.iter().position(|s| s.id == source_id) {
            let source = self.spatial_sources.remove(pos);
            log::info!(
                "GUI: Removed spatial source '{}' at position {:?}",
                source.file_name,
                source.position
            );
        }

        // Remove from non-spatial sources list if present
        if let Some(pos) = self
            .non_spatial_sources
            .iter()
            .position(|s| s.id == source_id)
        {
            let source = self.non_spatial_sources.remove(pos);
            log::info!("GUI: Removed non-spatial source '{}'", source.file_name);
        }

        // Stop playback first (if still playing)
        if let Err(e) = self.world.stop(source_id) {
            log::warn!("Failed to stop source {}: {}", source_id, e);
        }

        // Remove from world storage to free memory
        self.world.remove_audio_data(source_id);
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
                petalsonic::PetalSonicEvent::SourceCompleted { source_id } => {
                    log::info!(
                        "GUI: Source {} completed, removing from UI and world storage",
                        source_id
                    );

                    // Use unified delete logic
                    self.delete_source(source_id);
                }
                petalsonic::PetalSonicEvent::SourceLooped {
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

        // Poll for timing events and update history
        let timing_events = self.engine.poll_timing_events();
        for timing in timing_events {
            // Add to history
            self.timing_history.push_back(timing);

            // Keep history at max size
            while self.timing_history.len() > self.max_history_size {
                self.timing_history.pop_front();
            }
        }

        // Calculate control panel width based on screen size and ratio
        let screen_width = ctx.screen_rect().width();
        let panel_width = screen_width * self.control_panel_width_ratio;

        // Right panel for controls
        egui::SidePanel::right("control_panel")
            .default_width(panel_width)
            .show(ctx, |ui| {
                // Wrap entire panel content in a scroll area that supports both directions
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.heading("Control Panel");
                        ui.separator();

                        // Audio file selection
                        ui.label("Select Audio File:");
                        if !self.available_audio_files.is_empty() {
                            egui::ComboBox::from_label("")
                                .selected_text(
                                    &self.available_audio_files[self.selected_audio_file_index],
                                )
                                .show_ui(ui, |ui| {
                                    for (idx, file) in self.available_audio_files.iter().enumerate()
                                    {
                                        ui.selectable_value(
                                            &mut self.selected_audio_file_index,
                                            idx,
                                            file,
                                        );
                                    }
                                });
                        } else {
                            ui.label("No audio files found");
                        }

                        ui.add_space(10.0);

                        // Source type selection
                        ui.label("Source Type:");
                        let source_types = ["Spatial", "Non-Spatial"];
                        let current_type_index = match self.selected_source_type {
                            SourceType::Spatial => 0,
                            SourceType::NonSpatial => 1,
                        };
                        let mut temp_index = current_type_index;
                        egui::ComboBox::from_label("  ")
                            .selected_text(source_types[current_type_index])
                            .show_ui(ui, |ui| {
                                for (idx, source_type) in source_types.iter().enumerate() {
                                    ui.selectable_value(&mut temp_index, idx, *source_type);
                                }
                            });
                        let new_source_type = match temp_index {
                            0 => SourceType::Spatial,
                            1 => SourceType::NonSpatial,
                            _ => SourceType::Spatial,
                        };

                        // Reset add_source_mode when switching to non-spatial
                        if new_source_type != self.selected_source_type
                            && new_source_type == SourceType::NonSpatial
                        {
                            self.add_source_mode = false;
                        }
                        self.selected_source_type = new_source_type;

                        ui.add_space(10.0);

                        // Loop mode selection
                        ui.label("Loop Mode:");
                        let loop_modes = ["Once", "Infinite"];
                        egui::ComboBox::from_label(" ")
                            .selected_text(loop_modes[self.selected_loop_mode_index])
                            .show_ui(ui, |ui| {
                                for (idx, mode) in loop_modes.iter().enumerate() {
                                    ui.selectable_value(
                                        &mut self.selected_loop_mode_index,
                                        idx,
                                        *mode,
                                    );
                                }
                            });

                        ui.add_space(10.0);
                        ui.separator();
                        ui.add_space(10.0);

                        // Brush mode toggle
                        ui.label("Scene Editing:");
                        let brush_button_text = if self.brush_mode {
                            "Edit Mode: ON (painting walls)"
                        } else {
                            "Edit Mode: OFF"
                        };
                        if ui.button(brush_button_text).clicked() {
                            self.brush_mode = !self.brush_mode;
                            // Exit add source mode when entering brush mode
                            if self.brush_mode {
                                self.add_source_mode = false;
                            }
                        }

                        // Brush thickness slider
                        ui.add_space(5.0);
                        ui.horizontal(|ui| {
                            ui.label("Brush size:");
                            // Map slider value (0 or 1) to thickness (1 or 3)
                            let mut slider_value = if self.brush_thickness == 1 { 0 } else { 1 };
                            if ui
                                .add(
                                    egui::Slider::new(&mut slider_value, 0..=1)
                                        .custom_formatter(|v, _| {
                                            if v == 0.0 {
                                                "1 cell".to_string()
                                            } else {
                                                "3 cells".to_string()
                                            }
                                        })
                                        .custom_parser(|s| {
                                            if s == "1 cell" {
                                                Some(0.0)
                                            } else if s == "3 cells" {
                                                Some(1.0)
                                            } else {
                                                None
                                            }
                                        }),
                                )
                                .changed()
                            {
                                self.brush_thickness = if slider_value == 0 { 1 } else { 3 };
                            }
                        });

                        // Clear walls button
                        ui.add_space(5.0);
                        if ui.button("Clear All Walls").clicked() {
                            self.scene_grid.clear();
                        }

                        ui.add_space(10.0);

                        // Add source button - different behavior based on source type
                        let button_text = match self.selected_source_type {
                            SourceType::Spatial => {
                                if self.add_source_mode {
                                    "Click on grid to place"
                                } else {
                                    "Add Source"
                                }
                            }
                            SourceType::NonSpatial => "Add source",
                        };

                        if ui.button(button_text).clicked() {
                            match self.selected_source_type {
                                SourceType::Spatial => {
                                    // Toggle add mode for spatial sources
                                    self.add_source_mode = !self.add_source_mode;
                                }
                                SourceType::NonSpatial => {
                                    // Instantly add non-spatial source
                                    if let Err(e) = self.add_non_spatial_source() {
                                        log::error!("Failed to add non-spatial source: {}", e);
                                    }
                                }
                            }
                        }

                        ui.add_space(20.0);
                        ui.separator();

                        // Spatial sources section (collapsible)
                        egui::CollapsingHeader::new(format!(
                            "Spatial Sources ({})",
                            self.spatial_sources.len()
                        ))
                        .id_salt("spatial_sources_list")
                        .show(ui, |ui| {
                            let mut source_to_delete: Option<SourceId> = None;
                            let mut volume_changes: Vec<(SourceId, f32)> = Vec::new();

                            egui::ScrollArea::vertical()
                                .max_height(200.0)
                                .show(ui, |ui| {
                                    for (idx, source) in self.spatial_sources.iter().enumerate() {
                                        ui.group(|ui| {
                                            ui.horizontal(|ui| {
                                                ui.vertical(|ui| {
                                                    ui.label(format!(
                                                        "#{}: {}",
                                                        idx + 1,
                                                        source.file_name
                                                    ));
                                                    ui.label(format!(
                                                        "  Pos: ({:.1}, {:.1})",
                                                        source.position.x, source.position.z
                                                    ));
                                                    ui.label(format!(
                                                        "  Loop: {:?}",
                                                        source.loop_mode
                                                    ));

                                                    // Volume slider
                                                    ui.horizontal(|ui| {
                                                        ui.label("  Volume:");
                                                        let mut volume = source.volume;
                                                        if ui
                                                            .add(
                                                                egui::Slider::new(
                                                                    &mut volume,
                                                                    0.0..=1.0,
                                                                )
                                                                .show_value(true),
                                                            )
                                                            .changed()
                                                        {
                                                            volume_changes
                                                                .push((source.id, volume));
                                                        }
                                                    });
                                                });

                                                ui.with_layout(
                                                    egui::Layout::right_to_left(
                                                        egui::Align::Center,
                                                    ),
                                                    |ui| {
                                                        if ui.button("Delete").clicked() {
                                                            source_to_delete = Some(source.id);
                                                        }
                                                    },
                                                );
                                            });
                                        });
                                    }
                                });

                            // Apply volume changes
                            for (source_id, new_volume) in volume_changes {
                                if let Some(source) =
                                    self.spatial_sources.iter_mut().find(|s| s.id == source_id)
                                {
                                    source.volume = new_volume;
                                    let new_config =
                                        SourceConfig::spatial_from_position_with_volume(
                                            source.position,
                                            new_volume,
                                        );
                                    if let Err(e) =
                                        self.world.update_source_config(source_id, new_config)
                                    {
                                        log::error!(
                                            "Failed to update volume for source {}: {}",
                                            source_id,
                                            e
                                        );
                                    }
                                }
                            }

                            // Apply deletion after rendering to avoid borrow checker issues
                            if let Some(source_id) = source_to_delete {
                                self.delete_source(source_id);
                            }
                        });

                        ui.add_space(10.0);

                        // Non-spatial sources section (collapsible)
                        egui::CollapsingHeader::new(format!(
                            "Non-Spatial Sources ({})",
                            self.non_spatial_sources.len()
                        ))
                        .id_salt("non_spatial_sources_list")
                        .show(ui, |ui| {
                            let mut source_to_delete: Option<SourceId> = None;
                            let mut volume_changes: Vec<(SourceId, f32)> = Vec::new();

                            egui::ScrollArea::vertical()
                                .max_height(200.0)
                                .show(ui, |ui| {
                                    for (idx, source) in self.non_spatial_sources.iter().enumerate()
                                    {
                                        ui.group(|ui| {
                                            ui.horizontal(|ui| {
                                                ui.vertical(|ui| {
                                                    ui.label(format!(
                                                        "#{}: {}",
                                                        idx + 1,
                                                        source.file_name
                                                    ));
                                                    ui.label(format!(
                                                        "  Loop: {:?}",
                                                        source.loop_mode
                                                    ));

                                                    // Volume slider
                                                    ui.horizontal(|ui| {
                                                        ui.label("  Volume:");
                                                        let mut volume = source.volume;
                                                        if ui
                                                            .add(
                                                                egui::Slider::new(
                                                                    &mut volume,
                                                                    0.0..=1.0,
                                                                )
                                                                .show_value(true),
                                                            )
                                                            .changed()
                                                        {
                                                            volume_changes
                                                                .push((source.id, volume));
                                                        }
                                                    });
                                                });

                                                ui.with_layout(
                                                    egui::Layout::right_to_left(
                                                        egui::Align::Center,
                                                    ),
                                                    |ui| {
                                                        if ui.button("Delete").clicked() {
                                                            source_to_delete = Some(source.id);
                                                        }
                                                    },
                                                );
                                            });
                                        });
                                    }
                                });

                            // Apply volume changes
                            for (source_id, new_volume) in volume_changes {
                                if let Some(source) = self
                                    .non_spatial_sources
                                    .iter_mut()
                                    .find(|s| s.id == source_id)
                                {
                                    source.volume = new_volume;
                                    let new_config =
                                        SourceConfig::non_spatial_with_volume(new_volume);
                                    if let Err(e) =
                                        self.world.update_source_config(source_id, new_config)
                                    {
                                        log::error!(
                                            "Failed to update volume for source {}: {}",
                                            source_id,
                                            e
                                        );
                                    }
                                }
                            }

                            // Apply deletion after rendering to avoid borrow checker issues
                            if let Some(source_id) = source_to_delete {
                                self.delete_source(source_id);
                            }
                        });

                        ui.add_space(20.0);
                        ui.separator();

                        // Performance profiling widget
                        profiling::draw_profiling_widget(
                            ui,
                            &self.timing_history,
                            self.max_frame_time_us,
                        );
                    });
            });

        // Central panel for visualization
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("PetalSonic Spatial Audio Demo");

            ui.separator();

            // Allocate space for the visualization
            let available_size = ui.available_size();
            let size = available_size.x.min(available_size.y) - 20.0;
            let rect =
                Rect::from_center_size(ui.available_rect_before_wrap().center(), Vec2::splat(size));

            // Draw the grid and elements
            self.draw_grid(ui, rect);
            self.draw_wall_cells(ui, rect);
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
