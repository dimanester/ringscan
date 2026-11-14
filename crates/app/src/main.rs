// Toolchain smoke test: opens a window, takes a screenshot of itself, exits.
#![windows_subsystem = "windows"]

use eframe::egui;

fn main() -> eframe::Result {
    let shot = std::env::args().nth(1);
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([640.0, 400.0]),
        multisampling: 4,
        ..Default::default()
    };
    eframe::run_native(
        "RingScan smoke",
        opts,
        Box::new(move |_cc| Ok(Box::new(Smoke { shot, frame: 0 }))),
    )
}

struct Smoke {
    shot: Option<String>,
    frame: u32,
}

impl eframe::App for Smoke {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("RingScan — тест кирилиці: Привіт, світ! ґєїі");
            let c = ui.max_rect().center();
            ui.painter()
                .circle_filled(c, 100.0, egui::Color32::from_rgb(80, 140, 220));
        });
        self.frame += 1;
        let ctx = ui.ctx().clone();
        if self.frame == 5 {
            let path = self.shot.clone();
            let ctx2 = ctx.clone();
            ctx.request_screenshot(move |img| {
                if let Some(p) = path {
                    let _ = save_ppm(&p, &img);
                }
                ctx2.send_viewport_cmd(egui::ViewportCommand::Close);
                ctx2.request_repaint();
            });
        }
        ctx.request_repaint();
    }
}

fn save_ppm(path: &str, img: &egui::ColorImage) -> std::io::Result<()> {
    let mut out = format!("P6\n{} {}\n255\n", img.size[0], img.size[1]).into_bytes();
    for px in &img.pixels {
        out.extend_from_slice(&[px.r(), px.g(), px.b()]);
    }
    std::fs::write(path, out)
}
