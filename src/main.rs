mod app;
mod config;
mod helper;
mod network;
mod worker;

fn main() -> eframe::Result<()> {
    env_logger::init();

    // When launched as the root helper (via pkexec), run the helper server
    // instead of the GUI. The GUI passes --helper <socket_path> as args.
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--helper") {
        if let Some(socket_path) = args.get(pos + 1) {
            helper::run_helper(socket_path);
            return Ok(());
        }
    }

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("IDVS — Inferno Dante Virtual Soundcard")
            .with_inner_size([820.0, 560.0])
            .with_min_inner_size([640.0, 420.0]),
        ..Default::default()
    };

    eframe::run_native(
        "idvs",
        native_options,
        Box::new(|cc| Box::new(app::App::new(cc))),
    )
}
