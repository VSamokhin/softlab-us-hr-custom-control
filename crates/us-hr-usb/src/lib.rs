//! Cross-platform USB discovery and transport for TASCAM US-HR devices.

use std::{
    io,
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use rusb::{Context, Device, DeviceHandle, Hotplug, HotplugBuilder, UsbContext};
use thiserror::Error;
use us_hr_core::{
    ChannelMode, DeviceModel, DeviceSettings, DeviceStatus, Field, FieldValue, FirmwareInfo,
    MAX_MESSAGE_LEN, Message, MessageError, REQUEST_RECEIVE, REQUEST_SEND, REQUEST_TYPE_IN,
    REQUEST_TYPE_OUT, SettingChange, SettingsError, TASCAM_VENDOR_ID,
};

const CONTROL_TIMEOUT: Duration = Duration::from_millis(500);
const DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const HOTPLUG_EVENT_TIMEOUT: Duration = Duration::from_millis(250);
const READY_RETRY_DELAY: Duration = Duration::from_millis(10);
const READY_RETRIES: usize = 100;
const RESPONSE_RETRIES: usize = 100;
const FIELD_READY: u8 = 0xa1;
const FIELD_CHANNEL_GROUP: u8 = 0x61;
const FIELD_CHANNEL_INDEX: u8 = 0x62;
const GET_FADER: u8 = 0xc1;
const SET_FADER: u8 = 0x81;
const GET_MUTE: u8 = 0xc3;
const SET_MUTE: u8 = 0x83;
const GET_STEREO: u8 = 0xc6;
const SET_STEREO: u8 = 0x86;

/// USB identity and descriptor information for a connected device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Supported hardware model.
    pub model: DeviceModel,
    /// USB bus number where available.
    pub bus_number: u8,
    /// USB address where available.
    pub address: u8,
    /// Device firmware release number from the USB descriptor.
    pub usb_device_version: (u8, u8, u8),
    /// Product string, when readable without opening the control path.
    pub product_string: Option<String>,
    /// Manufacturer string, when readable without opening the control path.
    pub manufacturer_string: Option<String>,
    /// Serial number, when supplied by the device.
    pub serial_number: Option<String>,
}

/// Background notification source for supported-device connection changes.
///
/// The monitor uses libusb hot-plug callbacks when the active backend supports
/// them and keeps a periodic, read-only enumeration watchdog active to recover
/// from missed callbacks. Enumeration is also the fallback when callbacks are
/// unavailable.
pub struct DeviceMonitor {
    changes: Receiver<DeviceMonitorEvent>,
    shutdown: Sender<()>,
    worker: Option<JoinHandle<()>>,
}

/// A supported USB device topology event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceMonitorEvent {
    /// A supported device was attached.
    Arrived,
    /// A supported device was removed.
    Left,
    /// Periodic enumeration observed a change that a native callback may have
    /// missed.
    Rescan,
}

impl DeviceMonitor {
    /// Starts monitoring for supported-device connection changes.
    ///
    /// # Errors
    ///
    /// Returns an error if a libusb context or the monitor thread cannot be
    /// created. The monitor never opens a device for writing.
    pub fn start() -> Result<Self, UsbError> {
        let context = Context::new()?;
        let has_hotplug = rusb::has_hotplug();
        let (change_sender, changes) = mpsc::channel();
        let (shutdown, shutdown_receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("us-hr-device-monitor".to_owned())
            .spawn(move || {
                if has_hotplug && monitor_hotplug(&context, &change_sender, &shutdown_receiver) {
                    return;
                }
                monitor_by_polling(&change_sender, &shutdown_receiver);
            })
            .map_err(UsbError::MonitorThread)?;

        Ok(Self {
            changes,
            shutdown,
            worker: Some(worker),
        })
    }

    /// Returns all connection changes observed since the previous call.
    #[must_use]
    pub fn take_events(&self) -> Vec<DeviceMonitorEvent> {
        drain_monitor_events(&self.changes)
    }
}

fn drain_monitor_events(changes: &Receiver<DeviceMonitorEvent>) -> Vec<DeviceMonitorEvent> {
    let mut events = Vec::new();
    loop {
        match changes.try_recv() {
            Ok(event) => events.push(event),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return events,
        }
    }
}

impl Drop for DeviceMonitor {
    fn drop(&mut self) {
        let _ = self.shutdown.send(());
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                tracing::warn!("USB device-monitor thread terminated unexpectedly");
            }
        }
    }
}

struct SupportedDeviceHotplug {
    changes: Sender<DeviceMonitorEvent>,
}

impl<T: UsbContext> Hotplug<T> for SupportedDeviceHotplug {
    fn device_arrived(&mut self, device: Device<T>) {
        self.notify_if_supported(&device, DeviceMonitorEvent::Arrived);
    }

    fn device_left(&mut self, device: Device<T>) {
        self.notify_if_supported(&device, DeviceMonitorEvent::Left);
    }
}

impl SupportedDeviceHotplug {
    fn notify_if_supported<T: UsbContext>(&self, device: &Device<T>, event: DeviceMonitorEvent) {
        let is_supported = device.device_descriptor().is_ok_and(|descriptor| {
            descriptor.vendor_id() == TASCAM_VENDOR_ID
                && DeviceModel::from_product_id(descriptor.product_id()).is_some()
        });
        if is_supported {
            let _ = self.changes.send(event);
        }
    }
}

fn monitor_hotplug(
    context: &Context,
    changes: &Sender<DeviceMonitorEvent>,
    shutdown: &Receiver<()>,
) -> bool {
    let mut known_devices = discover().ok().map(|devices| device_identities(&devices));
    let mut last_poll = Instant::now();
    let mut builder = HotplugBuilder::new();
    builder.vendor_id(TASCAM_VENDOR_ID).enumerate(true);
    let registration = builder.register::<Context, _>(
        context,
        Box::new(SupportedDeviceHotplug {
            changes: changes.clone(),
        }),
    );
    let Ok(_registration) = registration else {
        return false;
    };

    loop {
        match shutdown.try_recv() {
            Ok(()) | Err(TryRecvError::Disconnected) => return true,
            Err(TryRecvError::Empty) => {}
        }
        if context.handle_events(Some(HOTPLUG_EVENT_TIMEOUT)).is_err() {
            return false;
        }
        if last_poll.elapsed() >= DEVICE_POLL_INTERVAL {
            poll_device_changes(&mut known_devices, changes);
            last_poll = Instant::now();
        }
    }
}

fn monitor_by_polling(changes: &Sender<DeviceMonitorEvent>, shutdown: &Receiver<()>) {
    let mut known_devices = None;
    loop {
        match shutdown.recv_timeout(DEVICE_POLL_INTERVAL) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }

        poll_device_changes(&mut known_devices, changes);
    }
}

fn poll_device_changes(
    known_devices: &mut Option<Vec<(u16, u8, u8)>>,
    changes: &Sender<DeviceMonitorEvent>,
) {
    let Ok(devices) = discover() else {
        return;
    };
    let current_devices = device_identities(&devices);
    if known_devices.as_ref() != Some(&current_devices) {
        *known_devices = Some(current_devices);
        let _ = changes.send(DeviceMonitorEvent::Rescan);
    }
}

fn device_identities(devices: &[DeviceInfo]) -> Vec<(u16, u8, u8)> {
    let mut identities = devices
        .iter()
        .map(|device| (device.model.product_id(), device.bus_number, device.address))
        .collect::<Vec<_>>();
    identities.sort_unstable();
    identities
}

/// Finds all supported US-HR devices.
///
/// # Errors
///
/// Returns an error if libusb cannot enumerate the USB bus or read a device
/// descriptor.
pub fn discover() -> Result<Vec<DeviceInfo>, UsbError> {
    let context = Context::new()?;
    let devices = context.devices()?;
    let mut found = Vec::new();

    for device in devices.iter() {
        let descriptor = device.device_descriptor()?;
        if descriptor.vendor_id() != TASCAM_VENDOR_ID {
            continue;
        }
        let Some(model) = DeviceModel::from_product_id(descriptor.product_id()) else {
            continue;
        };

        let strings = device.open().ok().map(|handle| {
            (
                handle.read_product_string_ascii(&descriptor).ok(),
                handle.read_manufacturer_string_ascii(&descriptor).ok(),
                handle.read_serial_number_string_ascii(&descriptor).ok(),
            )
        });
        let (product_string, manufacturer_string, serial_number) =
            strings.unwrap_or((None, None, None));
        let version = descriptor.device_version();
        found.push(DeviceInfo {
            model,
            bus_number: device.bus_number(),
            address: device.address(),
            usb_device_version: (version.major(), version.minor(), version.sub_minor()),
            product_string,
            manufacturer_string,
            serial_number,
        });
    }

    Ok(found)
}

/// Controls whether a transport is permitted to mutate device state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    /// Receive operations are allowed; sends are rejected locally.
    ReadOnly,
    /// Sending validated ordinary settings commands is allowed.
    ReadWrite,
}

/// Raw vendor-message channel to one US-HR device.
///
/// Implementations carry one encoded protocol message per call: `send` issues
/// the host-to-device vendor request (`0x40`/`0x1d`) and `receive` issues the
/// device-to-host request (`0xc0`/`0x1e`). Readiness polling, response
/// matching, write policy, and read-back verification live in [`Transport`],
/// so every platform backend shares them.
pub trait ControlChannel {
    /// Sends one encoded message and returns the number of bytes transferred.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend cannot complete the transfer.
    fn send(&mut self, payload: &[u8]) -> Result<usize, ChannelError>;

    /// Receives one encoded message into `buffer` and returns its length.
    ///
    /// # Errors
    ///
    /// Returns an error if the backend cannot complete the transfer.
    fn receive(&mut self, buffer: &mut [u8; MAX_MESSAGE_LEN]) -> Result<usize, ChannelError>;
}

/// Backend-neutral control-channel failure.
#[derive(Debug, Error)]
pub enum ChannelError {
    /// A libusb control transfer failed.
    #[error(transparent)]
    LibUsb(#[from] rusb::Error),
    /// An operating-system I/O request failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl ChannelError {
    /// Returns whether the protocol may retry the transfer after a short delay.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::LibUsb(error) => matches!(
                error,
                rusb::Error::Pipe | rusb::Error::Timeout | rusb::Error::Interrupted
            ),
            Self::Io(error) => matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
            ),
        }
    }
}

/// Control channel backed by a libusb device handle.
pub struct LibusbChannel {
    handle: DeviceHandle<Context>,
}

impl ControlChannel for LibusbChannel {
    fn send(&mut self, payload: &[u8]) -> Result<usize, ChannelError> {
        Ok(self.handle.write_control(
            REQUEST_TYPE_OUT,
            REQUEST_SEND,
            0,
            0,
            payload,
            CONTROL_TIMEOUT,
        )?)
    }

    fn receive(&mut self, buffer: &mut [u8; MAX_MESSAGE_LEN]) -> Result<usize, ChannelError> {
        Ok(self.handle.read_control(
            REQUEST_TYPE_IN,
            REQUEST_RECEIVE,
            0,
            0,
            buffer,
            CONTROL_TIMEOUT,
        )?)
    }
}

/// Protocol connection to one US-HR device over any [`ControlChannel`].
pub struct Transport<C> {
    channel: C,
    info: DeviceInfo,
    access_mode: AccessMode,
}

/// Control channel chosen for the current platform and installed drivers.
pub enum PlatformChannel {
    /// Direct USB control transfers through libusb (macOS and Linux).
    Libusb(LibusbChannel),
    /// Vendor requests relayed by the TEAC Windows driver.
    #[cfg(windows)]
    TeacDriver(teac::TeacDriverChannel),
}

impl ControlChannel for PlatformChannel {
    fn send(&mut self, payload: &[u8]) -> Result<usize, ChannelError> {
        match self {
            Self::Libusb(channel) => channel.send(payload),
            #[cfg(windows)]
            Self::TeacDriver(channel) => channel.send(payload),
        }
    }

    fn receive(&mut self, buffer: &mut [u8; MAX_MESSAGE_LEN]) -> Result<usize, ChannelError> {
        match self {
            Self::Libusb(channel) => channel.receive(buffer),
            #[cfg(windows)]
            Self::TeacDriver(channel) => channel.receive(buffer),
        }
    }
}

/// Protocol connection opened through the platform's control backend.
pub type UsbTransport = Transport<PlatformChannel>;

impl Transport<PlatformChannel> {
    /// Opens the first supported device in read-only mode.
    ///
    /// # Errors
    ///
    /// Returns an error if USB enumeration or opening fails, or if no supported
    /// interface is connected.
    pub fn open_first_read_only() -> Result<Self, UsbError> {
        Self::open_first(AccessMode::ReadOnly)
    }

    /// Opens the first supported device with the requested safety policy.
    ///
    /// # Errors
    ///
    /// Returns an error if USB enumeration or opening fails, or if no supported
    /// interface is connected.
    pub fn open_first(access_mode: AccessMode) -> Result<Self, UsbError> {
        #[cfg(windows)]
        if let Some(transport) = teac::open_first(access_mode)? {
            return Ok(transport);
        }
        Self::open_first_libusb(access_mode)
    }

    fn open_first_libusb(access_mode: AccessMode) -> Result<Self, UsbError> {
        let context = Context::new()?;
        let devices = context.devices()?;
        for device in devices.iter() {
            let descriptor = device.device_descriptor()?;
            if descriptor.vendor_id() != TASCAM_VENDOR_ID {
                continue;
            }
            let Some(model) = DeviceModel::from_product_id(descriptor.product_id()) else {
                continue;
            };
            let handle = device.open().map_err(|source| UsbError::OpenDevice {
                device: model.display_name(),
                source,
            })?;
            let version = descriptor.device_version();
            let info = DeviceInfo {
                model,
                bus_number: device.bus_number(),
                address: device.address(),
                usb_device_version: (version.major(), version.minor(), version.sub_minor()),
                product_string: handle.read_product_string_ascii(&descriptor).ok(),
                manufacturer_string: handle.read_manufacturer_string_ascii(&descriptor).ok(),
                serial_number: handle.read_serial_number_string_ascii(&descriptor).ok(),
            };
            return Ok(Self::new(
                PlatformChannel::Libusb(LibusbChannel { handle }),
                info,
                access_mode,
            ));
        }
        Err(UsbError::NotFound)
    }
}

impl<C: ControlChannel> Transport<C> {
    /// Wraps an already opened control channel with the shared protocol logic.
    #[must_use]
    pub const fn new(channel: C, info: DeviceInfo, access_mode: AccessMode) -> Self {
        Self {
            channel,
            info,
            access_mode,
        }
    }

    /// Returns descriptor information for the open device.
    #[must_use]
    pub const fn info(&self) -> &DeviceInfo {
        &self.info
    }

    /// Sends a non-mutating field query and decodes the device response.
    ///
    /// Query messages contain only the vendor protocol's type-1 query marker,
    /// so this operation remains available through a read-only transport.
    ///
    /// # Errors
    ///
    /// Returns an error if the query cannot be encoded, a USB transfer fails,
    /// or the expected response does not arrive.
    pub fn query(
        &mut self,
        request_field_id: u8,
        response_field_id: u8,
    ) -> Result<Message, UsbError> {
        let mut query = Message::new();
        query.push(Field {
            id: request_field_id,
            value: FieldValue::Query,
        })?;
        self.exchange(&query, |response| {
            response
                .fields()
                .iter()
                .any(|field| field.id == response_field_id)
        })
    }

    /// Reads identity, sample-rate, routing, mixer, and power state.
    ///
    /// # Errors
    ///
    /// Returns an error if a USB query fails or the device returns incomplete
    /// or unknown status data.
    pub fn read_status(&mut self) -> Result<DeviceStatus, UsbError> {
        let identity = self.query(0x31, 0x32)?;
        let firmware = FirmwareInfo {
            version: required_u16(&identity, 0x32)?,
            build: required_u16(&identity, 0x33)?,
            product_name: required_string(&identity, 0x34)?,
            product_signature: required_string(&identity, 0x37)?,
            build_date: required_string(&identity, 0x35)?,
            build_time: required_string(&identity, 0x36)?,
        };
        let sample_rate_hz = required_u32(&self.query(0x18, 0x18)?, 0x18)?;
        let auto_power_save = required_u8(&self.query(0x14, 0x15)?, 0x15)? != 0;

        let direct_monitor = mode_from_wire(self.query_channel(5, 1, GET_STEREO)?)?;
        let input_1_enabled = self.query_channel(4, 1, GET_MUTE)? == 0;
        let input_2_enabled = self.query_channel(4, 2, GET_MUTE)? == 0;
        let monitor_fader = self.query_channel(5, 1, GET_FADER)?;
        SettingChange::MonitorBalance(monitor_fader).validate()?;
        let monitor_balance = 127 - monitor_fader;
        let loopback_enabled = self.query_channel(6, 0, GET_FADER)? != 0;
        let loopback_input = mode_from_wire(self.query_channel(4, 1, GET_STEREO)?)?;
        let loopback_output = mode_from_wire(self.query_channel(3, 1, GET_STEREO)?)?;
        let broadcast_volume = self.query_channel(6, 1, GET_FADER)?;

        let settings = DeviceSettings {
            direct_monitor,
            input_1_enabled,
            input_2_enabled,
            monitor_balance,
            loopback_enabled,
            loopback_input,
            loopback_output,
            broadcast_volume,
            auto_power_save,
        };
        settings.validate()?;

        Ok(DeviceStatus {
            firmware,
            sample_rate_hz,
            settings,
        })
    }

    /// Applies one normal control-panel setting.
    ///
    /// # Errors
    ///
    /// Returns an error for a read-only transport, an out-of-range value, an
    /// encoding failure, or a failed USB transfer.
    pub fn apply_setting(&mut self, change: SettingChange) -> Result<(), UsbError> {
        validate_change_for_write(self.access_mode, change)?;
        match change {
            SettingChange::DirectMonitor(mode) => {
                self.set_channel(5, 1, SET_STEREO, mode_to_wire(mode))
            }
            SettingChange::Input1Enabled(enabled) => {
                self.set_channel(4, 1, SET_MUTE, u8::from(!enabled))
            }
            SettingChange::Input2Enabled(enabled) => {
                self.set_channel(4, 2, SET_MUTE, u8::from(!enabled))
            }
            SettingChange::MonitorBalance(value) => self.set_channel(5, 1, SET_FADER, 127 - value),
            SettingChange::LoopbackEnabled(enabled) => {
                self.set_channel(6, 0, SET_FADER, u8::from(enabled))
            }
            SettingChange::LoopbackInput(mode) => {
                self.set_channel(4, 1, SET_STEREO, mode_to_wire(mode))
            }
            SettingChange::LoopbackOutput(mode) => {
                self.set_channel(3, 1, SET_STEREO, mode_to_wire(mode))
            }
            SettingChange::BroadcastVolume(value) => self.set_channel(6, 1, SET_FADER, value),
            SettingChange::AutoPowerSave(enabled) => {
                let mut message = Message::new();
                message.push(Field {
                    id: 0x15,
                    value: FieldValue::U8(u8::from(enabled)),
                })?;
                self.send_message(&message)
            }
        }
    }

    /// Applies one setting and verifies it by reading the device state back.
    ///
    /// # Errors
    ///
    /// Returns an error if writing or status read-back fails, or if the returned
    /// state does not contain the requested value.
    pub fn apply_setting_verified(
        &mut self,
        change: SettingChange,
    ) -> Result<DeviceStatus, UsbError> {
        self.apply_setting(change)?;
        let status = self.read_status()?;
        if change.matches(&status.settings) {
            Ok(status)
        } else {
            Err(UsbError::VerificationFailed {
                expected: format!("{change:?}"),
                actual: status.settings,
            })
        }
    }

    /// Applies a complete settings snapshot and verifies exact read-back.
    ///
    /// # Errors
    ///
    /// Returns an error if any write or status query fails, or if the returned
    /// state differs from the requested snapshot.
    pub fn apply_settings_verified(
        &mut self,
        expected: DeviceSettings,
    ) -> Result<DeviceStatus, UsbError> {
        validate_snapshot_for_write(self.access_mode, &expected)?;
        for change in expected.changes() {
            self.apply_setting(change)?;
        }
        let status = self.read_status()?;
        if status.settings == expected {
            Ok(status)
        } else {
            Err(UsbError::VerificationFailed {
                expected: format!("{expected:?}"),
                actual: status.settings,
            })
        }
    }

    fn exchange<F>(&mut self, request: &Message, is_expected: F) -> Result<Message, UsbError>
    where
        F: Fn(&Message) -> bool,
    {
        self.wait_until_ready()?;
        self.write_control(&request.encode()?)?;
        for _ in 0..RESPONSE_RETRIES {
            let response = match self
                .receive()
                .and_then(|bytes| Message::decode(&bytes).map_err(UsbError::from))
            {
                Ok(response) => response,
                Err(error) if is_retryable_receive_error(&error) => {
                    tracing::debug!(%error, "retrying transient USB receive failure");
                    thread::sleep(READY_RETRY_DELAY);
                    continue;
                }
                Err(error) => return Err(error),
            };
            tracing::debug!(?response, "received US-HR protocol message");
            if is_expected(&response) {
                return Ok(response);
            }
            thread::sleep(READY_RETRY_DELAY);
        }
        Err(UsbError::ResponseTimeout)
    }

    fn query_channel(&mut self, group: u8, index: u8, command: u8) -> Result<u8, UsbError> {
        let mut request = Message::new();
        request.push(Field {
            id: FIELD_CHANNEL_GROUP,
            value: FieldValue::U8(group),
        })?;
        request.push(Field {
            id: FIELD_CHANNEL_INDEX,
            value: FieldValue::U8(index),
        })?;
        request.push(Field {
            id: command,
            value: FieldValue::Query,
        })?;
        let response = self.exchange(&request, |response| {
            channel_value(response, group, index, command).is_some()
        })?;
        channel_value(&response, group, index, command)
            .ok_or(UsbError::InvalidResponseField(command))
    }

    fn set_channel(
        &mut self,
        group: u8,
        index: u8,
        command: u8,
        value: u8,
    ) -> Result<(), UsbError> {
        let mut message = Message::new();
        for field in [
            Field {
                id: FIELD_CHANNEL_GROUP,
                value: FieldValue::U8(group),
            },
            Field {
                id: FIELD_CHANNEL_INDEX,
                value: FieldValue::U8(index),
            },
            Field {
                id: command,
                value: FieldValue::U8(value),
            },
        ] {
            message.push(field)?;
        }
        self.send_message(&message)
    }

    fn send_message(&mut self, message: &Message) -> Result<(), UsbError> {
        self.wait_until_ready()?;
        self.write_control(&message.encode()?)
    }

    fn wait_until_ready(&mut self) -> Result<(), UsbError> {
        for _ in 0..READY_RETRIES {
            let message = match self
                .receive()
                .and_then(|bytes| Message::decode(&bytes).map_err(UsbError::from))
            {
                Ok(message) => message,
                Err(error) if is_retryable_receive_error(&error) => {
                    tracing::debug!(%error, "retrying transient USB readiness failure");
                    thread::sleep(READY_RETRY_DELAY);
                    continue;
                }
                Err(error) => return Err(error),
            };
            match readiness_state(&message) {
                Some(true) => return Ok(()),
                Some(false) | None => thread::sleep(READY_RETRY_DELAY),
            }
        }
        Err(UsbError::ReadyTimeout)
    }

    fn write_control(&mut self, payload: &[u8]) -> Result<(), UsbError> {
        if payload.len() > MAX_MESSAGE_LEN {
            return Err(UsbError::MessageTooLong(payload.len()));
        }
        let transferred = self.channel.send(payload)?;
        if transferred != payload.len() {
            return Err(UsbError::ShortWrite {
                expected: payload.len(),
                actual: transferred,
            });
        }
        Ok(())
    }

    fn receive(&mut self) -> Result<Vec<u8>, UsbError> {
        let mut buffer = [0_u8; MAX_MESSAGE_LEN];
        let transferred = self.channel.receive(&mut buffer)?;
        Ok(buffer[..transferred.min(MAX_MESSAGE_LEN)].to_vec())
    }
}

#[cfg(windows)]
pub use teac::TeacDriverChannel;

#[cfg(windows)]
mod teac {
    //! Control channel through the TEAC driver installed by the TASCAM package.

    use us_hr_core::{MAX_MESSAGE_LEN, REQUEST_RECEIVE, REQUEST_SEND};
    use us_hr_windows::{ControlFile, control_interfaces};

    use super::{
        AccessMode, ChannelError, ControlChannel, DeviceInfo, PlatformChannel, Transport, UsbError,
        UsbTransport, discover,
    };

    /// Vendor requests relayed by the TEAC driver's private control file.
    pub struct TeacDriverChannel(ControlFile);

    impl ControlChannel for TeacDriverChannel {
        fn send(&mut self, payload: &[u8]) -> Result<usize, ChannelError> {
            Ok(self.0.vendor_out(REQUEST_SEND, 0, 0, payload)?)
        }

        fn receive(&mut self, buffer: &mut [u8; MAX_MESSAGE_LEN]) -> Result<usize, ChannelError> {
            Ok(self.0.vendor_in(REQUEST_RECEIVE, 0, 0, buffer)?)
        }
    }

    /// Opens the first device served by the TEAC driver, if any.
    ///
    /// Returns `Ok(None)` when no TEAC driver interface is present, so the
    /// caller can fall back to libusb.
    pub fn open_first(access_mode: AccessMode) -> Result<Option<UsbTransport>, UsbError> {
        let interfaces = match control_interfaces() {
            Ok(interfaces) => interfaces,
            Err(error) => {
                tracing::warn!(%error, "could not enumerate TASCAM driver interfaces");
                return Ok(None);
            }
        };
        // The in-box usbaudio2 driver registers the same render interface but
        // has no control file, so a missing file means "not the TEAC driver".
        let mut opened = None;
        for interface in interfaces {
            match ControlFile::open(&interface.control_path) {
                Ok(file) => {
                    opened = Some((interface, file));
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tracing::debug!(path = %interface.control_path, "no TASCAM driver control file");
                }
                Err(source) => {
                    return Err(UsbError::OpenDriverControl {
                        device: interface.model.display_name(),
                        source,
                    });
                }
            }
        }
        let Some((interface, file)) = opened else {
            return Ok(None);
        };
        let info = discover()
            .ok()
            .and_then(|devices| {
                devices
                    .into_iter()
                    .find(|device| device.model == interface.model)
            })
            .unwrap_or(DeviceInfo {
                model: interface.model,
                bus_number: 0,
                address: 0,
                usb_device_version: (0, 0, 0),
                product_string: None,
                manufacturer_string: None,
                serial_number: None,
            });
        tracing::debug!(path = %interface.control_path, "opened TASCAM driver control file");
        Ok(Some(Transport::new(
            PlatformChannel::TeacDriver(TeacDriverChannel(file)),
            info,
            access_mode,
        )))
    }
}

/// USB discovery or control-transfer failure.
#[derive(Debug, Error)]
pub enum UsbError {
    /// Underlying libusb failure.
    #[error(transparent)]
    LibUsb(#[from] rusb::Error),
    /// A control transfer on the open device failed.
    #[error(transparent)]
    Channel(#[from] ChannelError),
    /// The background device-monitor thread could not be started.
    #[error("failed to start USB device monitor: {0}")]
    MonitorThread(#[source] std::io::Error),
    /// The typed device message was malformed or exceeded its limit.
    #[error(transparent)]
    Message(#[from] MessageError),
    /// A setting value was outside the device-defined range.
    #[error(transparent)]
    Settings(#[from] SettingsError),
    /// No supported device was found.
    #[error("no supported TASCAM US-HR device found")]
    NotFound,
    /// A supported device was enumerated, but its control path could not be opened.
    #[error("{device} was detected, but its USB control path could not be opened: {source}")]
    OpenDevice {
        /// Display name of the detected device.
        device: &'static str,
        /// Error reported by the active libusb backend.
        #[source]
        source: rusb::Error,
    },
    /// The TEAC Windows driver serves the device, but refused to open its
    /// control file.
    #[error(
        "{device} was detected, but the TASCAM driver's control path could not be opened: {source}"
    )]
    OpenDriverControl {
        /// Display name of the detected device.
        device: &'static str,
        /// Error reported by the operating system.
        #[source]
        source: io::Error,
    },
    /// A write was attempted through a read-only transport.
    #[error("device writes are disabled by the transport safety policy")]
    ReadOnly,
    /// The caller attempted to send more than 64 bytes.
    #[error("message is {0} bytes; the device limit is 64")]
    MessageTooLong(usize),
    /// libusb reported a successful but incomplete write.
    #[error("short USB write: expected {expected} bytes, transferred {actual}")]
    ShortWrite {
        /// Requested byte count.
        expected: usize,
        /// Actual byte count.
        actual: usize,
    },
    /// The device did not report ready before the retry limit.
    #[error("device did not become ready within the protocol retry limit")]
    ReadyTimeout,
    /// The requested response field did not arrive before the retry limit.
    #[error("device did not return the expected response within the protocol retry limit")]
    ResponseTimeout,
    /// A required response field was absent or had the wrong data type.
    #[error("missing or invalid response field {0:#04x}")]
    InvalidResponseField(u8),
    /// The device returned an unknown stereo-mode value.
    #[error("unknown channel mode value {0}")]
    UnknownChannelMode(u8),
    /// A successful write was not reflected in the subsequent device read-back.
    #[error("read-back verification failed; expected {expected}, actual {actual:?}")]
    VerificationFailed {
        /// Requested setting or snapshot.
        expected: String,
        /// Snapshot returned by the device.
        actual: DeviceSettings,
    },
}

fn field_u8(message: &Message, id: u8) -> Option<u8> {
    message.fields().iter().find_map(|field| {
        if field.id == id {
            if let FieldValue::U8(value) = &field.value {
                return Some(*value);
            }
        }
        None
    })
}

fn readiness_state(message: &Message) -> Option<bool> {
    message.fields().iter().find_map(|field| {
        if field.id != FIELD_READY {
            return None;
        }
        match &field.value {
            FieldValue::U16(value) => Some(*value != 0),
            FieldValue::U8(value) => Some(*value != 0),
            _ => Some(false),
        }
    })
}

fn channel_value(message: &Message, group: u8, index: u8, command: u8) -> Option<u8> {
    let mut current_group = None;
    let mut current_index = None;
    for field in message.fields() {
        match (field.id, &field.value) {
            (FIELD_CHANNEL_GROUP, FieldValue::U8(value)) => current_group = Some(*value),
            (FIELD_CHANNEL_INDEX, FieldValue::U8(value)) => current_index = Some(*value),
            (field_id, FieldValue::U8(value))
                if field_id == command
                    && current_group == Some(group)
                    && current_index == Some(index) =>
            {
                return Some(*value);
            }
            _ => {}
        }
    }
    None
}

fn required_u8(message: &Message, id: u8) -> Result<u8, UsbError> {
    field_u8(message, id).ok_or(UsbError::InvalidResponseField(id))
}

fn required_u16(message: &Message, id: u8) -> Result<u16, UsbError> {
    message
        .fields()
        .iter()
        .find_map(|field| match (&field.id, &field.value) {
            (field_id, FieldValue::U16(value)) if *field_id == id => Some(*value),
            _ => None,
        })
        .ok_or(UsbError::InvalidResponseField(id))
}

fn required_u32(message: &Message, id: u8) -> Result<u32, UsbError> {
    message
        .fields()
        .iter()
        .find_map(|field| match (&field.id, &field.value) {
            (field_id, FieldValue::U32(value)) if *field_id == id => Some(*value),
            _ => None,
        })
        .ok_or(UsbError::InvalidResponseField(id))
}

fn required_string(message: &Message, id: u8) -> Result<String, UsbError> {
    message
        .fields()
        .iter()
        .find_map(|field| match (&field.id, &field.value) {
            (field_id, FieldValue::Bytes(value)) if *field_id == id => {
                let end = value
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(value.len());
                Some(String::from_utf8_lossy(&value[..end]).into_owned())
            }
            _ => None,
        })
        .ok_or(UsbError::InvalidResponseField(id))
}

fn mode_from_wire(value: u8) -> Result<ChannelMode, UsbError> {
    match value {
        1 => Ok(ChannelMode::Mono),
        4 => Ok(ChannelMode::Stereo),
        other => Err(UsbError::UnknownChannelMode(other)),
    }
}

const fn mode_to_wire(mode: ChannelMode) -> u8 {
    match mode {
        ChannelMode::Mono => 1,
        ChannelMode::Stereo => 4,
    }
}

fn validate_change_for_write(
    access_mode: AccessMode,
    change: SettingChange,
) -> Result<(), UsbError> {
    if access_mode != AccessMode::ReadWrite {
        return Err(UsbError::ReadOnly);
    }
    change.validate()?;
    Ok(())
}

fn validate_snapshot_for_write(
    access_mode: AccessMode,
    settings: &DeviceSettings,
) -> Result<(), UsbError> {
    if access_mode != AccessMode::ReadWrite {
        return Err(UsbError::ReadOnly);
    }
    settings.validate()?;
    Ok(())
}

fn is_retryable_receive_error(error: &UsbError) -> bool {
    matches!(error, UsbError::Channel(error) if error.is_transient())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn device_identity_comparison_ignores_enumeration_order() {
        let first = device(DeviceModel::Us1x2Hr, 1, 2);
        let second = device(DeviceModel::Us2x2Hr, 1, 3);

        assert_eq!(
            device_identities(&[first.clone(), second.clone()]),
            device_identities(&[second, first])
        );
    }

    #[test]
    fn device_identity_comparison_detects_connection_changes() {
        let connected = device(DeviceModel::Us1x2Hr, 1, 2);

        assert_ne!(device_identities(&[]), device_identities(&[connected]));
    }

    #[test]
    fn preserves_back_to_back_disconnect_and_reconnect_events() {
        let (sender, receiver) = mpsc::channel();
        assert!(sender.send(DeviceMonitorEvent::Left).is_ok());
        assert!(sender.send(DeviceMonitorEvent::Arrived).is_ok());

        assert_eq!(
            drain_monitor_events(&receiver),
            vec![DeviceMonitorEvent::Left, DeviceMonitorEvent::Arrived]
        );
    }

    #[test]
    fn selects_value_from_repeated_channel_groups() -> Result<(), MessageError> {
        let mut message = Message::new();
        for (id, value) in [
            (FIELD_CHANNEL_GROUP, 4),
            (FIELD_CHANNEL_INDEX, 1),
            (GET_MUTE, 0),
            (FIELD_CHANNEL_GROUP, 4),
            (FIELD_CHANNEL_INDEX, 2),
            (GET_MUTE, 1),
        ] {
            message.push(Field {
                id,
                value: FieldValue::U8(value),
            })?;
        }
        assert_eq!(channel_value(&message, 4, 1, GET_MUTE), Some(0));
        assert_eq!(channel_value(&message, 4, 2, GET_MUTE), Some(1));
        assert_eq!(channel_value(&message, 4, 3, GET_MUTE), None);
        Ok(())
    }

    #[test]
    fn maps_observed_channel_modes() {
        assert_eq!(mode_from_wire(1).ok(), Some(ChannelMode::Mono));
        assert_eq!(mode_from_wire(4).ok(), Some(ChannelMode::Stereo));
        assert!(mode_from_wire(2).is_err());
        assert_eq!(mode_to_wire(ChannelMode::Mono), 1);
        assert_eq!(mode_to_wire(ChannelMode::Stereo), 4);
    }

    #[test]
    fn prevalidates_complete_snapshots_before_writing() {
        let invalid = DeviceSettings {
            broadcast_volume: 128,
            ..DeviceSettings::default()
        };

        assert!(matches!(
            validate_snapshot_for_write(AccessMode::ReadWrite, &invalid),
            Err(UsbError::Settings(
                SettingsError::BroadcastVolumeOutOfRange(128)
            ))
        ));
        assert!(matches!(
            validate_snapshot_for_write(AccessMode::ReadOnly, &invalid),
            Err(UsbError::ReadOnly)
        ));
    }

    #[test]
    fn readiness_requires_an_explicit_nonzero_ready_field() -> Result<(), MessageError> {
        let missing = Message::new();
        let mut false_u8 = Message::new();
        false_u8.push(Field {
            id: FIELD_READY,
            value: FieldValue::U8(0),
        })?;
        let mut true_u16 = Message::new();
        true_u16.push(Field {
            id: FIELD_READY,
            value: FieldValue::U16(1),
        })?;
        let mut wrong_type = Message::new();
        wrong_type.push(Field {
            id: FIELD_READY,
            value: FieldValue::Query,
        })?;

        assert_eq!(readiness_state(&missing), None);
        assert_eq!(readiness_state(&false_u8), Some(false));
        assert_eq!(readiness_state(&true_u16), Some(true));
        assert_eq!(readiness_state(&wrong_type), Some(false));
        Ok(())
    }

    #[test]
    fn retries_only_temporary_receive_errors() {
        assert!(is_retryable_receive_error(&UsbError::Channel(
            ChannelError::LibUsb(rusb::Error::Pipe)
        )));
        assert!(is_retryable_receive_error(&UsbError::Channel(
            ChannelError::Io(io::Error::from(io::ErrorKind::TimedOut))
        )));
        assert!(!is_retryable_receive_error(&UsbError::Channel(
            ChannelError::LibUsb(rusb::Error::Access)
        )));
        assert!(!is_retryable_receive_error(&UsbError::Channel(
            ChannelError::Io(io::Error::from(io::ErrorKind::PermissionDenied))
        )));
        assert!(!is_retryable_receive_error(&UsbError::LibUsb(
            rusb::Error::Pipe
        )));
    }

    /// In-memory device that answers with the mappings in `docs/protocol.md`.
    struct SimulatedDevice {
        settings: DeviceSettings,
        pending: std::collections::VecDeque<Vec<u8>>,
        acknowledgements_before_response: usize,
        transient_failures: usize,
        ignore_writes: bool,
        sent: Vec<Vec<u8>>,
    }

    impl SimulatedDevice {
        fn new(settings: DeviceSettings) -> Self {
            Self {
                settings,
                pending: std::collections::VecDeque::new(),
                acknowledgements_before_response: 0,
                transient_failures: 0,
                ignore_writes: false,
                sent: Vec::new(),
            }
        }

        fn channel_value(&self, group: u8, index: u8, command: u8) -> Option<u8> {
            let settings = &self.settings;
            match (group, index, command) {
                (5, 1, GET_STEREO) => Some(mode_to_wire(settings.direct_monitor)),
                (4, 1, GET_MUTE) => Some(u8::from(!settings.input_1_enabled)),
                (4, 2, GET_MUTE) => Some(u8::from(!settings.input_2_enabled)),
                (5, 1, GET_FADER) => Some(127 - settings.monitor_balance),
                (6, 0, GET_FADER) => Some(u8::from(settings.loopback_enabled)),
                (4, 1, GET_STEREO) => Some(mode_to_wire(settings.loopback_input)),
                (3, 1, GET_STEREO) => Some(mode_to_wire(settings.loopback_output)),
                (6, 1, GET_FADER) => Some(settings.broadcast_volume),
                _ => None,
            }
        }

        fn apply_channel_write(&mut self, group: u8, index: u8, command: u8, value: u8) {
            if self.ignore_writes {
                return;
            }
            let settings = &mut self.settings;
            let mode = mode_from_wire(value).ok();
            match (group, index, command) {
                (5, 1, SET_STEREO) => settings.direct_monitor = mode.unwrap_or_default(),
                (4, 1, SET_MUTE) => settings.input_1_enabled = value == 0,
                (4, 2, SET_MUTE) => settings.input_2_enabled = value == 0,
                (5, 1, SET_FADER) => settings.monitor_balance = 127 - value,
                (6, 0, SET_FADER) => settings.loopback_enabled = value != 0,
                (4, 1, SET_STEREO) => settings.loopback_input = mode.unwrap_or_default(),
                (3, 1, SET_STEREO) => settings.loopback_output = mode.unwrap_or_default(),
                (6, 1, SET_FADER) => settings.broadcast_volume = value,
                _ => {}
            }
        }

        fn respond(&mut self, request: &Message) -> Result<Option<Message>, MessageError> {
            let mut response = Message::new();
            let fields = request.fields();
            match fields {
                [
                    Field {
                        id: 0x31,
                        value: FieldValue::Query,
                    },
                ] => {
                    response = identity_response()?;
                }
                [
                    Field {
                        id: 0x18,
                        value: FieldValue::Query,
                    },
                ] => {
                    response.push(Field {
                        id: 0x18,
                        value: FieldValue::U32(48_000),
                    })?;
                }
                [
                    Field {
                        id: 0x14,
                        value: FieldValue::Query,
                    },
                ] => {
                    response.push(Field {
                        id: 0x15,
                        value: FieldValue::U8(u8::from(self.settings.auto_power_save)),
                    })?;
                }
                [
                    Field {
                        id: 0x15,
                        value: FieldValue::U8(value),
                    },
                ] => {
                    if !self.ignore_writes {
                        self.settings.auto_power_save = *value != 0;
                    }
                    return Ok(None);
                }
                [
                    Field {
                        id: FIELD_CHANNEL_GROUP,
                        value: FieldValue::U8(group),
                    },
                    Field {
                        id: FIELD_CHANNEL_INDEX,
                        value: FieldValue::U8(index),
                    },
                    Field { id: command, value },
                ] => match value {
                    FieldValue::Query => {
                        let Some(current) = self.channel_value(*group, *index, *command) else {
                            return Ok(None);
                        };
                        for field in [
                            Field {
                                id: FIELD_CHANNEL_GROUP,
                                value: FieldValue::U8(*group),
                            },
                            Field {
                                id: FIELD_CHANNEL_INDEX,
                                value: FieldValue::U8(*index),
                            },
                            Field {
                                id: *command,
                                value: FieldValue::U8(current),
                            },
                        ] {
                            response.push(field)?;
                        }
                    }
                    FieldValue::U8(new_value) => {
                        self.apply_channel_write(*group, *index, *command, *new_value);
                        return Ok(None);
                    }
                    _ => return Ok(None),
                },
                _ => return Ok(None),
            }
            Ok(Some(response))
        }
    }

    fn identity_response() -> Result<Message, MessageError> {
        let mut response = Message::new();
        for (id, value) in [
            (0x32, FieldValue::U16(100)),
            (0x33, FieldValue::U16(14)),
            (0x34, FieldValue::Bytes(b"US-1x2HR".to_vec())),
            (0x35, FieldValue::Bytes(b"2023/01/01".to_vec())),
            (0x36, FieldValue::Bytes(b"12:00".to_vec())),
            (0x37, FieldValue::Bytes(b"TASCAM".to_vec())),
        ] {
            response.push(Field { id, value })?;
        }
        Ok(response)
    }

    fn ready_message() -> Result<Vec<u8>, MessageError> {
        let mut message = Message::new();
        message.push(Field {
            id: FIELD_READY,
            value: FieldValue::U8(1),
        })?;
        message.encode()
    }

    impl ControlChannel for SimulatedDevice {
        fn send(&mut self, payload: &[u8]) -> Result<usize, ChannelError> {
            self.sent.push(payload.to_vec());
            let request = Message::decode(payload)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let response = self
                .respond(&request)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if let Some(response) = response {
                let encoded = response
                    .encode()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                for _ in 0..self.acknowledgements_before_response {
                    self.pending.push_back(
                        ready_message()
                            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
                    );
                }
                self.pending.push_back(encoded);
            }
            Ok(payload.len())
        }

        fn receive(&mut self, buffer: &mut [u8; MAX_MESSAGE_LEN]) -> Result<usize, ChannelError> {
            if self.transient_failures > 0 {
                self.transient_failures -= 1;
                return Err(ChannelError::LibUsb(rusb::Error::Timeout));
            }
            let bytes = match self.pending.pop_front() {
                Some(bytes) => bytes,
                None => ready_message()
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
            };
            buffer[..bytes.len()].copy_from_slice(&bytes);
            Ok(bytes.len())
        }
    }

    fn simulated_settings() -> DeviceSettings {
        DeviceSettings {
            direct_monitor: ChannelMode::Stereo,
            input_1_enabled: true,
            input_2_enabled: false,
            monitor_balance: 40,
            loopback_enabled: true,
            loopback_input: ChannelMode::Mono,
            loopback_output: ChannelMode::Stereo,
            broadcast_volume: 77,
            auto_power_save: false,
        }
    }

    fn simulated_transport(
        simulated: SimulatedDevice,
        access_mode: AccessMode,
    ) -> Transport<SimulatedDevice> {
        Transport::new(simulated, device(DeviceModel::Us1x2Hr, 1, 2), access_mode)
    }

    #[test]
    fn reads_complete_status_through_any_control_channel() -> Result<(), UsbError> {
        let mut transport = simulated_transport(
            SimulatedDevice::new(simulated_settings()),
            AccessMode::ReadOnly,
        );

        let status = transport.read_status()?;

        assert_eq!(status.settings, simulated_settings());
        assert_eq!(status.sample_rate_hz, 48_000);
        assert_eq!(status.firmware.version, 100);
        assert_eq!(status.firmware.build, 14);
        assert_eq!(status.firmware.product_name, "US-1x2HR");
        Ok(())
    }

    #[test]
    fn skips_acknowledgements_and_transient_failures() -> Result<(), UsbError> {
        let mut device = SimulatedDevice::new(simulated_settings());
        device.acknowledgements_before_response = 2;
        device.transient_failures = 3;
        let mut transport = simulated_transport(device, AccessMode::ReadOnly);

        assert_eq!(transport.read_status()?.settings, simulated_settings());
        Ok(())
    }

    #[test]
    fn read_only_transport_never_sends_writes() {
        let mut transport = simulated_transport(
            SimulatedDevice::new(simulated_settings()),
            AccessMode::ReadOnly,
        );

        assert!(matches!(
            transport.apply_setting(SettingChange::BroadcastVolume(10)),
            Err(UsbError::ReadOnly)
        ));
        assert!(matches!(
            transport.apply_settings_verified(DeviceSettings::default()),
            Err(UsbError::ReadOnly)
        ));
        assert_eq!(transport.channel.sent, Vec::<Vec<u8>>::new());
    }

    #[test]
    fn verified_writes_are_read_back() -> Result<(), UsbError> {
        let mut transport = simulated_transport(
            SimulatedDevice::new(simulated_settings()),
            AccessMode::ReadWrite,
        );

        let status = transport.apply_setting_verified(SettingChange::BroadcastVolume(12))?;
        assert_eq!(status.settings.broadcast_volume, 12);

        let expected = DeviceSettings {
            monitor_balance: 100,
            auto_power_save: true,
            ..DeviceSettings::default()
        };
        let status = transport.apply_settings_verified(expected)?;
        assert_eq!(status.settings, expected);
        Ok(())
    }

    #[test]
    fn reports_writes_missing_from_read_back() {
        let mut device = SimulatedDevice::new(simulated_settings());
        device.ignore_writes = true;
        let mut transport = simulated_transport(device, AccessMode::ReadWrite);

        assert!(matches!(
            transport.apply_setting_verified(SettingChange::BroadcastVolume(12)),
            Err(UsbError::VerificationFailed { .. })
        ));
    }
}
