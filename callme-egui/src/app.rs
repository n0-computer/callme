use std::{collections::BTreeMap, str::FromStr};

use anyhow::{anyhow, Context, Result};
use async_channel::{Receiver, Sender};
use callme::{
    audio::{AudioConfig, AudioContext},
    rtc::{MediaTrack, RtcConnection, RtcProtocol, TrackKind},
};
use eframe::NativeOptions;
use egui::{Color32, RichText, Ui};
use iroh::{protocol::Router, Endpoint, KeyParsingError, NodeId};
use tokio::task::JoinSet;
use tracing::{info, warn};

const DEFAULT: &str = "<default>";

pub struct App {
    is_first_update: bool,
    state: AppState,
}

enum UiSection {
    Config,
    Main,
}

struct AppState {
    section: UiSection,
    remote_node_id: Option<Result<NodeId, KeyParsingError>>,
    worker: WorkerHandle,
    our_node_id: Option<NodeId>,
    devices: callme::audio::Devices,
    audio_config: UiAudioConfig,
    calls: BTreeMap<NodeId, CallState>,
}

struct UiAudioConfig {
    selected_input: String,
    selected_output: String,
    processing_enabled: bool,
}

impl From<&UiAudioConfig> for AudioConfig {
    fn from(value: &UiAudioConfig) -> Self {
        let input_device = if value.selected_input == DEFAULT {
            None
        } else {
            Some(value.selected_input.to_string())
        };
        let output_device = if value.selected_output == DEFAULT {
            None
        } else {
            Some(value.selected_output.to_string())
        };
        AudioConfig {
            input_device,
            output_device,
            processing_enabled: value.processing_enabled,
        }
    }
}

impl Default for UiAudioConfig {
    fn default() -> Self {
        Self {
            selected_input: DEFAULT.to_string(),
            selected_output: DEFAULT.to_string(),
            processing_enabled: true,
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.is_first_update {
            self.is_first_update = false;
            ctx.set_zoom_factor(1.5);
            let ctx = ctx.clone();
            let callback = Box::new(move || ctx.request_repaint());
            self.state.cmd(Command::SetUpdateCallback { callback });
        }
        // on android, add some space at the top.
        #[cfg(target_os = "android")]
        egui::TopBottomPanel::top("my_panel")
            .min_height(40.)
            .show(ctx, |_ui| {});

        self.state.update(ctx);
    }
}

impl App {
    pub fn run(options: NativeOptions) -> Result<(), eframe::Error> {
        let handle = Worker::spawn();
        let devices =
            callme::audio::AudioContext::list_devices_sync().expect("failed to list audio devices");
        let state = AppState {
            section: UiSection::Config,
            remote_node_id: Default::default(),
            worker: handle,
            our_node_id: None,
            devices,
            audio_config: Default::default(),
            calls: Default::default(),
        };

        let app = App {
            state,
            is_first_update: true,
        };
        eframe::run_native("callme", options, Box::new(|_cc| Ok(Box::new(app))))
    }
}
impl AppState {
    fn update(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.worker.event_rx.try_recv() {
            match event {
                Event::EndpointBound(node_id) => {
                    self.our_node_id = Some(node_id);
                }
                Event::SetCallState(node_id, call_state) => {
                    if matches!(call_state, CallState::Aborted) {
                        self.calls.remove(&node_id);
                    } else {
                        self.calls.insert(node_id, call_state);
                    }
                }
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| match self.section {
            UiSection::Config => self.ui_section_config(ui),
            UiSection::Main => self.ui_section_call(ui),
        });
    }

    fn audio_config(&self) -> AudioConfig {
        (&self.audio_config).into()
    }

    fn ui_section_call(&mut self, ui: &mut Ui) {
        ui.heading("Call a remote node");
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                if ui
                    .button("📋 Paste node id")
                    .on_hover_text("Click to paste")
                    .clicked()
                {
                    #[cfg(not(target_os = "android"))]
                    let pasted = {
                        arboard::Clipboard::new()
                            .expect("failed to access clipboard")
                            .get_text()
                            .expect("failed to get text from clipboard")
                    };

                    #[cfg(target_os = "android")]
                    let pasted = {
                        android_clipboard::get_text().expect("failed to get text from clipboard")
                    };

                    let node_id = NodeId::from_str(&pasted);
                    self.remote_node_id = Some(node_id);
                }
            });
            if let Some(node_id) = self.remote_node_id.as_ref() {
                ui.horizontal(|ui| match node_id {
                    Ok(node_id) => {
                        if ui.button("Call").clicked() {
                            self.cmd(Command::Call { node_id: *node_id });
                        }
                        ui.label(fmt_node_id(&node_id.fmt_short()));
                    }
                    Err(err) => {
                        ui.label(fmt_error(&format!("Invalid node id: {err}")));
                    }
                });
            }
        });

        ui.add_space(8.);
        ui.heading("Accept calls");
        if let Some(node_id) = &self.our_node_id {
            ui.horizontal(|ui| {
                ui.label("Our node id:".to_string());
                ui.label(fmt_node_id(&node_id.fmt_short()));
                if ui
                    .button("📋 Copy")
                    .on_hover_text("Click to copy")
                    .clicked()
                {
                    #[cfg(not(target_os = "android"))]
                    {
                        if let Err(err) = arboard::Clipboard::new()
                            .expect("failed to get clipboard")
                            .set_text(node_id.to_string())
                        {
                            warn!("failed to copy text to clipboard: {err}");
                        }
                    }
                    #[cfg(target_os = "android")]
                    if let Err(err) = android_clipboard::set_text(node_id.to_string()) {
                        warn!("failed to copy text to clipboard: {err}");
                    }
                }
            });
        }

        ui.add_space(8.);
        ui.heading("Active calls");
        ui.vertical(|ui| {
            for (node_id, state) in &self.calls {
                let node_id = *node_id;
                ui.horizontal(|ui| {
                    ui.label(fmt_node_id(&node_id.fmt_short()));
                    ui.label(format!("{}", state));
                    if matches!(state, CallState::Incoming) {
                        if ui.button("Accept").clicked() {
                            self.cmd(Command::HandleIncoming {
                                node_id,
                                accept: true,
                            });
                        }
                        if ui.button("Decline").clicked() {
                            self.cmd(Command::HandleIncoming {
                                node_id,
                                accept: false,
                            });
                        }
                    } else if ui.button("Drop").clicked() {
                        self.cmd(Command::Abort { node_id });
                    }
                });
            }
        });
    }

    fn cmd(&self, command: Command) {
        self.worker
            .command_tx
            .send_blocking(command)
            .expect("worker thread is dead");
    }

    fn ui_section_config(&mut self, ui: &mut Ui) {
        ui.heading("Audio config");
        ui.vertical(|ui| {
            egui::ComboBox::from_label("Capture device")
                .selected_text(&self.audio_config.selected_input)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(self.audio_config.selected_input == DEFAULT, DEFAULT)
                        .clicked()
                    {
                        self.audio_config.selected_input = DEFAULT.to_string();
                    }
                    for device in &self.devices.input {
                        if ui
                            .selectable_label(&self.audio_config.selected_input == device, device)
                            .clicked()
                        {
                            self.audio_config.selected_input = device.to_string()
                        }
                    }
                });

            egui::ComboBox::from_label("Playback device")
                .selected_text(&self.audio_config.selected_output)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(self.audio_config.selected_output == DEFAULT, DEFAULT)
                        .clicked()
                    {
                        self.audio_config.selected_output = DEFAULT.to_string();
                    }
                    for device in &self.devices.output {
                        if ui
                            .selectable_label(&self.audio_config.selected_output == device, device)
                            .clicked()
                        {
                            self.audio_config.selected_output = device.to_string()
                        }
                    }
                });

            #[cfg(feature = "audio-processing")]
            ui.checkbox(
                &mut self.audio_config.processing_enabled,
                "Enable echo cancellation",
            );

            if ui.button("Save & start").clicked() {
                let audio_config = self.audio_config();
                self.cmd(Command::SetAudioConfig { audio_config });
                self.section = UiSection::Main;
            }
        });
    }
}

fn fmt_node_id(text: &str) -> RichText {
    let text = format!("{text}…");
    egui::RichText::new(text)
        .underline()
        .family(egui::FontFamily::Monospace)
}

fn fmt_error(text: &str) -> RichText {
    egui::RichText::new(text).color(Color32::LIGHT_RED)
}

enum Event {
    EndpointBound(NodeId),
    SetCallState(NodeId, CallState),
}

#[derive(strum::Display)]
enum CallState {
    Incoming,
    Calling,
    Active,
    Aborted,
}

enum CallInfo {
    Calling,
    Incoming(RtcConnection),
    Active(RtcConnection),
}

type UpdateCallback = Box<dyn Fn() + Send + 'static>;

enum Command {
    SetUpdateCallback { callback: UpdateCallback },
    SetAudioConfig { audio_config: AudioConfig },
    Call { node_id: NodeId },
    HandleIncoming { node_id: NodeId, accept: bool },
    Abort { node_id: NodeId },
}

struct Worker {
    command_rx: Receiver<Command>,
    event_tx: Sender<Event>,
    active_calls: BTreeMap<NodeId, CallInfo>,
    update_callback: Option<UpdateCallback>,
    endpoint: Endpoint,
    handler: RtcProtocol,
    call_tasks: JoinSet<(NodeId, Result<()>)>,
    connect_tasks: JoinSet<(NodeId, Result<(RtcConnection, MediaTrack)>)>,
    _router: Router,
    audio_context: Option<AudioContext>,
}

struct WorkerHandle {
    command_tx: Sender<Command>,
    event_rx: Receiver<Event>,
}

impl Worker {
    pub fn spawn() -> WorkerHandle {
        let (command_tx, command_rx) = async_channel::bounded(16);
        let (event_tx, event_rx) = async_channel::bounded(16);
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
                let mut worker = Worker::start(event_tx, command_rx)
                    .await
                    .expect("worker failed to start");
                if let Err(err) = worker.run().await {
                    warn!("worker stopped with error: {err:?}");
                }
            });
        });
        handle
    }

    async fn emit(&self, event: Event) -> Result<()> {
        self.event_tx.send(event).await?;
        if let Some(callback) = &self.update_callback {
            callback();
        }
        Ok(())
    }

    async fn start(
        event_tx: async_channel::Sender<Event>,
        command_rx: async_channel::Receiver<Command>,
    ) -> Result<Self> {
        let endpoint = callme::net::bind_endpoint().await?;
        let handler = RtcProtocol::new(endpoint.clone());
        let _router = Router::builder(endpoint.clone())
            .accept(RtcProtocol::ALPN, handler.clone())
            .spawn()
            .await?;
        Ok(Self {
            command_rx,
            event_tx,
            active_calls: Default::default(),
            call_tasks: JoinSet::new(),
            connect_tasks: JoinSet::new(),
            endpoint,
            handler,
            _router,
            audio_context: None,
            update_callback: None,
        })
    }

    async fn run(&mut self) -> Result<()> {
        self.emit(Event::EndpointBound(self.endpoint.node_id()))
            .await?;
        loop {
            tokio::select! {
                command = self.command_rx.recv() => {
                    let command = command?;
                    if let Err(err) = self.handle_command(command).await {
                        warn!("command failed: {err}");
                    }
                }
                conn = self.handler.accept() => {
                    let Some(conn) = conn? else {
                        break;
                    };
                    self.handle_incoming(conn).await?;
                }
                Some(res) = self.call_tasks.join_next(), if !self.call_tasks.is_empty() => {
                    let (node_id, res) = res.expect("connection task panicked");
                    if let Err(err) = res {
                        warn!("connection with {} closed: {err:?}", node_id.fmt_short());
                    } else {
                        info!("connection with {} closed", node_id.fmt_short());
                    }
                    self.active_calls.remove(&node_id);
                    self.emit(Event::SetCallState(node_id, CallState::Aborted))
                        .await?;
                }
                Some(res) = self.connect_tasks.join_next(), if !self.connect_tasks.is_empty() => {
                    let (node_id, res) = res.expect("connect task panicked");
                    self.handle_connected(node_id, res).await?;
                }
            }
        }
        Ok(())
    }

    async fn handle_incoming(&mut self, conn: RtcConnection) -> Result<()> {
        let node_id = conn.transport().remote_node_id()?;
        info!("incoming connection from {}", node_id.fmt_short());
        self.active_calls.insert(node_id, CallInfo::Incoming(conn));
        self.emit(Event::SetCallState(node_id, CallState::Incoming))
            .await?;
        Ok(())
    }

    async fn handle_connected(
        &mut self,
        node_id: NodeId,
        conn: Result<(RtcConnection, MediaTrack)>,
    ) -> Result<()> {
        match conn {
            Ok((conn, track)) => {
                self.accept_from_connect(conn, track).await?;
            }
            Err(err) => {
                warn!("connection to {} failed: {err:?}", node_id);
                self.active_calls.remove(&node_id);
                self.emit(Event::SetCallState(node_id, CallState::Aborted))
                    .await?;
            }
        }
        Ok(())
    }

    async fn accept_from_connect(&mut self, conn: RtcConnection, track: MediaTrack) -> Result<()> {
        let node_id = conn.transport().remote_node_id()?;
        self.active_calls
            .insert(node_id, CallInfo::Active(conn.clone()));
        self.emit(Event::SetCallState(node_id, CallState::Active))
            .await?;
        let audio_context = self
            .audio_context
            .clone()
            .context("missing audio context")?;
        self.call_tasks.spawn(async move {
            info!("starting connection with {}", node_id.fmt_short());
            let fut = async {
                audio_context.play_track(track).await?;
                let capture_track = audio_context.capture_track().await?;
                conn.send_track(capture_track).await?;
                #[allow(clippy::redundant_pattern_matching)]
                while let Some(_) = conn.recv_track().await? {}
                anyhow::Ok(())
            };
            let res = fut.await;
            info!("connection with {} closed: {:?}", node_id.fmt_short(), res);
            (node_id, res)
        });
        Ok(())
    }

    async fn accept_from_accept(&mut self, conn: RtcConnection) -> Result<()> {
        let node_id = conn.transport().remote_node_id()?;
        self.active_calls
            .insert(node_id, CallInfo::Active(conn.clone()));
        self.emit(Event::SetCallState(node_id, CallState::Active))
            .await?;
        let audio_context = self
            .audio_context
            .clone()
            .context("missing audio context")?;
        self.call_tasks.spawn(async move {
            info!("starting connection with {}", node_id.fmt_short());
            let fut = async {
                let capture_track = audio_context.capture_track().await?;
                conn.send_track(capture_track).await?;
                info!("added capture track to rtc connection");
                while let Some(remote_track) = conn.recv_track().await? {
                    info!(
                        "new remote track: {:?} {:?}",
                        remote_track.kind(),
                        remote_track.codec()
                    );
                    match remote_track.kind() {
                        TrackKind::Audio => {
                            audio_context.play_track(remote_track).await?;
                        }
                        TrackKind::Video => unimplemented!(),
                    }
                }
                anyhow::Ok(())
            };
            let res = fut.await;
            info!("connection with {} closed: {:?}", node_id.fmt_short(), res);
            (node_id, res)
        });
        Ok(())
    }

    async fn handle_command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::SetUpdateCallback { callback } => {
                self.update_callback = Some(callback);
            }
            Command::SetAudioConfig { audio_config } => {
                let audio_context = AudioContext::new(audio_config).await?;
                self.audio_context = Some(audio_context);
            }
            Command::Call { node_id } => {
                if self.active_calls.contains_key(&node_id) {
                    return Ok(());
                }
                self.active_calls.insert(node_id, CallInfo::Calling);
                self.emit(Event::SetCallState(node_id, CallState::Calling))
                    .await?;

                let handler = self.handler.clone();
                self.connect_tasks.spawn(async move {
                    let fut = async {
                        let conn = handler.connect(node_id).await?;
                        let track = conn.recv_track().await?.ok_or_else(|| {
                            anyhow!("connection closed without receiving a single track")
                        })?;
                        anyhow::Ok((conn, track))
                    };
                    (node_id, fut.await)
                });
            }
            Command::HandleIncoming { node_id, accept } => {
                let Some(CallInfo::Incoming(conn)) = self.active_calls.remove(&node_id) else {
                    return Ok(());
                };
                if accept {
                    self.accept_from_accept(conn).await?;
                } else {
                    conn.transport().close(0u32.into(), b"bye");
                    self.emit(Event::SetCallState(node_id, CallState::Aborted))
                        .await?;
                }
            }
            Command::Abort { node_id } => {
                if let Some(state) = self.active_calls.remove(&node_id) {
                    match state {
                        CallInfo::Calling => {}
                        CallInfo::Active(conn) => {
                            conn.transport().close(0u32.into(), b"bye");
                        }
                        CallInfo::Incoming(conn) => {
                            conn.transport().close(0u32.into(), b"bye");
                        }
                    }
                    self.emit(Event::SetCallState(node_id, CallState::Aborted))
                        .await?;
                }
            }
        }
        Ok(())
    }
}
