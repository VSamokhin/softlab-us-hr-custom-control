// Release builds are GUI applications on Windows; debug builds keep a console
// for diagnostics.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod history;
mod presets;
mod theme;

use std::time::{Duration, Instant};

use eframe::egui;
use history::UndoHistory;
use presets::{PresetCollection, PresetStore};
use us_hr_core::{ChannelMode, DeviceSettings, DeviceStatus, SettingChange};
use us_hr_usb::{
    AccessMode, DeviceInfo, DeviceMonitor, DeviceMonitorEvent, UsbTransport, discover,
};

const UNDO_CAPACITY: usize = 50;
const DEVICE_MONITOR_REPAINT_INTERVAL: Duration = Duration::from_millis(250);
const CONNECTION_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const CONNECTION_RETRY_LIMIT: u8 = 5;

#[cfg(windows)]
const DETECTED_WITHOUT_CONTROL: &str = "The interface was found, but Windows does not yet expose \
     its control path to this application. See the README's Windows section.";
#[cfg(not(windows))]
const DETECTED_WITHOUT_CONTROL: &str =
    "The interface was found, but its control state could not be read.";

fn main() -> eframe::Result {
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([920.0, 760.0])
        .with_min_inner_size([780.0, 650.0]);
    if let Some(icon) = application_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "US-HR Custom Control",
        options,
        Box::new(|creation_context| {
            theme::configure(&creation_context.egui_ctx);
            Ok(Box::new(ControlApp::new()))
        }),
    )
}

fn application_icon() -> Option<egui::IconData> {
    eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/app-icon-256.png")).ok()
}

struct ControlApp {
    devices: Vec<DeviceInfo>,
    error: Option<String>,
    notice: Option<String>,
    status: Option<DeviceStatus>,
    settings: DeviceSettings,
    history: UndoHistory,
    preset_store: Option<PresetStore>,
    presets: PresetCollection,
    preset_name: String,
    selected_preset: Option<usize>,
    device_monitor: Option<DeviceMonitor>,
    next_connection_retry: Option<Instant>,
    connection_retries_remaining: u8,
}

impl ControlApp {
    fn new() -> Self {
        let (preset_store, presets, preset_error) = load_presets();
        let (device_monitor, monitor_error) = match DeviceMonitor::start() {
            Ok(monitor) => (Some(monitor), None),
            Err(error) => (
                None,
                Some(format!("Automatic device detection unavailable: {error}")),
            ),
        };
        let mut app = Self {
            devices: Vec::new(),
            error: None,
            notice: None,
            status: None,
            settings: DeviceSettings::default(),
            history: UndoHistory::new(UNDO_CAPACITY),
            preset_store,
            presets,
            preset_name: String::new(),
            selected_preset: None,
            device_monitor,
            next_connection_retry: None,
            connection_retries_remaining: 0,
        };
        app.refresh();
        if let Some(error) = preset_error.or(monitor_error) {
            app.error = Some(error);
        }
        app
    }

    fn refresh(&mut self) {
        self.notice = None;
        match discover() {
            Ok(devices) => {
                self.devices = devices;
                self.error = None;
            }
            Err(error) => {
                self.devices.clear();
                self.status = None;
                self.error = Some(error.to_string());
                return;
            }
        }
        if self.devices.is_empty() {
            self.status = None;
            return;
        }
        match read_device_status() {
            Ok(status) => self.sync_status(status),
            Err(error) => {
                self.status = None;
                self.error = Some(error.to_string());
            }
        }
    }

    fn handle_device_events(&mut self, events: &[DeviceMonitorEvent]) {
        let previous_devices = self.devices.clone();
        let saw_arrival = events.contains(&DeviceMonitorEvent::Arrived);
        self.refresh();

        if !same_device_set(&previous_devices, &self.devices) {
            self.history.clear();
        }
        if self.status.is_some() {
            self.cancel_connection_retries();
        } else if saw_arrival || !self.devices.is_empty() {
            self.schedule_connection_retries();
        } else {
            self.cancel_connection_retries();
        }
    }

    fn schedule_connection_retries(&mut self) {
        self.connection_retries_remaining = CONNECTION_RETRY_LIMIT;
        self.next_connection_retry = Some(Instant::now() + CONNECTION_RETRY_INTERVAL);
    }

    fn cancel_connection_retries(&mut self) {
        self.connection_retries_remaining = 0;
        self.next_connection_retry = None;
    }

    fn retry_connection_if_due(&mut self) {
        let Some(next_retry) = self.next_connection_retry else {
            return;
        };
        if Instant::now() < next_retry {
            return;
        }

        self.connection_retries_remaining = self.connection_retries_remaining.saturating_sub(1);
        self.refresh();
        if self.status.is_some() || self.connection_retries_remaining == 0 {
            self.cancel_connection_retries();
        } else {
            self.next_connection_retry = Some(Instant::now() + CONNECTION_RETRY_INTERVAL);
        }
    }

    fn apply_change(&mut self, change: SettingChange) {
        let Some(previous) = self.status.as_ref().map(|status| status.settings) else {
            return;
        };
        let result = UsbTransport::open_first(AccessMode::ReadWrite)
            .and_then(|mut transport| transport.apply_setting_verified(change));
        match result {
            Ok(status) => {
                if !change.matches(&previous) {
                    self.history.record(previous);
                }
                self.sync_status(status);
                self.notice = Some(format!("Applied and verified {change:?}."));
                self.error = None;
            }
            Err(error) => self.recover_after_error(format!("Could not verify setting: {error}")),
        }
    }

    fn apply_snapshot(
        &mut self,
        expected: DeviceSettings,
        success_message: String,
        record_previous: bool,
    ) -> bool {
        let Some(previous) = self.status.as_ref().map(|status| status.settings) else {
            return false;
        };
        let result = UsbTransport::open_first(AccessMode::ReadWrite)
            .and_then(|mut transport| transport.apply_settings_verified(expected));
        match result {
            Ok(status) => {
                if record_previous && previous != expected {
                    self.history.record(previous);
                }
                self.sync_status(status);
                self.notice = Some(success_message);
                self.error = None;
                true
            }
            Err(error) => {
                self.recover_after_error(format!("Could not verify settings snapshot: {error}"));
                false
            }
        }
    }

    fn undo(&mut self) {
        let Some(previous) = self.history.pop() else {
            return;
        };
        if !self.apply_snapshot(previous, "Undo applied and verified.".to_owned(), false) {
            self.history.record(previous);
        }
    }

    fn save_preset(&mut self) {
        let Some(settings) = self.status.as_ref().map(|status| status.settings) else {
            return;
        };
        let Some(store) = &self.preset_store else {
            self.error = Some("Preset storage is unavailable.".to_owned());
            return;
        };
        let original = self.presets.clone();
        match self.presets.upsert(&self.preset_name, settings) {
            Ok(index) => match store.save(&self.presets) {
                Ok(()) => {
                    self.selected_preset = Some(index);
                    self.preset_name.clear();
                    self.notice = Some("Preset saved.".to_owned());
                    self.error = None;
                }
                Err(error) => {
                    self.presets = original;
                    self.error = Some(format!("Could not save presets: {error}"));
                }
            },
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    fn apply_selected_preset(&mut self) {
        let Some(index) = self.selected_preset else {
            return;
        };
        let Some(preset) = self.presets.presets.get(index).cloned() else {
            self.error = Some("Selected preset no longer exists.".to_owned());
            self.selected_preset = None;
            return;
        };
        self.apply_snapshot(
            preset.settings,
            format!("Preset {:?} applied and verified.", preset.name),
            true,
        );
    }

    fn delete_selected_preset(&mut self) {
        let Some(index) = self.selected_preset else {
            return;
        };
        let Some(store) = &self.preset_store else {
            self.error = Some("Preset storage is unavailable.".to_owned());
            return;
        };
        let original = self.presets.clone();
        match self.presets.remove(index) {
            Ok(preset) => match store.save(&self.presets) {
                Ok(()) => {
                    self.selected_preset = None;
                    self.notice = Some(format!("Deleted preset {:?}.", preset.name));
                    self.error = None;
                }
                Err(error) => {
                    self.presets = original;
                    self.error = Some(format!("Could not save presets: {error}"));
                }
            },
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    fn sync_status(&mut self, status: DeviceStatus) {
        self.settings = status.settings;
        self.status = Some(status);
    }

    fn recover_after_error(&mut self, message: String) {
        match read_device_status() {
            Ok(status) => self.sync_status(status),
            Err(read_error) => {
                self.status = None;
                self.error = Some(format!(
                    "{message}; status recovery also failed: {read_error}"
                ));
                return;
            }
        }
        self.error = Some(message);
        self.notice = None;
    }
}

impl eframe::App for ControlApp {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        let mut actions = Vec::new();

        context.request_repaint_after(DEVICE_MONITOR_REPAINT_INTERVAL);
        let device_events = self
            .device_monitor
            .as_ref()
            .map_or_else(Vec::new, DeviceMonitor::take_events);
        if !device_events.is_empty() {
            self.handle_device_events(&device_events);
        }
        self.retry_connection_if_due();
        if self.status.is_some()
            && !self.history.is_empty()
            && context.input_mut(|input| {
                input.consume_shortcut(&egui::KeyboardShortcut::new(
                    egui::Modifiers::COMMAND,
                    egui::Key::Z,
                ))
            })
        {
            actions.push(UiAction::Undo);
        }

        egui::TopBottomPanel::bottom("device-status-bar")
            .exact_height(46.0)
            .frame(
                egui::Frame::new()
                    .fill(theme::SURFACE)
                    .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
                    .inner_margin(egui::Margin::symmetric(24, 10)),
            )
            .show(context, |ui| status_bar(ui, self));

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BACKGROUND)
                    .inner_margin(egui::Margin::same(24)),
            )
            .show(context, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        header(
                            ui,
                            connection_state(!self.devices.is_empty(), self.status.is_some()),
                            &mut actions,
                        );
                        ui.add_space(18.0);
                        status_metrics(ui, self);
                        ui.add_space(16.0);

                        ui.add_enabled_ui(self.status.is_some(), |ui| {
                            ui.columns(2, |columns| {
                                theme::card(
                                    &mut columns[0],
                                    "Monitor",
                                    "Direct monitoring",
                                    "Shape the zero-latency input mix sent to your outputs.",
                                    |ui| {
                                        direct_monitor_controls(
                                            ui,
                                            &mut self.settings,
                                            &mut actions,
                                        );
                                    },
                                );
                                theme::card(
                                    &mut columns[1],
                                    "Broadcast",
                                    "Loopback routing",
                                    "Build a clean computer-and-input mix for streaming.",
                                    |ui| {
                                        loopback_controls(ui, &mut self.settings, &mut actions);
                                    },
                                );
                            });
                        });

                        ui.add_space(14.0);
                        power_controls(ui, self, &mut actions);
                        ui.add_space(14.0);
                        preset_controls(ui, self, &mut actions);
                    });
            });
        for action in actions {
            match action {
                UiAction::Change(change) => self.apply_change(change),
                UiAction::Refresh => self.refresh(),
                UiAction::Undo => self.undo(),
                UiAction::SavePreset => self.save_preset(),
                UiAction::ApplyPreset => self.apply_selected_preset(),
                UiAction::DeletePreset => self.delete_selected_preset(),
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum UiAction {
    Change(SettingChange),
    Refresh,
    Undo,
    SavePreset,
    ApplyPreset,
    DeletePreset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionState {
    Offline,
    Detected,
    Ready,
}

const fn connection_state(device_detected: bool, status_ready: bool) -> ConnectionState {
    if status_ready {
        ConnectionState::Ready
    } else if device_detected {
        ConnectionState::Detected
    } else {
        ConnectionState::Offline
    }
}

fn header(ui: &mut egui::Ui, connection: ConnectionState, actions: &mut Vec<UiAction>) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(
                egui::RichText::new("US-HR CONTROL")
                    .size(11.0)
                    .strong()
                    .color(theme::ACCENT),
            );
            ui.label(
                egui::RichText::new("Studio control surface")
                    .size(27.0)
                    .strong()
                    .color(theme::TEXT),
            );
            ui.label(
                egui::RichText::new("Mixer, routing and recall for TASCAM US-HR interfaces")
                    .size(13.0)
                    .color(theme::TEXT_MUTED),
            );
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add_sized([92.0, 36.0], egui::Button::new("Refresh"))
                .clicked()
            {
                actions.push(UiAction::Refresh);
            }
            connection_badge(ui, connection);
        });
    });
}

fn connection_badge(ui: &mut egui::Ui, connection: ConnectionState) {
    let (label, color, fill) = match connection {
        ConnectionState::Ready => ("DEVICE ONLINE", theme::SUCCESS, theme::SUCCESS_BG),
        ConnectionState::Detected => ("DEVICE DETECTED", theme::WARNING, theme::WARNING_BG),
        ConnectionState::Offline => ("NO DEVICE", theme::TEXT_MUTED, theme::SURFACE_RAISED),
    };
    egui::Frame::new()
        .fill(fill)
        .stroke(egui::Stroke::new(1.0_f32, color.gamma_multiply(0.55)))
        .corner_radius(egui::CornerRadius::same(18))
        .inner_margin(egui::Margin::symmetric(12, 7))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (dot, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                ui.painter().circle_filled(dot.center(), 4.0, color);
                ui.label(egui::RichText::new(label).size(10.0).strong().color(color));
            });
        });
}

fn status_metrics(ui: &mut egui::Ui, app: &ControlApp) {
    let device_name = app
        .devices
        .first()
        .map_or("Not connected", |device| device.model.display_name());
    let firmware = app.status.as_ref().map_or_else(
        || "—".to_owned(),
        |status| {
            format!(
                "{} · build {}",
                format_firmware(status.firmware.version),
                status.firmware.build
            )
        },
    );
    let sample_rate = app.status.as_ref().map_or_else(
        || "—".to_owned(),
        |status| format_sample_rate(status.sample_rate_hz),
    );
    ui.columns(3, |columns| {
        theme::metric(&mut columns[0], "Interface", device_name, theme::TEXT);
        theme::metric(&mut columns[1], "Firmware", &firmware, theme::TEXT);
        theme::metric(&mut columns[2], "Sample rate", &sample_rate, theme::ACCENT);
    });
}

fn status_bar(ui: &mut egui::Ui, app: &ControlApp) {
    let (label, message, color) = if let Some(error) = &app.error {
        ("ATTENTION", error.as_str(), theme::ERROR)
    } else if let Some(notice) = &app.notice {
        ("VERIFIED", notice.as_str(), theme::SUCCESS)
    } else if app.status.is_some() {
        (
            "READY",
            "Device state synchronized with hardware.",
            theme::SUCCESS,
        )
    } else if app.devices.is_empty() {
        (
            "OFFLINE",
            "Connect a supported US-HR interface to begin.",
            theme::TEXT_MUTED,
        )
    } else {
        ("ATTENTION", DETECTED_WITHOUT_CONTROL, theme::ERROR)
    };

    ui.horizontal(|ui| {
        let (dot, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
        ui.painter().circle_filled(dot.center(), 4.0, color);
        ui.label(egui::RichText::new(label).size(10.0).strong().color(color));
        ui.separator();
        ui.label(egui::RichText::new(message).size(11.5).color(theme::TEXT));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new("US-HR CUSTOM CONTROL  ·  0.1.0")
                    .size(9.5)
                    .strong()
                    .color(theme::TEXT_MUTED),
            );
        });
    });
}

fn preset_controls(ui: &mut egui::Ui, app: &mut ControlApp, actions: &mut Vec<UiAction>) {
    theme::card(
        ui,
        "Recall",
        "Presets",
        "Capture the verified hardware state, then recall it as one undoable operation.",
        |ui| {
            theme::field_label(ui, "Save current configuration");
            ui.horizontal(|ui| {
                let button_width = 132.0;
                let edit_width = (ui.available_width() - button_width - 10.0).max(180.0);
                ui.add_sized(
                    [edit_width, 36.0],
                    egui::TextEdit::singleline(&mut app.preset_name)
                        .hint_text("Preset name, e.g. Streaming"),
                );
                if ui
                    .add_enabled(
                        app.status.is_some() && app.preset_store.is_some(),
                        egui::Button::new("Save current")
                            .fill(theme::ACCENT_DARK)
                            .min_size(egui::vec2(button_width, 36.0)),
                    )
                    .clicked()
                {
                    actions.push(UiAction::SavePreset);
                }
            });

            ui.add_space(8.0);
            theme::field_label(ui, "Recall saved configuration");
            let selected_name = app
                .selected_preset
                .and_then(|index| app.presets.presets.get(index))
                .map_or("Choose a saved preset", |preset| preset.name.as_str());
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("preset-selector")
                    .width((ui.available_width() - 260.0).max(200.0))
                    .selected_text(selected_name)
                    .show_ui(ui, |ui| {
                        for (index, preset) in app.presets.presets.iter().enumerate() {
                            ui.selectable_value(
                                &mut app.selected_preset,
                                Some(index),
                                &preset.name,
                            );
                        }
                    });
                let can_use_selected = app.status.is_some() && app.selected_preset.is_some();
                if ui
                    .add_enabled(
                        can_use_selected,
                        egui::Button::new("Apply & verify")
                            .fill(theme::ACCENT_DARK)
                            .min_size(egui::vec2(134.0, 36.0)),
                    )
                    .clicked()
                {
                    actions.push(UiAction::ApplyPreset);
                }
                if ui
                    .add_enabled(
                        app.selected_preset.is_some() && app.preset_store.is_some(),
                        egui::Button::new("Delete").min_size(egui::vec2(92.0, 36.0)),
                    )
                    .clicked()
                {
                    actions.push(UiAction::DeletePreset);
                }
            });
        },
    );
}

fn direct_monitor_controls(
    ui: &mut egui::Ui,
    settings: &mut DeviceSettings,
    actions: &mut Vec<UiAction>,
) {
    theme::field_label(ui, "Channel format");
    if mode_buttons(ui, &mut settings.direct_monitor) {
        actions.push(UiAction::Change(SettingChange::DirectMonitor(
            settings.direct_monitor,
        )));
    }
    ui.add_space(8.0);
    theme::field_label(ui, "Input channels");
    ui.horizontal(|ui| {
        let width = ((ui.available_width() - 10.0) / 2.0).max(120.0);
        if toggle_button(ui, "Input 1", &mut settings.input_1_enabled, width) {
            actions.push(UiAction::Change(SettingChange::Input1Enabled(
                settings.input_1_enabled,
            )));
        }
        if toggle_button(ui, "Input 2", &mut settings.input_2_enabled, width) {
            actions.push(UiAction::Change(SettingChange::Input2Enabled(
                settings.input_2_enabled,
            )));
        }
    });
    ui.add_space(8.0);
    theme::field_label(ui, "Monitor balance");
    ui.spacing_mut().slider_width = ui.available_width();
    let response = ui.add(
        egui::Slider::new(&mut settings.monitor_balance, 0..=127)
            .show_value(false)
            .trailing_fill(true),
    );
    if slider_committed(&response) {
        actions.push(UiAction::Change(SettingChange::MonitorBalance(
            settings.monitor_balance,
        )));
    }
    slider_legend(ui, "INPUT", "COMPUTER", settings.monitor_balance);
}

fn loopback_controls(
    ui: &mut egui::Ui,
    settings: &mut DeviceSettings,
    actions: &mut Vec<UiAction>,
) {
    theme::field_label(ui, "Loopback engine");
    let full_width = ui.available_width();
    if toggle_button(
        ui,
        "Loopback enabled",
        &mut settings.loopback_enabled,
        full_width,
    ) {
        actions.push(UiAction::Change(SettingChange::LoopbackEnabled(
            settings.loopback_enabled,
        )));
    }
    ui.add_space(8.0);
    ui.columns(2, |columns| {
        theme::field_label(&mut columns[0], "Input mapping");
        if mode_buttons(&mut columns[0], &mut settings.loopback_input) {
            actions.push(UiAction::Change(SettingChange::LoopbackInput(
                settings.loopback_input,
            )));
        }
        theme::field_label(&mut columns[1], "Computer return");
        if mode_buttons(&mut columns[1], &mut settings.loopback_output) {
            actions.push(UiAction::Change(SettingChange::LoopbackOutput(
                settings.loopback_output,
            )));
        }
    });
    ui.add_space(8.0);
    theme::field_label(ui, "Broadcast level");
    ui.spacing_mut().slider_width = ui.available_width();
    let response = ui.add(
        egui::Slider::new(&mut settings.broadcast_volume, 0..=127)
            .show_value(false)
            .trailing_fill(true),
    );
    if slider_committed(&response) {
        actions.push(UiAction::Change(SettingChange::BroadcastVolume(
            settings.broadcast_volume,
        )));
    }
    slider_legend(ui, "MIN", "MAX", settings.broadcast_volume);
}

fn mode_buttons(ui: &mut egui::Ui, value: &mut ChannelMode) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let width = ((ui.available_width() - 10.0) / 2.0).max(68.0);
        let mono = *value == ChannelMode::Mono;
        if ui
            .add_sized([width, 34.0], egui::Button::new("Mono").selected(mono))
            .clicked()
        {
            *value = ChannelMode::Mono;
            changed = !mono;
        }
        let stereo = *value == ChannelMode::Stereo;
        if ui
            .add_sized([width, 34.0], egui::Button::new("Stereo").selected(stereo))
            .clicked()
        {
            *value = ChannelMode::Stereo;
            changed |= !stereo;
        }
    });
    changed
}

fn toggle_button(ui: &mut egui::Ui, label: &str, value: &mut bool, width: f32) -> bool {
    let state = if *value { "ON" } else { "OFF" };
    let text = format!("{label}   {state}");
    let clicked = ui
        .add_sized([width, 34.0], egui::Button::new(text).selected(*value))
        .clicked();
    if clicked {
        *value = !*value;
    }
    clicked
}

fn slider_legend(ui: &mut egui::Ui, left: &str, right: &str, value: u8) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(left)
                .size(9.5)
                .strong()
                .color(theme::TEXT_MUTED),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new(right)
                    .size(9.5)
                    .strong()
                    .color(theme::TEXT_MUTED),
            );
            ui.label(
                egui::RichText::new(format!("{value:03}"))
                    .monospace()
                    .strong()
                    .color(theme::ACCENT),
            );
        });
    });
}

fn power_controls(ui: &mut egui::Ui, app: &mut ControlApp, actions: &mut Vec<UiAction>) {
    theme::card(
        ui,
        "Device",
        "Power management",
        "Reduce power consumption when the interface is operating standalone.",
        |ui| {
            ui.add_enabled_ui(app.status.is_some(), |ui| {
                if ui
                    .checkbox(
                        &mut app.settings.auto_power_save,
                        "Enable automatic power saving",
                    )
                    .changed()
                {
                    actions.push(UiAction::Change(SettingChange::AutoPowerSave(
                        app.settings.auto_power_save,
                    )));
                }
            });
        },
    );
}

fn slider_committed(response: &egui::Response) -> bool {
    response.drag_stopped() || (response.changed() && !response.dragged())
}

fn read_device_status() -> Result<DeviceStatus, us_hr_usb::UsbError> {
    UsbTransport::open_first_read_only().and_then(|mut transport| transport.read_status())
}

fn same_device_set(left: &[DeviceInfo], right: &[DeviceInfo]) -> bool {
    fn identities(devices: &[DeviceInfo]) -> Vec<(u16, u8, u8)> {
        let mut identities = devices
            .iter()
            .map(|device| (device.model.product_id(), device.bus_number, device.address))
            .collect::<Vec<_>>();
        identities.sort_unstable();
        identities
    }

    identities(left) == identities(right)
}

fn load_presets() -> (Option<PresetStore>, PresetCollection, Option<String>) {
    match PresetStore::for_current_user() {
        Ok(store) => match store.load() {
            Ok(presets) => (Some(store), presets, None),
            Err(error) => (
                Some(store),
                PresetCollection::default(),
                Some(format!("Could not load presets: {error}")),
            ),
        },
        Err(error) => (
            None,
            PresetCollection::default(),
            Some(format!("Preset storage unavailable: {error}")),
        ),
    }
}

fn format_firmware(version: u16) -> String {
    format!("{}.{:02}", version / 100, version % 100)
}

fn format_sample_rate(hz: u32) -> String {
    if hz % 1_000 == 0 {
        format!("{} kHz", hz / 1_000)
    } else {
        format!("{}.{:01} kHz", hz / 1_000, (hz % 1_000) / 100)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConnectionState, connection_state, format_firmware, format_sample_rate, same_device_set,
    };
    use us_hr_core::DeviceModel;
    use us_hr_usb::DeviceInfo;

    fn device(model: DeviceModel, bus_number: u8, address: u8) -> DeviceInfo {
        DeviceInfo {
            model,
            bus_number,
            address,
            usb_device_version: (1, 0, 0),
            product_string: None,
            manufacturer_string: None,
            serial_number: None,
        }
    }

    #[test]
    fn compares_device_sets_without_using_enumeration_order() {
        let first = device(DeviceModel::Us1x2Hr, 1, 2);
        let second = device(DeviceModel::Us2x2Hr, 1, 3);

        assert!(same_device_set(
            &[first.clone(), second.clone()],
            &[second, first]
        ));
        assert!(!same_device_set(&[], &[device(DeviceModel::Us1x2Hr, 1, 2)]));
        assert!(!same_device_set(
            &[
                device(DeviceModel::Us1x2Hr, 1, 2),
                device(DeviceModel::Us1x2Hr, 1, 2),
            ],
            &[
                device(DeviceModel::Us1x2Hr, 1, 2),
                device(DeviceModel::Us2x2Hr, 1, 3),
            ]
        ));
    }

    #[test]
    fn formats_vendor_firmware_number() {
        assert_eq!(format_firmware(100), "1.00");
        assert_eq!(format_firmware(207), "2.07");
    }

    #[test]
    fn formats_sample_rates_compactly() {
        assert_eq!(format_sample_rate(44_100), "44.1 kHz");
        assert_eq!(format_sample_rate(48_000), "48 kHz");
        assert_eq!(format_sample_rate(192_000), "192 kHz");
    }

    #[test]
    fn distinguishes_detection_from_ready_control_access() {
        assert_eq!(connection_state(false, false), ConnectionState::Offline);
        assert_eq!(connection_state(true, false), ConnectionState::Detected);
        assert_eq!(connection_state(true, true), ConnectionState::Ready);
    }
}
