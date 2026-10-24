//! Vendor control access to TASCAM US-HR interfaces through the TEAC Windows
//! driver.
//!
//! Windows' in-box `usbaudio2` driver exposes no path for vendor control
//! requests. The TEAC driver (`us-1x2hr.sys`, installed by the official
//! TASCAM package) does: the vendor Settings Panel opens a private control file
//! on the driver's `KSCATEGORY_RENDER` interface and relays each USB vendor
//! request through one buffered IOCTL. This crate reproduces that path. The
//! request layout below is documented in `docs/protocol.md` and is built by
//! safe code so it can be tested on every platform; only the Windows FFI that
//! sends it is `unsafe`.

use us_hr_core::{DeviceModel, MAX_MESSAGE_LEN, TASCAM_VENDOR_ID};

#[cfg(windows)]
mod ffi;

#[cfg(windows)]
pub use ffi::{ControlFile, control_interfaces};

/// Reference string the TEAC driver serves as its private control file.
pub const CONTROL_REFERENCE: &str = "us-1x2hr";

/// `CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_ANY_ACCESS)`.
pub const IOCTL_VENDOR_REQUEST: u32 = 0x0022_2000;

/// `URB_FUNCTION_VENDOR_DEVICE`: a vendor request to the whole device.
const URB_FUNCTION_VENDOR_DEVICE: u16 = 0x0017;
const HEADER_LEN: usize = 0x10;
const MIN_INPUT_LEN: usize = 0x14;
const DIRECTION_OUT: u32 = 0;
const DIRECTION_IN: u32 = 1;

/// One device interface served by the TEAC driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlInterface {
    /// Supported hardware model identified from the interface path.
    pub model: DeviceModel,
    /// Path of the driver's private control file.
    pub control_path: String,
}

/// Builds the IOCTL input buffer for a host-to-device vendor request.
///
/// # Errors
///
/// Returns the payload length if it exceeds the protocol's 64-byte limit.
pub fn vendor_out_request(
    request: u8,
    value: u16,
    index: u16,
    payload: &[u8],
) -> Result<Vec<u8>, usize> {
    if payload.len() > MAX_MESSAGE_LEN {
        return Err(payload.len());
    }
    let mut buffer = header(DIRECTION_OUT, payload.len(), request, value, index);
    buffer.extend_from_slice(payload);
    buffer.resize(buffer.len().max(MIN_INPUT_LEN), 0);
    Ok(buffer)
}

/// Builds the IOCTL input buffer for a device-to-host vendor request.
///
/// The received data is returned in the IOCTL output buffer, whose size sets
/// the requested transfer length.
#[must_use]
pub fn vendor_in_request(request: u8, value: u16, index: u16) -> Vec<u8> {
    let mut buffer = header(DIRECTION_IN, 0, request, value, index);
    buffer.resize(MIN_INPUT_LEN, 0);
    buffer
}

fn header(direction: u32, length: usize, request: u8, value: u16, index: u16) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(HEADER_LEN + MAX_MESSAGE_LEN);
    buffer.extend_from_slice(&direction.to_le_bytes());
    // Callers bound the payload by MAX_MESSAGE_LEN, so the cast is lossless.
    buffer.extend_from_slice(&u32::try_from(length).unwrap_or(u32::MAX).to_le_bytes());
    buffer.extend_from_slice(&URB_FUNCTION_VENDOR_DEVICE.to_le_bytes());
    buffer.extend_from_slice(&value.to_le_bytes());
    buffer.extend_from_slice(&index.to_le_bytes());
    buffer.push(0);
    buffer.push(request);
    buffer
}

/// Returns the supported model whose audio function an interface path names.
///
/// Paths look like
/// `\\?\usb#vid_0644&pid_806f&mi_00#...#{65e8773e-...}\ksus1x2hr`; matching
/// is case-insensitive and requires the audio function `MI_00`.
#[must_use]
pub fn model_from_interface_path(path: &str) -> Option<DeviceModel> {
    let path = path.to_ascii_lowercase();
    let vendor = format!("vid_{TASCAM_VENDOR_ID:04x}&pid_");
    let start = path.find(&vendor)? + vendor.len();
    let product_id = path.get(start..start + 4)?;
    if !path.get(start + 4..)?.starts_with("&mi_00") {
        return None;
    }
    DeviceModel::from_product_id(u16::from_str_radix(product_id, 16).ok()?)
}

/// Replaces an interface path's reference string with the control file name.
///
/// The vendor panel cuts the path after the interface-class GUID and appends
/// `\us-1x2hr`; a path without a reference string gets one appended.
#[must_use]
pub fn control_path(interface_path: &str) -> Option<String> {
    let class_end = interface_path.rfind('}')? + 1;
    Some(format!(
        "{}\\{CONTROL_REFERENCE}",
        &interface_path[..class_end]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RENDER_PATH: &str = "\\\\?\\usb#vid_0644&pid_806f&mi_00#6&3a27a6a4&0&0000#\
         {65e8773e-8f56-11d0-a3b9-00a0c9223196}\\ksus1x2hr";

    #[test]
    fn encodes_vendor_out_requests_like_the_vendor_panel() -> Result<(), usize> {
        let request = vendor_out_request(0x1d, 0, 0, &[0x31, 0x01, 0x00, 0x00])?;

        assert_eq!(
            request,
            [
                0, 0, 0, 0, // direction: host to device
                4, 0, 0, 0, // payload length
                0x17, 0x00, // URB_FUNCTION_VENDOR_DEVICE
                0, 0, // wValue
                0, 0, // wIndex
                0, 0x1d, // reserved, bRequest
                0x31, 0x01, 0x00, 0x00, // payload
            ]
        );
        Ok(())
    }

    #[test]
    fn pads_short_out_requests_and_rejects_oversized_payloads() -> Result<(), usize> {
        assert_eq!(vendor_out_request(0x1d, 0, 0, &[])?.len(), MIN_INPUT_LEN);
        assert_eq!(
            vendor_out_request(0x1d, 0, 0, &[0; MAX_MESSAGE_LEN])?.len(),
            HEADER_LEN + MAX_MESSAGE_LEN
        );
        assert_eq!(
            vendor_out_request(0x1d, 0, 0, &[0; MAX_MESSAGE_LEN + 1]),
            Err(MAX_MESSAGE_LEN + 1)
        );
        Ok(())
    }

    #[test]
    fn encodes_vendor_in_requests_like_the_vendor_panel() {
        assert_eq!(
            vendor_in_request(0x1e, 0x0102, 0x0304),
            [
                1, 0, 0, 0, // direction: device to host
                0, 0, 0, 0, // length comes from the output buffer
                0x17, 0x00, 0x02, 0x01, 0x04, 0x03, 0, 0x1e, // URB, wValue, wIndex, bRequest
                0, 0, 0, 0, // padding to the driver's minimum input size
            ]
        );
    }

    #[test]
    fn identifies_supported_audio_functions_from_interface_paths() {
        assert_eq!(
            model_from_interface_path(RENDER_PATH),
            Some(DeviceModel::Us1x2Hr)
        );
        assert_eq!(
            model_from_interface_path(&RENDER_PATH.to_ascii_uppercase()),
            Some(DeviceModel::Us1x2Hr)
        );
        assert_eq!(
            model_from_interface_path(&RENDER_PATH.replace("pid_806f", "pid_8071")),
            Some(DeviceModel::Us4x4Hr)
        );
        assert_eq!(
            model_from_interface_path(&RENDER_PATH.replace("mi_00", "mi_01")),
            None
        );
        assert_eq!(
            model_from_interface_path(&RENDER_PATH.replace("vid_0644", "vid_1234")),
            None
        );
        assert_eq!(
            model_from_interface_path(&RENDER_PATH.replace("pid_806f", "pid_0001")),
            None
        );
    }

    #[test]
    fn replaces_the_reference_string_with_the_control_file() {
        let expected = "\\\\?\\usb#vid_0644&pid_806f&mi_00#6&3a27a6a4&0&0000#\
             {65e8773e-8f56-11d0-a3b9-00a0c9223196}\\us-1x2hr";
        assert_eq!(control_path(RENDER_PATH).as_deref(), Some(expected));
        assert_eq!(
            control_path(RENDER_PATH.trim_end_matches("\\ksus1x2hr")).as_deref(),
            Some(expected)
        );
        assert_eq!(control_path("no interface class"), None);
    }
}
