use std::str::FromStr;

use async_channel::{Receiver, Sender};
use callme::{audio::AudioConfig, run::NetEvent};
use eframe::NativeOptions;
use egui::{vec2, OutputCommand, Ui, WidgetText};
use egui_dock::{DockArea, DockState, Style, TabViewer};
use iroh::NodeId;
use n0_future::StreamExt;
use strum::VariantArray;

const DEFAULT: &str = "<default>";

pub struct App {
    dock_state: DockState<Tab>,
    state: AppState,
}

struct AppState {
    remote_node_id: String,
    worker: WorkerHandle,
    log: Vec<String>,
    our_node_id: Option<String>,
    devices: callme::audio::Devices,
    selected_input: String,
    selected_output: String,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.state.update();
        ctx.set_pixels_per_point(4.0);
        ctx.style_mut(|s| s.spacing.button_padding = vec2(8.0, 8.0));

        egui::TopBottomPanel::top("my_panel")
            .min_height(40.)
            .show(ctx, |_ui| {});
        egui::CentralPanel::default().show(ctx, |ui| {
            // self.ui_section_call(ui);

            // self.ui_section_config(ui);

            // self.ui_section_log(ui);
            DockArea::new(&mut self.dock_state)
                .style(Style::from_egui(ui.style().as_ref()))
                .show_close_buttons(false)
                .show_leaf_close_all_buttons(false)
                .show_leaf_collapse_buttons(false)
                .show_inside(
                    ui,
                    &mut MyTabViewer {
                        state: &mut self.state,
                    },
                );
        });
    }
}

// First, let's pick a type that we'll use to attach some data to each tab.
// It can be any type.
#[derive(Debug, Copy, Clone, strum::AsRefStr, strum::VariantArray, strum::Display)]
enum Tab {
    Call,
    Config,
    Log,
}

// To define the contents and properties of individual tabs, we implement the `TabViewer`
// trait. Only three things are mandatory: the `Tab` associated type, and the `ui` and
// `title` methods. There are more methods in `TabViewer` which you can also override.
struct MyTabViewer<'a> {
    state: &'a mut AppState,
}

impl<'a> TabViewer for MyTabViewer<'a> {
    // This associated type is used to attach some data to each tab.
    type Tab = Tab;

    // Returns the current `tab`'s title.
    fn title(&mut self, tab: &mut Self::Tab) -> WidgetText {
        tab.as_ref().into()
    }

    // Defines the contents of a given `tab`.
    fn ui(&mut self, ui: &mut Ui, tab: &mut Self::Tab) {
        match tab {
            Tab::Call => self.state.ui_section_call(ui),
            Tab::Config => self.state.ui_section_config(ui),
            Tab::Log => self.state.ui_section_log(ui),
        }
    }

    fn closeable(&mut self, _tab: &mut Self::Tab) -> bool {
        false
    }
}

impl App {
    pub fn run(options: NativeOptions) -> Result<(), eframe::Error> {
        let handle = Worker::spawn();
        let devices =
            callme::audio::AudioContext::list_devices_sync().expect("failed to list audio devices");
        let state = AppState {
            remote_node_id: Default::default(),
            worker: handle,
            log: Default::default(),
            our_node_id: None,
            devices,
            selected_input: DEFAULT.to_string(),
            selected_output: DEFAULT.to_string(),
        };

        let tabs = Tab::VARIANTS.to_vec();
        let dock_state = DockState::new(tabs);
        let app = App { state, dock_state };
        eframe::run_native(
            "egui-android-demo",
            options,
            Box::new(|_cc| Ok(Box::new(app))),
        )
    }
}
impl AppState {
    fn update(&mut self) {
        if let Ok(event) = self.worker.event_rx.try_recv() {
            match event {
                Event::Log(line) => self.log.push(line),
                Event::EndpointBound(node_id) => {
                    self.our_node_id = Some(node_id.to_string());
                }
                Event::Net(event) => self.log.push(format!("{event:?}")),
            }
        }
    }
    fn audio_config(&self) -> AudioConfig {
        let input_device = if self.selected_input == DEFAULT {
            None
        } else {
            Some(self.selected_input.to_string())
        };
        let output_device = if self.selected_output == DEFAULT {
            None
        } else {
            Some(self.selected_output.to_string())
        };
        AudioConfig {
            input_device,
            output_device,
            processing_enabled: true,
        }
    }

    fn ui_section_call(&mut self, ui: &mut Ui) {
        ui.heading("Call a remote node");
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                let name_label = ui.label("Node id: ");
                ui.text_edit_singleline(&mut self.remote_node_id)
                    .labelled_by(name_label.id);
                #[cfg(target_os = "android")]
                if ui
                    .button("📋 Paste")
                    .on_hover_text("Click to paste")
                    .clicked()
                {
                    self.remote_node_id =
                        android_clipboard::get_text().expect("failed to get text from clipboard");
                }
            });
        });
        ui.horizontal(|ui| {
            if ui.button("Call").clicked() {
                self.worker
                    .command_tx
                    .send_blocking(Command::Call {
                        node_id: self.remote_node_id.clone(),
                        audio_config: self.audio_config(),
                    })
                    .unwrap();
            }
        });

        ui.heading("Accept a call");
        if let Some(node_id) = &self.our_node_id {
            ui.horizontal(|ui| {
                if ui.button("Accept calls").clicked() {
                    self.worker
                        .command_tx
                        .send_blocking(Command::Accept {
                            audio_config: self.audio_config(),
                        })
                        .unwrap();
                }
                if ui
                    .button("📋 Copy node id")
                    .on_hover_text("Click to copy")
                    .clicked()
                {
                    ui.output_mut(|writer| {
                        writer
                            .commands
                            .push(OutputCommand::CopyText(node_id.to_string()));
                    });
                    #[cfg(target_os = "android")]
                    if let Err(err) = android_clipboard::set_text(node_id.to_string()) {
                        tracing::warn!("failed to copy text to clipboard: {err}");
                    }
                }
            });
        }
    }

    fn ui_section_config(&mut self, ui: &mut Ui) {
        ui.heading("Audio config");
        ui.vertical(|ui| {
            egui::ComboBox::from_label("Capture device")
                .selected_text(&self.selected_input)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(self.selected_input == DEFAULT, DEFAULT)
                        .clicked()
                    {
                        self.selected_input = DEFAULT.to_string();
                    }
                    for device in &self.devices.input {
                        if ui
                            .selectable_label(&self.selected_input == device, device)
                            .clicked()
                        {
                            self.selected_input = device.to_string()
                        }
                    }
                });

            egui::ComboBox::from_label("Playback device")
                .selected_text(&self.selected_output)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(self.selected_output == DEFAULT, DEFAULT)
                        .clicked()
                    {
                        self.selected_output = DEFAULT.to_string();
                    }
                    for device in &self.devices.output {
                        if ui
                            .selectable_label(&self.selected_output == device, device)
                            .clicked()
                        {
                            self.selected_output = device.to_string()
                        }
                    }
                });
        });
    }

    fn ui_section_log(&mut self, ui: &mut Ui) {
        ui.heading("Log");
        egui::ScrollArea::vertical().show(ui, |ui| {
            for line in &self.log {
                ui.label(line);
            }
        });
    }
}

enum Event {
    EndpointBound(NodeId),
    Net(NetEvent),
    Log(String),
}

enum Command {
    Call {
        node_id: String,
        audio_config: AudioConfig,
    },
    Accept {
        audio_config: AudioConfig,
    },
}

struct Worker {
    command_rx: Receiver<Command>,
    event_tx: Sender<Event>,
}

struct WorkerHandle {
    command_tx: Sender<Command>,
    event_rx: Receiver<Event>,
}

impl Worker {
    pub fn spawn() -> WorkerHandle {
        let (command_tx, command_rx) = async_channel::bounded(16);
        let (event_tx, event_rx) = async_channel::bounded(16);
        let mut worker = Worker {
            event_tx,
            command_rx,
        };
        let handle = WorkerHandle {
            event_rx,
            command_tx,
        };
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to start tokio runtime");
            rt.block_on(async move {
                worker.run().await.expect("worker died");
            });
        });
        handle
    }

    async fn run(&mut self) -> anyhow::Result<()> {
        let ep = callme::net::bind_endpoint().await?;
        self.event_tx
            .send(Event::EndpointBound(ep.node_id()))
            .await?;
        self.log(format!("our node id: {}", ep.node_id().fmt_short()))
            .await;
        let (accept_event_tx, accept_event_rx) = async_channel::bounded(16);
        let (connect_event_tx, connect_event_rx) = async_channel::bounded(16);

        let event_task = n0_future::task::spawn({
            let event_tx = self.event_tx.clone();
            async move {
                let events = n0_future::stream::race(accept_event_rx, connect_event_rx);
                tokio::pin!(events);
                while let Some(event) = events.next().await {
                    event_tx.send(Event::Net(event)).await.ok();
                }
            }
        });
        while let Ok(command) = self.command_rx.recv().await {
            match command {
                Command::Call {
                    node_id,
                    audio_config,
                } => {
                    let node_id = match iroh::NodeId::from_str(&node_id) {
                        Ok(node_id) => node_id,
                        Err(err) => {
                            self.log(format!("failed to parse node id: {err}")).await;
                            continue;
                        }
                    };
                    callme::run::connect(
                        &ep,
                        audio_config,
                        node_id,
                        Some(connect_event_tx.clone()),
                    )
                    .await?;
                }
                Command::Accept { audio_config } => {
                    let accept_event_tx = accept_event_tx.clone();
                    let _accept_task = n0_future::task::spawn({
                        let ep = ep.clone();
                        let audio_config = audio_config.clone();
                        async move {
                            let res =
                                callme::run::accept(&ep, audio_config, Some(accept_event_tx)).await;
                            if let Err(err) = &res {
                                tracing::error!("accept task failed: {err:?}");
                            }
                            res
                        }
                    });
                }
            }
        }
        event_task.await?;
        Ok(())
    }

    async fn log(&self, msg: String) {
        self.event_tx.send(Event::Log(msg)).await.unwrap();
    }
}
