# US-HR settings-panel and USB protocol analysis

This document records the interoperability analysis used by the implementation.
It is deliberately limited to ordinary settings and status operations; firmware
update commands are excluded.

## What the vendor settings panel does

The panel is a controller for state implemented in the interface firmware. It is
not the audio driver and it does not carry the audio stream. For the US-1x2HR it
exposes:

- direct-monitor mono/stereo mode;
- enable/mute state for input channels 1 and 2;
- input-versus-computer monitor balance;
- loopback enable, input mono/stereo, and computer-output mono/stereo;
- broadcast volume;
- automatic power saving;
- product, firmware/build, and current sample-rate information.

The Windows panel additionally exposes the driver's audio buffer size. That is
an ASIO-driver setting rather than a portable device command and is not part of
this USB implementation.

## Device identity and transport

| Model | USB VID:PID | Validation status |
|---|---|---|
| US-1x2HR | `0644:806f` | Control protocol tested with real hardware |
| US-2x2HR | `0644:8070` | USB identity detection only; control protocol untested |
| US-4x4HR | `0644:8071` | USB identity detection only; control protocol untested |

Only the US-1x2HR has been tested with real hardware. Recognizing the product
IDs of the US-2x2HR and US-4x4HR during USB discovery does not establish that
their status or setting commands are compatible with the US-1x2HR mappings
documented below.

Messages use vendor, device-recipient control transfers with value and index set
to zero:

| Direction | `bmRequestType` | `bRequest` | Maximum payload |
|---|---:|---:|---:|
| Host to device | `0x40` | `0x1d` | 64 bytes |
| Device to host | `0xc0` | `0x1e` | 64 bytes |

## Message format

A transfer contains typed fields and ends with `00 00`. Multi-byte integers are
little-endian.

| Type | Encoding after field ID and type |
|---:|---|
| `0` | terminator; valid only as field ID `0` |
| `1` | query marker, no payload |
| `2` | one-byte unsigned value |
| `3` | two-byte unsigned value |
| `4` | four-byte unsigned value |
| `5` | one-byte length followed by that many bytes |

Before sending, the client receives until readiness field `0xa1` reports a
non-zero value. After sending a query, acknowledgement-only replies may arrive
before the response, so the client polls until the requested field and channel
context are present.

## Global queries

| Request | Response | Meaning |
|---:|---:|---|
| `0x31` | `0x32` | firmware version (`u16`, 100 means 1.00) |
| `0x31` | `0x33` | firmware build (`u16`) |
| `0x31` | `0x34` | product name (bytes) |
| `0x31` | `0x35` | firmware build date (bytes) |
| `0x31` | `0x36` | firmware build time (bytes) |
| `0x31` | `0x37` | product signature (bytes) |
| `0x18` | `0x18` | current sample rate (`u32`) |
| `0x14` | `0x15` | automatic power save (`u8`) |

Automatic power save is written with field `0x15`, type `2`, value zero or one.

## Channel commands

Channel operations begin with group field `0x61` and index field `0x62`, both
type `2`. A query then carries a type-1 get command; a write carries the matching
type-2 set command and value.

| Property | Get | Set |
|---|---:|---:|
| fader/value | `0xc1` | `0x81` |
| mute | `0xc3` | `0x83` |
| mono/stereo | `0xc6` | `0x86` |

Observed mono/stereo values are `1` for mono and `4` for stereo. The US-1x2HR
panel maps its controls as follows:

| Control | Group | Index | Property and conversion |
|---|---:|---:|---|
| Direct monitor | 5 | 1 | mono/stereo |
| Audio input 1 | 4 | 1 | inverse of mute |
| Audio input 2 | 4 | 2 | inverse of mute |
| Monitor balance | 5 | 1 | fader; UI value is `127 - wire value` |
| Loopback enabled | 6 | 0 | fader interpreted as Boolean |
| Loopback input | 4 | 1 | mono/stereo |
| Loopback computer output | 3 | 1 | mono/stereo |
| Broadcast volume | 6 | 1 | fader, range 0 through 127 |

## Platform assessment

- macOS arm64: validated against a connected US-1x2HR through the standard USB
  stack. The vendor panel itself is an older Intel Qt application; this project
  is native Rust/arm64 when built on Apple Silicon.
- Linux x86-64: compilation is covered by CI, but the build has not been launched
  or tested with real hardware on Linux. The same libusb control transfers are
  expected to work alongside USB Audio Class support, and a narrow `uaccess`
  udev rule is included, but this remains unvalidated.
- Windows x86-64: descriptor discovery was validated with a connected US-1x2HR
  using the built-in Windows USB Audio 2 driver. The device enumerated as
  `0644:806f`, but libusb could not open it: the composite parent used
  `usbccgp`, its only function used `usbaudio2`, and neither exposed a
  libusb-compatible control path. Rebinding the only function or composite
  parent to WinUSB would disable audio. Control therefore goes through the TEAC
  driver described under [Windows transport](#windows-transport). Status
  reads and a verified write through that backend were validated on
  2026-10-24 (see below). Windows release packages remain disabled.

The only hardware validation performed to date used a US-1x2HR reporting
firmware 1.00 build 14. Readback of every modeled setting succeeded, and the
write path was verified by reapplying broadcast volume 77 and reading 77 back.
No status query or setting change has been tested on a US-2x2HR or US-4x4HR.

## Windows transport

Evidence: offline analysis of the official `TASCAM_US-HR_Installer_1.00.exe`
(Inno Setup 5.5.7, signed by TEAC Corporation). The installer was unpacked
without being run; all 33 files matched the installer's stored SHA-1 hashes.
The findings below come from the driver INF, PE imports and exports, and a
disassembly of `US-HR_SettingsPanel.exe` 1.0.0.0.

**Hardware validation (2026-10-24):** Windows 11 Pro build 26300 x64,
US-1x2HR firmware 1.00 build 14 (build date `Sep  2 2020 17:42:40`), with the
TASCAM Windows 10 driver package installed (`us-1x2hr` service, driver version
13.49.20.427, 05/18/2020; the composite parent stays on `usbccgp`). The backend
opened `…{65e8773e-8f56-11d0-a3b9-00a0c9223196}\us-1x2hr`, and `status` read
the firmware identity, sample rate (44.1 kHz), and every modeled setting
without errors in about 175 ms; the GUI showed the same state. Readiness
replies carried field `0xa1` as `u16`. The write path was then verified by
reapplying broadcast volume 70 (its current value) and reading 70 back; a
following status matched the earlier one in every field. Because the value did
not change, this proves the write direction is accepted, not that a different
value takes effect. The audio endpoints stayed present throughout.

- The package installs TEAC's own AVStream/KMDF driver, `us-1x2hr.sys`
  1.0.0.0 (INF `uac20.inf`, DriverVer 05/18/2020, 13.49.20.427, for Windows 10, WHCP-signed).
  It binds to `USB\VID_0644&PID_806F&MI_00`, `…PID_8070&MI_00` and
  `…PID_8071&MI_00`, replacing `usbaudio2` on the audio function. It is not a
  Thesycon TUSBAudio driver and ships no vendor API DLL.
- The driver registers only the standard KS interface classes (AUDIO, RENDER,
  CAPTURE, TOPOLOGY) with reference string `ksus1x2hr`. Its device SDDL grants
  read/write to everyone, so no elevation is needed.
- The Settings Panel imports `SetupDi*`, `CreateFileW` and `DeviceIoControl`
  directly. It enumerates KSCATEGORY_RENDER
  `{65E8773E-8F56-11D0-A3B9-00A0C9223196}`, replaces the interface path's
  reference string with `\us-1x2hr`, and opens that path with
  `GENERIC_READ | GENERIC_WRITE` and read/write sharing.
- Every protocol transfer uses IOCTL `0x00222000`
  (`CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_ANY_ACCESS)`).
  The input buffer starts with this header:

  | Offset | Size | Field | Host to device | Device to host |
  |---:|---:|---|---|---|
  | `0x00` | u32 | transfer direction | `0` | `1` |
  | `0x04` | u32 | payload length | payload length | `0` |
  | `0x08` | u16 | URB function | `0x0017` (`URB_FUNCTION_VENDOR_DEVICE`) | `0x0017` |
  | `0x0a` | u16 | `wValue` | `0` | `0` |
  | `0x0c` | u16 | `wIndex` | `0` | `0` |
  | `0x0e` | u8 | reserved | `0` | `0` |
  | `0x0f` | u8 | `bRequest` | `0x1d` | `0x1e` |
  | `0x10` | n | payload | message bytes | none |

  Inputs are padded to at least `0x14` bytes. Host-to-device requests pass no
  output buffer; device-to-host requests receive the message in the output
  buffer, whose size is the requested length. The panel calls the write
  helper only with `bRequest` `0x1d` and the read helper only with `0x1e`, both
  with `wValue` and `wIndex` zero. That matches the vendor requests above, so
  the message format is unchanged.
- The header layout is confirmed for reads and writes by the validation above. Still
  unconfirmed: the exact meaning of header bytes `0x00` and `0x0e`
  (inferred from `_URB_CONTROL_VENDOR_OR_CLASS_REQUEST` conventions), and
  whether control requests are safe while the driver streams ASIO audio. The
  ASIO DLL uses further private IOCTLs (`0x222028`–`0x222068`, `0x22603e`,
  `0x22a041`, `0x22a054`) for streaming, buffer size and sample rate; they are
  not decoded and are not used.
