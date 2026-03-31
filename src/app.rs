use egui::{
    Color32, FontId, Label, RichText, ScrollArea, TextEdit, Ui,
};

use crate::config::Config;
use crate::network::{list_interfaces, NetworkInterface};
use crate::worker::{PtpStatus, WorkerCmd, WorkerEvent, WorkerState, spawn_worker};

pub struct App {
    config: Config,
    state: WorkerState,
    logs: Vec<String>,
    ptp: PtpStatus,
    interfaces: Vec<NetworkInterface>,
    show_settings: bool,
    show_log: bool,
    show_about: bool,
    pending_close: bool,

    // Worker communication
    cmd_tx: std::sync::mpsc::Sender<WorkerCmd>,
    event_rx: std::sync::mpsc::Receiver<WorkerEvent>,
}

impl App {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let config = Config::load();
        let (cmd_tx, event_rx) = spawn_worker();
        let interfaces = list_interfaces();

        App {
            config,
            state: WorkerState::Idle,
            logs: Vec::new(),
            ptp: PtpStatus::default(),
            interfaces,
            show_settings: false,
            show_log: false,
            show_about: false,
            pending_close: false,
            cmd_tx,
            event_rx,
        }
    }

    fn is_running(&self) -> bool {
        matches!(self.state, WorkerState::Running { .. })
    }

    fn is_busy(&self) -> bool {
        !matches!(self.state, WorkerState::Idle | WorkerState::Running { .. } | WorkerState::Error(_))
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                WorkerEvent::Log(msg) => {
                    self.logs.push(msg);
                    if self.logs.len() > 2000 {
                        self.logs.drain(0..500);
                    }
                }
                WorkerEvent::StateChanged(state) => {
                    self.state = state;
                }
                WorkerEvent::PtpStatus(status) => {
                    self.ptp = status;
                }
            }
        }
    }

    fn start(&mut self) {
        self.config.sample_rate = 48000;
        self.config.save();
        self.logs.clear();
        self.ptp = PtpStatus::default();
        let _ = self.cmd_tx.send(WorkerCmd::Start(self.config.clone()));
    }

    fn stop(&mut self) {
        let _ = self.cmd_tx.send(WorkerCmd::Stop);
    }

    fn show_config_panel(&mut self, ui: &mut Ui) {
        ui.heading("Configuration");
        ui.add_space(8.0);

        egui::Grid::new("config_grid")
            .num_columns(2)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                // Network Interface + inline refresh button
                ui.label("Network Interface:");
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_source("iface_combo")
                        .selected_text(if self.config.interface.is_empty() {
                            "Select interface...".to_string()
                        } else {
                            self.interfaces
                                .iter()
                                .find(|i| i.name == self.config.interface)
                                .map(|i| i.to_string())
                                .unwrap_or_else(|| self.config.interface.clone())
                        })
                        .width(200.0)
                        .show_ui(ui, |ui| {
                            for iface in &self.interfaces.clone() {
                                let selected = iface.name == self.config.interface;
                                if ui.selectable_label(selected, iface.to_string()).clicked() {
                                    self.config.interface = iface.name.clone();
                                }
                            }
                        });
                    if ui.button("⟳").on_hover_text("Refresh interfaces").clicked() {
                        self.interfaces = list_interfaces();
                    }
                });
                ui.end_row();

                ui.label("Device Name:");
                ui.add(
                    TextEdit::singleline(&mut self.config.device_name)
                        .desired_width(200.0)
                        .hint_text("e.g. my-dante-device"),
                );
                ui.end_row();

                ui.label("Channels:");
                channels_combo(ui, &mut self.config.tx_channels, &mut self.config.rx_channels);
                ui.end_row();

                ui.label("Latency:");
                latency_combo(ui, "lat", &mut self.config.latency_ns);
                ui.end_row();
            });

        ui.add_space(8.0);

        ui.horizontal(|ui| {
            ui.checkbox(&mut self.config.use_pipewire, "Add persistent PipeWire node (untick to use ALSA directly)");
        });
    }

    fn show_log_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_log;
        egui::Window::new("Log")
            .open(&mut open)
            .resizable(true)
            .default_size([620.0, 320.0])
            .show(ctx, |ui| {
                ScrollArea::vertical()
                    .id_source("log_scroll")
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &self.logs {
                            ui.add(Label::new(RichText::new(line).monospace().size(11.0)));
                        }
                    });
            });
        self.show_log = open;
    }

    fn show_about_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_about;
        egui::Window::new("About")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.heading("idvs");
                    ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                    ui.add_space(4.0);
                    ui.hyperlink("https://github.com/stanelie/idvs");
                });
            });
        self.show_about = open;
    }

    fn show_settings_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_settings;
        egui::Window::new("Settings")
            .open(&mut open)
            .resizable(true)
            .min_width(480.0)
            .show(ctx, |ui| {
                let path_width = (ui.available_width() - 160.0).max(200.0);

                egui::Grid::new("adv_grid")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Clock Socket Path:");
                        ui.add(
                            TextEdit::singleline(&mut self.config.clock_path)
                                .desired_width(path_width),
                        );
                        ui.end_row();

                        ui.label("Observation Socket:");
                        ui.add(
                            TextEdit::singleline(&mut self.config.observation_path)
                                .desired_width(path_width),
                        );
                        ui.end_row();

                        ui.label("statime Binary:");
                        ui.horizontal(|ui| {
                            let mut s = self.config.statime_bin.display().to_string();
                            if ui
                                .add(TextEdit::singleline(&mut s).desired_width(path_width - 80.0))
                                .changed()
                            {
                                self.config.statime_bin = s.into();
                            }
                            if ui.button("Browse…").clicked() {
                                if let Some(path) = rfd::FileDialog::new()
                                    .set_title("Select statime binary")
                                    .pick_file()
                                {
                                    self.config.statime_bin = path;
                                }
                            }
                        });
                        ui.end_row();

                        ui.label("Inferno Plugin (.so):");
                        ui.horizontal(|ui| {
                            let mut s = self.config.inferno_so.display().to_string();
                            if ui
                                .add(TextEdit::singleline(&mut s).desired_width(path_width - 80.0))
                                .changed()
                            {
                                self.config.inferno_so = s.into();
                            }
                            if ui.button("Browse…").clicked() {
                                if let Some(path) = rfd::FileDialog::new()
                                    .set_title("Select Inferno plugin (.so)")
                                    .add_filter("Shared Library", &["so"])
                                    .pick_file()
                                {
                                    self.config.inferno_so = path;
                                }
                            }
                        });
                        ui.end_row();

                        ui.label("PipeWire Headroom:");
                        ui.add(
                            egui::DragValue::new(&mut self.config.pipewire_headroom)
                                .speed(8.0)
                                .clamp_range(64u32..=1024u32)
                                .suffix(" frames"),
                        );
                        ui.end_row();
                    });

                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "sudo must be configured for passwordless statime execution,\nor run idvs as root.",
                    )
                    .small()
                    .color(Color32::GRAY),
                );
            });
        self.show_settings = open;
    }

    fn show_status_panel(&mut self, ui: &mut Ui) {
        ui.heading("Status");
        ui.add_space(8.0);

        // Main status badge
        let (badge_text, badge_color) = match &self.state {
            WorkerState::Idle => ("Stopped", Color32::DARK_GRAY),
            WorkerState::StartingStatime => ("Starting statime...", Color32::GOLD),
            WorkerState::WaitingForClock => ("Waiting for clock sync...", Color32::GOLD),
            WorkerState::ConfiguringAlsa => ("Configuring ALSA...", Color32::GOLD),
            WorkerState::AddingPipeWireNode => ("Adding PipeWire node...", Color32::GOLD),
            WorkerState::Running { .. } => {
                if self.ptp.synced {
                    ("Live  (Clock Synced)", Color32::from_rgb(0, 200, 80))
                } else if self.ptp.has_data {
                    ("Running (Syncing...)", Color32::GOLD)
                } else {
                    ("Running (waiting for clock...)", Color32::GOLD)
                }
            }
            WorkerState::Stopping => ("Stopping...", Color32::GOLD),
            WorkerState::Error(_) => ("Error", Color32::RED),
        };

        ui.label(
            RichText::new(badge_text)
                .font(FontId::proportional(18.0))
                .color(badge_color)
                .strong(),
        );

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);

        // Clock / status grid
        egui::Grid::new("ptp_grid")
            .num_columns(2)
            .spacing([8.0, 4.0])
            .show(ui, |ui| {

                if self.ptp.has_data {
                    ui.label("Clock Offset:");
                    ui.label(format_ns(self.ptp.offset_ns));
                    ui.end_row();

                    ui.label("Network Delay:");
                    ui.label(format_ns(self.ptp.delay_ns));
                    ui.end_row();


                } else if self.is_running() {
                    ui.label("Clock:");
                    ui.label(RichText::new("Waiting for sync...").color(Color32::GRAY));
                    ui.end_row();
                }

                if self.is_running() {
                    ui.label("ALSA Device:");
                    ui.label(RichText::new("dante").monospace());
                    ui.end_row();

                    ui.label("ALSA plug device:");
                    ui.label(RichText::new("dante_plug").monospace());
                    ui.end_row();
                }
            });

        if let WorkerState::Error(ref msg) = self.state {
            ui.add_space(8.0);
            ui.label(RichText::new(msg).color(Color32::RED).small());
        }

        if self.is_running() {
            ui.add_space(12.0);
            ui.separator();
            ui.add_space(4.0);
            ui.label(RichText::new("Usage:").strong());
            ui.label(
                RichText::new("ALSA: use device name 'dante' or 'dante_plug'")
                    .small()
                    .color(Color32::LIGHT_GRAY),
            );
            if self.config.use_pipewire {
                ui.label(
                    RichText::new(format!(
                        "PipeWire: sink '{}' visible in audio router",
                        self.config.device_name
                    ))
                    .small()
                    .color(Color32::LIGHT_GRAY),
                );
            }
        }

        // Push Start/Stop buttons to the lower-right
        let btn_size = egui::vec2(110.0, 42.0);
        let spare = ui.available_height();
        if spare > btn_size.y + 8.0 {
            ui.add_space(spare - btn_size.y - 8.0);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let running = self.is_running();
            let stopping = matches!(self.state, WorkerState::Stopping);
            let starting = self.is_busy() && !stopping;

            let btn_text = if running || stopping {
                RichText::new("■  Stop").font(FontId::proportional(16.0))
            } else if starting {
                RichText::new("Starting…")
                    .font(FontId::proportional(16.0))
                    .color(Color32::GOLD)
            } else {
                RichText::new("▶  Start").font(FontId::proportional(16.0))
            };
            let enabled = running || (!starting && !stopping);
            if ui
                .add_enabled(
                    enabled,
                    egui::Button::new(btn_text).min_size(btn_size),
                )
                .clicked()
            {
                if running {
                    self.stop();
                } else {
                    self.start();
                }
            }
            if self.is_busy() {
                ui.spinner();
                ui.label(state_label(&self.state));
            }
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();

        // Graceful shutdown: intercept the window close (X) button.
        if ctx.input(|i| i.viewport().close_requested()) {
            if self.is_running() || self.is_busy() {
                // Cancel the OS close and kick off a stop instead.
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                if !matches!(self.state, WorkerState::Stopping) {
                    self.stop();
                }
                self.pending_close = true;
            }
            // else: not running — let the close proceed naturally.
        }

        // Once a pending close reaches Idle/Error, the cleanup is done — close for real.
        if self.pending_close
            && matches!(self.state, WorkerState::Idle | WorkerState::Error(_))
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        if self.is_busy() || self.is_running() || self.pending_close {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }

        // Floating windows (opened via menu)
        if self.show_settings {
            self.show_settings_window(ctx);
        }
        if self.show_log {
            self.show_log_window(ctx);
        }
        if self.show_about {
            self.show_about_window(ctx);
        }

        // Menu bar
        egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("Settings", |ui| {
                    if ui.button("Advanced Settings…").clicked() {
                        self.show_settings = true;
                        ui.close_menu();
                    }
                });
                if ui.button("Log").clicked() {
                    self.show_log = !self.show_log;
                }
                ui.menu_button("Help", |ui| {
                    if ui.button("About…").clicked() {
                        self.show_about = true;
                        ui.close_menu();
                    }
                });
            });
        });

        // Config panel on the left
        egui::SidePanel::left("config_panel")
            .resizable(true)
            .min_width(300.0)
            .default_width(400.0)
            .show(ctx, |ui| {
                ScrollArea::vertical().show(ui, |ui| {
                    self.show_config_panel(ui);
                });
            });

        // Status in the remaining center
        egui::CentralPanel::default().show(ctx, |ui| {
            self.show_status_panel(ui);
        });
    }
}

fn channels_combo(ui: &mut Ui, tx: &mut u32, rx: &mut u32) {
    let sizes = [8u32, 16, 32, 64];
    let current_label = if *tx == *rx && sizes.contains(tx) {
        format!("{}×{}", tx, tx)
    } else {
        format!("{}×{}", tx, rx)
    };
    egui::ComboBox::from_id_source("ch_combo")
        .selected_text(current_label)
        .width(100.0)
        .show_ui(ui, |ui| {
            for &n in &sizes {
                let selected = *tx == n && *rx == n;
                if ui.selectable_label(selected, format!("{}×{}", n, n)).clicked() {
                    *tx = n;
                    *rx = n;
                }
            }
        });
}

fn latency_combo(ui: &mut Ui, id: &str, value: &mut u32) {
    let label = format!("{} ms", *value / 1_000_000);
    egui::ComboBox::from_id_source(id)
        .selected_text(label)
        .width(100.0)
        .show_ui(ui, |ui| {
            for &ms in &[1u32, 2, 3, 4, 6, 10, 20, 40] {
                ui.selectable_value(value, ms * 1_000_000, format!("{ms} ms"));
            }
        });
}

fn format_ns(ns: f64) -> String {
    let abs = ns.abs();
    let sign = if ns < 0.0 { "-" } else { "+" };
    if abs < 1_000.0 {
        format!("{sign}{abs:.1} ns")
    } else if abs < 1_000_000.0 {
        format!("{sign}{:.2} µs", abs / 1_000.0)
    } else {
        format!("{sign}{:.2} ms", abs / 1_000_000.0)
    }
}

fn state_label(state: &WorkerState) -> &'static str {
    match state {
        WorkerState::StartingStatime => "Starting statime...",
        WorkerState::WaitingForClock => "Waiting for clock sync...",
        WorkerState::ConfiguringAlsa => "Configuring ALSA...",
        WorkerState::AddingPipeWireNode => "Adding PipeWire node...",
        WorkerState::Stopping => "Stopping...",
        _ => "",
    }
}
