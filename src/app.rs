use eframe::egui;

pub struct RustedOutClient;

impl RustedOutClient {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        Self
    }
}

impl eframe::App for RustedOutClient {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.heading("RustedOutClient");
                ui.add_space(12.0);
                ui.label("Proxmox profile not configured.");
            });
        });
    }
}
