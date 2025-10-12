use egui::{Color32, Pos2, Stroke, Vec2};
use petalsonic_core::RenderTimingEvent;
use std::collections::VecDeque;

/// Draw a performance profiling widget showing render timing history
///
/// # Arguments
/// * `ui` - The egui UI context
/// * `timing_history` - History of render timing events
/// * `max_frame_time_us` - Maximum allowed frame time (constraint) in microseconds
pub fn draw_profiling_widget(
    ui: &mut egui::Ui,
    timing_history: &VecDeque<RenderTimingEvent>,
    max_frame_time_us: u64,
) {
    ui.collapsing("Performance Profiling", |ui| {
        if timing_history.is_empty() {
            ui.label("No timing data available yet...");
            return;
        }

        // Get the most recent timing event
        let latest = timing_history.back().unwrap();

        // Convert to milliseconds for display
        let max_frame_time_ms = max_frame_time_us as f32 / 1000.0;
        let latest_total_ms = latest.total_time_us as f32 / 1000.0;
        let latest_mixing_ms = latest.mixing_time_us as f32 / 1000.0;
        let latest_resampling_ms = latest.resampling_time_us as f32 / 1000.0;

        // Calculate utilization percentage
        let utilization =
            (latest.total_time_us as f32 / max_frame_time_us as f32 * 100.0).min(999.0);

        // Display current values
        ui.heading("Current Frame");
        ui.label(format!(
            "Total: {:.2} ms ({:.1}%)",
            latest_total_ms, utilization
        ));
        ui.label(format!("Mixing: {:.2} ms", latest_mixing_ms));
        ui.label(format!("Resampling: {:.2} ms", latest_resampling_ms));
        ui.label(format!("Constraint: {:.2} ms", max_frame_time_ms));

        // Warning if approaching limit
        if utilization > 90.0 {
            ui.colored_label(Color32::RED, "⚠ WARNING: Approaching performance limit!");
        } else if utilization > 70.0 {
            ui.colored_label(Color32::YELLOW, "⚠ Caution: High CPU usage");
        }

        ui.add_space(10.0);

        // Draw the graph
        ui.heading("Timing History");
        let graph_height = 200.0;
        let (response, painter) = ui.allocate_painter(
            Vec2::new(ui.available_width(), graph_height),
            egui::Sense::hover(),
        );
        let rect = response.rect;

        // Draw background
        painter.rect_filled(rect, 0.0, Color32::from_gray(20));

        if timing_history.len() < 2 {
            return;
        }

        // Calculate graph bounds
        let max_y_value = (max_frame_time_us as f32 * 1.2).max(
            timing_history
                .iter()
                .map(|t| t.total_time_us as f32)
                .max_by(|a, b| a.partial_cmp(b).unwrap())
                .unwrap_or(max_frame_time_us as f32),
        );

        // Draw horizontal constraint line (red)
        let constraint_y = rect.max.y - (max_frame_time_us as f32 / max_y_value) * rect.height();
        painter.line_segment(
            [
                Pos2::new(rect.min.x, constraint_y),
                Pos2::new(rect.max.x, constraint_y),
            ],
            Stroke::new(2.0, Color32::RED),
        );

        // Draw constraint label
        painter.text(
            Pos2::new(rect.max.x - 5.0, constraint_y - 5.0),
            egui::Align2::RIGHT_BOTTOM,
            format!("{:.1}ms", max_frame_time_ms),
            egui::FontId::proportional(10.0),
            Color32::RED,
        );

        // Draw timing lines
        let x_step = rect.width() / (timing_history.len() - 1) as f32;

        // Draw total time (white line)
        let mut total_points = Vec::new();
        for (i, timing) in timing_history.iter().enumerate() {
            let x = rect.min.x + i as f32 * x_step;
            let y = rect.max.y - (timing.total_time_us as f32 / max_y_value) * rect.height();
            total_points.push(Pos2::new(x, y));
        }

        // Draw line segments
        for window in total_points.windows(2) {
            painter.line_segment([window[0], window[1]], Stroke::new(2.0, Color32::WHITE));
        }

        // Draw mixing time (cyan line)
        let mut mixing_points = Vec::new();
        for (i, timing) in timing_history.iter().enumerate() {
            let x = rect.min.x + i as f32 * x_step;
            let y = rect.max.y - (timing.mixing_time_us as f32 / max_y_value) * rect.height();
            mixing_points.push(Pos2::new(x, y));
        }

        for window in mixing_points.windows(2) {
            painter.line_segment(
                [window[0], window[1]],
                Stroke::new(1.5, Color32::LIGHT_BLUE),
            );
        }

        // Draw resampling time (yellow line)
        let mut resampling_points = Vec::new();
        for (i, timing) in timing_history.iter().enumerate() {
            let x = rect.min.x + i as f32 * x_step;
            let y = rect.max.y - (timing.resampling_time_us as f32 / max_y_value) * rect.height();
            resampling_points.push(Pos2::new(x, y));
        }

        for window in resampling_points.windows(2) {
            painter.line_segment([window[0], window[1]], Stroke::new(1.5, Color32::YELLOW));
        }

        // Draw legend
        let legend_x = rect.min.x + 10.0;
        let legend_y = rect.min.y + 10.0;

        painter.line_segment(
            [
                Pos2::new(legend_x, legend_y),
                Pos2::new(legend_x + 20.0, legend_y),
            ],
            Stroke::new(2.0, Color32::WHITE),
        );
        painter.text(
            Pos2::new(legend_x + 25.0, legend_y),
            egui::Align2::LEFT_CENTER,
            "Total",
            egui::FontId::proportional(10.0),
            Color32::WHITE,
        );

        painter.line_segment(
            [
                Pos2::new(legend_x, legend_y + 15.0),
                Pos2::new(legend_x + 20.0, legend_y + 15.0),
            ],
            Stroke::new(1.5, Color32::LIGHT_BLUE),
        );
        painter.text(
            Pos2::new(legend_x + 25.0, legend_y + 15.0),
            egui::Align2::LEFT_CENTER,
            "Mixing",
            egui::FontId::proportional(10.0),
            Color32::LIGHT_BLUE,
        );

        painter.line_segment(
            [
                Pos2::new(legend_x, legend_y + 30.0),
                Pos2::new(legend_x + 20.0, legend_y + 30.0),
            ],
            Stroke::new(1.5, Color32::YELLOW),
        );
        painter.text(
            Pos2::new(legend_x + 25.0, legend_y + 30.0),
            egui::Align2::LEFT_CENTER,
            "Resampling",
            egui::FontId::proportional(10.0),
            Color32::YELLOW,
        );

        // Draw Y-axis labels
        let num_y_labels = 5;
        for i in 0..=num_y_labels {
            let value = (max_y_value / 1000.0) * (i as f32 / num_y_labels as f32);
            let y = rect.max.y - (i as f32 / num_y_labels as f32) * rect.height();
            painter.text(
                Pos2::new(rect.min.x + 2.0, y),
                egui::Align2::LEFT_CENTER,
                format!("{:.1}", value),
                egui::FontId::proportional(9.0),
                Color32::GRAY,
            );
        }
    });
}
