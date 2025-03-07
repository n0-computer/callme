use std::str::FromStr;

use async_channel::{Receiver, Sender};
use callme::run::NetEvent;
use eframe::NativeOptions;
use egui::{vec2, OutputCommand};
use iroh::NodeId;
use n0_future::StreamExt;

pub struct App {
    remote_node_id: String,
    worker: WorkerHandle,
    log: Vec<String>,
    our_node_id: Option<String>,
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Ok(event) = self.worker.event_rx.try_recv() {
            match event {
                Event::Log(line) => self.log.push(line),
                Event::EndpointBound(node_id) => {
                    self.our_node_id = Some(node_id.to_string());
                }
                Event::Net(event) => self.log.push(format!("{event:?}")),
            }
        }
        ctx.set_pixels_per_point(4.0);
        ctx.style_mut(|s| s.spacing.button_padding = vec2(8.0, 8.0));

        egui::TopBottomPanel::top("my_panel")
            .min_height(40.)
            .show(ctx, |_ui| {});
        egui::CentralPanel::default().show(ctx, |ui| {
            // ui.vertical(|ui| {
            ui.heading("Call a remote node");
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    let name_label = ui.label("Node id: ");
                    ui.text_edit_singleline(&mut self.remote_node_id)
                        .labelled_by(name_label.id);
                });
                #[cfg(target_os = "android")]
                {
                    if ui
                        .button("📋 Paste")
                        .on_hover_text("Click to paste")
                        .clicked()
                    {
                        self.remote_node_id = android_clipboard::get_text()
                            .expect("failed to get text from clipboard");
                    }
                }
            });
            if ui.button("Call").clicked() {
                self.call();
            }

            ui.heading("Accept a call");
            if let Some(node_id) = &self.our_node_id {
                ui.vertical(|ui| {
                    // ui.label(format!("{}…", &node_id[..16]));
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

            ui.heading("Log");
            egui::ScrollArea::vertical().show(ui, |ui| {
                for line in &self.log {
                    ui.label(line);
                }
            });
            // });
        });
    }
}

impl App {
    pub fn run(options: NativeOptions) -> Result<(), eframe::Error> {
        let handle = Worker::spawn();
        let app = App {
            remote_node_id: Default::default(),
            worker: handle,
            log: Default::default(),
            our_node_id: None,
        };
        eframe::run_native(
            "egui-android-demo",
            options,
            Box::new(|_cc| Ok(Box::new(app))),
        )
    }

    pub fn call(&mut self) {
        self.worker
            .command_tx
            .send_blocking(Command::Call(self.remote_node_id.clone()))
            .unwrap();
    }
}

enum Event {
    EndpointBound(NodeId),
    Net(NetEvent),
    Log(String),
}

enum Command {
    Call(String),
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
        let devices = callme::audio::AudioContext::list_devices().await?;
        self.log(format!("{devices:#?}")).await;
        let audio_config = callme::audio::AudioConfig::default();
        let (accept_event_tx, accept_event_rx) = async_channel::bounded(16);
        let (connect_event_tx, connect_event_rx) = async_channel::bounded(16);
        let accept_task = n0_future::task::spawn({
            let ep = ep.clone();
            let audio_config = audio_config.clone();
            async move {
                let res = callme::run::accept(&ep, audio_config, Some(accept_event_tx)).await;
                if let Err(err) = &res {
                    tracing::error!("accept task failed: {err:?}");
                }
                res
            }
        });
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
                Command::Call(node_id) => {
                    let node_id = match iroh::NodeId::from_str(&node_id) {
                        Ok(node_id) => node_id,
                        Err(err) => {
                            self.log(format!("failed to parse node id: {err}")).await;
                            continue;
                        }
                    };
                    callme::run::connect(
                        &ep,
                        audio_config.clone(),
                        node_id,
                        Some(connect_event_tx.clone()),
                    )
                    .await?;
                }
            }
        }
        accept_task.await??;
        event_task.await?;
        Ok(())
    }

    async fn log(&self, msg: String) {
        self.event_tx.send(Event::Log(msg)).await.unwrap();
    }
}
