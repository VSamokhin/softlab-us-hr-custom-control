//! `SetupAPI` enumeration and IOCTL transfers for the TEAC driver.
//!
//! This is the project's only `unsafe` code. Each block calls one Win32 API
//! with buffers owned by the surrounding safe function.

#![allow(unsafe_code)]

use std::{ffi::OsString, io, mem, os::windows::ffi::OsStringExt, ptr};

use windows_sys::{
    Win32::{
        Devices::DeviceAndDriverInstallation::{
            DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SP_DEVICE_INTERFACE_DATA,
            SP_DEVICE_INTERFACE_DETAIL_DATA_W, SetupDiDestroyDeviceInfoList,
            SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
        },
        Foundation::{
            CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_ITEMS, GENERIC_READ,
            GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
        },
        Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING},
        System::IO::DeviceIoControl,
    },
    core::GUID,
};

use us_hr_core::MAX_MESSAGE_LEN;

use crate::{
    ControlInterface, IOCTL_VENDOR_REQUEST, control_path, model_from_interface_path,
    vendor_in_request, vendor_out_request,
};

/// `KSCATEGORY_RENDER`, the interface class the vendor panel enumerates.
const KSCATEGORY_RENDER: GUID = GUID::from_u128(0x65e8_773e_8f56_11d0_a3b9_00a0_c922_3196);

/// Lists the supported devices whose render interface the TEAC driver serves.
///
/// # Errors
///
/// Returns an error if `SetupAPI` cannot enumerate present interfaces.
pub fn control_interfaces() -> io::Result<Vec<ControlInterface>> {
    let devices = DeviceInfoSet::present_interfaces(&KSCATEGORY_RENDER)?;
    let mut found = Vec::new();
    for index in 0.. {
        let Some(path) = devices.interface_path(&KSCATEGORY_RENDER, index)? else {
            break;
        };
        let Some(model) = model_from_interface_path(&path) else {
            continue;
        };
        let Some(control_path) = control_path(&path) else {
            continue;
        };
        if !found
            .iter()
            .any(|known: &ControlInterface| known.control_path == control_path)
        {
            found.push(ControlInterface {
                model,
                control_path,
            });
        }
    }
    Ok(found)
}

struct DeviceInfoSet(HDEVINFO);

impl DeviceInfoSet {
    fn present_interfaces(class: &GUID) -> io::Result<Self> {
        // SAFETY: `class` outlives the call; a null enumerator and window are
        // permitted by SetupDiGetClassDevsW.
        let handle = unsafe {
            SetupDiGetClassDevsW(
                class,
                ptr::null(),
                ptr::null_mut(),
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )
        };
        if handle == INVALID_HANDLE_VALUE as HDEVINFO {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(handle))
    }

    fn interface_path(&self, class: &GUID, index: u32) -> io::Result<Option<String>> {
        let mut interface = SP_DEVICE_INTERFACE_DATA {
            cbSize: size_of_u32::<SP_DEVICE_INTERFACE_DATA>(),
            InterfaceClassGuid: GUID::from_u128(0),
            Flags: 0,
            Reserved: 0,
        };
        // SAFETY: the set handle is valid for `self`'s lifetime and
        // `interface` is a properly sized, writable structure.
        let found = unsafe {
            SetupDiEnumDeviceInterfaces(self.0, ptr::null(), class, index, &raw mut interface)
        };
        if found == 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(win32_code(ERROR_NO_MORE_ITEMS)) {
                Ok(None)
            } else {
                Err(error)
            };
        }

        let mut required = 0_u32;
        // SAFETY: a null detail buffer with size zero asks only for the
        // required size, which is written to `required`.
        let sized = unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                self.0,
                &raw const interface,
                ptr::null_mut(),
                0,
                &raw mut required,
                ptr::null_mut(),
            )
        };
        if sized == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(win32_code(ERROR_INSUFFICIENT_BUFFER)) {
                return Err(error);
            }
        }

        // A u64 buffer satisfies the detail structure's alignment.
        let words = (required as usize).div_ceil(mem::size_of::<u64>()).max(1);
        let mut buffer = vec![0_u64; words];
        let detail = buffer
            .as_mut_ptr()
            .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        // SAFETY: `buffer` is aligned for and at least as large as the
        // detail structure, so its size field can be written.
        unsafe {
            (*detail).cbSize = size_of_u32::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        }
        // SAFETY: `detail` points to `required` writable bytes owned by
        // `buffer`, which outlives the call.
        let filled = unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                self.0,
                &raw const interface,
                detail,
                required,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if filled == 0 {
            return Err(io::Error::last_os_error());
        }

        let path_offset = mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath);
        let path_units = (required as usize).saturating_sub(path_offset) / 2;
        // SAFETY: the API wrote a NUL-terminated UTF-16 path of at most
        // `path_units` units starting at `DevicePath`, inside `buffer`.
        let units = unsafe {
            std::slice::from_raw_parts((&raw const (*detail).DevicePath).cast::<u16>(), path_units)
        };
        let length = units
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(units.len());
        Ok(Some(
            OsString::from_wide(&units[..length])
                .to_string_lossy()
                .into_owned(),
        ))
    }
}

impl Drop for DeviceInfoSet {
    fn drop(&mut self) {
        // SAFETY: the handle came from SetupDiGetClassDevsW and is destroyed
        // exactly once.
        unsafe {
            SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

/// Open handle to the TEAC driver's private control file.
pub struct ControlFile(HANDLE);

// SAFETY: a file handle may be used from any thread; `ControlFile` has unique
// ownership and is not shared without `&mut`.
unsafe impl Send for ControlFile {}

impl ControlFile {
    /// Opens a control file listed by [`control_interfaces`].
    ///
    /// # Errors
    ///
    /// Returns the operating-system error if the driver refuses the open.
    pub fn open(control_path: &str) -> io::Result<Self> {
        let wide = control_path
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the
        // call; null security attributes and template handle are permitted.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(handle))
    }

    /// Sends a host-to-device vendor request and returns the payload length.
    ///
    /// # Errors
    ///
    /// Returns an error if the payload exceeds 64 bytes or the IOCTL fails.
    pub fn vendor_out(
        &mut self,
        request: u8,
        value: u16,
        index: u16,
        payload: &[u8],
    ) -> io::Result<usize> {
        let input = vendor_out_request(request, value, index, payload).map_err(|length| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{length}-byte payload exceeds the {MAX_MESSAGE_LEN}-byte limit"),
            )
        })?;
        self.ioctl(&input, &mut [])?;
        Ok(payload.len())
    }

    /// Receives a device-to-host vendor request into `buffer`.
    ///
    /// # Errors
    ///
    /// Returns an error if the IOCTL fails.
    pub fn vendor_in(
        &mut self,
        request: u8,
        value: u16,
        index: u16,
        buffer: &mut [u8],
    ) -> io::Result<usize> {
        let input = vendor_in_request(request, value, index);
        self.ioctl(&input, buffer)
    }

    fn ioctl(&mut self, input: &[u8], output: &mut [u8]) -> io::Result<usize> {
        let input_len =
            u32::try_from(input.len()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let output_len = u32::try_from(output.len())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let output_ptr = if output.is_empty() {
            ptr::null_mut()
        } else {
            output.as_mut_ptr().cast()
        };
        let mut returned = 0_u32;
        // SAFETY: the handle is open, both buffers are valid for their stated
        // lengths for the duration of this synchronous call, and no
        // OVERLAPPED structure is used.
        let succeeded = unsafe {
            DeviceIoControl(
                self.0,
                IOCTL_VENDOR_REQUEST,
                input.as_ptr().cast(),
                input_len,
                output_ptr,
                output_len,
                &raw mut returned,
                ptr::null_mut(),
            )
        };
        if succeeded == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((returned as usize).min(output.len()))
    }
}

impl Drop for ControlFile {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateFileW and is closed exactly once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

fn size_of_u32<T>() -> u32 {
    u32::try_from(mem::size_of::<T>()).unwrap_or(u32::MAX)
}

fn win32_code(code: u32) -> i32 {
    i32::try_from(code).unwrap_or(i32::MAX)
}
