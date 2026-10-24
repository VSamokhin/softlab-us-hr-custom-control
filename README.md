# US-HR Custom Control

An independent, cross-platform control application for the TASCAM US-HR USB
audio-interface family. Only the US-1x2HR is currently supported and tested for
status queries and setting changes.

The project does **not** replace an audio driver. It controls the interface's
internal mixer and device settings over vendor-specific USB control transfers.
macOS and Linux use their standard USB-audio stacks. Windows control goes
through the official TASCAM driver; it is implemented but not yet validated
with hardware, so CI builds Windows only as a preview artifact.

The reverse-engineering findings and command map are documented in
[`docs/protocol.md`](docs/protocol.md).

## Current status

- Automatically detects connected and disconnected US-1x2HR, US-2x2HR, and
  US-4x4HR devices. Detection of a model does not imply control support.
- Reads firmware identity, sample rate, and all US-1x2HR panel settings.
- Controls direct-monitor mode, input enables, monitor balance, loopback routing,
  broadcast volume, and automatic power saving.
- Saves named, cross-platform JSON presets and applies complete snapshots.
- Keeps a 50-state session undo history of verified device configurations.
- Reads the device back after every write and reports mismatches as errors.
- Provides both a command-line diagnostic/control tool and a desktop GUI.
- Uses an original XLR-socket application icon across native window and release
  packaging surfaces.
- Keeps read-only queries and opt-in write access separate in the USB API.
- Only the US-1x2HR has been tested with real hardware, using firmware 1.00
  build 14 on macOS. US-2x2HR and US-4x4HR control and status behaviors have not
  been tested and must not be assumed to be compatible.
- macOS arm64 is the only platform tested at runtime with real hardware. The
  Linux x64 build compiles in CI, but has not been runtime-tested or
  hardware-validated. Windows releases are disabled until the TASCAM-driver
  backend has been validated with real hardware.

Firmware updating is intentionally out of scope until the ordinary settings
protocol is complete and thoroughly tested.

## Build

Install Rust 1.85 or newer with [rustup](https://rustup.rs/), then run:

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
cargo llvm-cov --workspace --all-features --summary-only --fail-under-lines 35
```

The coverage command requires `cargo-llvm-cov` and the Rust
`llvm-tools-preview` component. CI enforces a minimum of 35% total line
coverage and uploads a browsable HTML report from every run. Generate the same
report locally with:

```sh
cargo llvm-cov --workspace --all-features --html
open target/llvm-cov/html/index.html
```

### Pre-commit checks

Enable the repository's tracked Git hooks once after cloning:

```sh
git config core.hooksPath .githooks
```

The pre-commit hook rejects staged whitespace errors and runs rustfmt, Clippy,
and the workspace tests. It also runs `cargo deny` when installed, plus
`actionlint` and `shellcheck` for relevant staged files when those tools are
installed. Coverage and cross-platform builds remain CI checks. Git's standard
`--no-verify` option can bypass the hook when necessary; CI still validates
pushed changes.

Run the read-only probe:

```sh
cargo run -p us-hr-cli -- list
cargo run -p us-hr-cli -- inspect
cargo run -p us-hr-cli -- status
```

Change one setting explicitly, for example:

```sh
cargo run -p us-hr-cli -- set direct-monitor stereo
cargo run -p us-hr-cli -- set broadcast-volume 100
```

Run the GUI:

```sh
cargo run -p us-hr-control
```

On macOS, build a Finder-launchable application bundle with:

```sh
./packaging/macos/bundle.sh
open "target/release/US-HR Custom Control.app"
```

The `.app` can also be double-clicked in Finder. The file named
`target/release/us-hr-control` is the underlying command-line executable, not a
macOS application bundle.

### Presets and undo

Enter a name under **Presets** and choose **Save current** to store the latest
verified device state. Choose a preset and select **Apply and verify** to write
the complete snapshot and confirm it through hardware read-back. **Cmd+Z** on
macOS or **Ctrl+Z** on Linux and Windows restores the previous verified state
from the current application session. Up to 50 states are retained; preset
application creates one undo step rather than one step per setting. The history
is cleared when the connected device set changes.

Presets are stored as JSON in the operating system's per-user configuration
directory. They remain available after restarting the application and use the
same format on macOS and Linux.

### Linux permissions

The Linux x64 build is compilation-tested in CI only. It has not been launched
or tested with a connected interface on Linux.

Install `packaging/linux/99-tascam-us-hr.rules` in `/etc/udev/rules.d/`, then
reload udev rules and reconnect the interface. The rules use logind's `uaccess`
tag rather than making the device world-writable.

### Windows

Windows control requires the official TASCAM US-HR driver (TEAC
`us-1x2hr.sys`, installed by `TASCAM_US-HR_Installer_1.00.exe`). With Windows'
built-in USB Audio 2 driver the interface is detected, but no application can
send it vendor control requests, so the GUI reports it as detected without
control and the CLI's `inspect` fails.

When the TASCAM driver is installed, the application talks to the device the
same way the official Settings Panel does: it finds the driver's render
interface, opens the driver's private `us-1x2hr` control file, and relays the
same 64-byte vendor messages used on macOS and Linux through the driver's
vendor-request IOCTL. Audio keeps running through the TASCAM driver; this
application never installs, replaces, or rebinds drivers. The transport is
documented in [`docs/protocol.md`](docs/protocol.md#windows-transport).

**The Windows backend has not yet been validated with real hardware.** It is
built from analysis of the official installer and covered by unit tests only,
so no Windows release package is published. CI builds, lints, and tests the
workspace on Windows x64 and uploads a `windows-preview-x64` workflow artifact.
The preview release executable is a windowed application with the embedded
application icon; debug builds keep a console for logs.

Do not replace the audio function or composite parent with a generic WinUSB
driver: doing so would prevent Windows from using the interface for audio.

`packaging/windows/collect-driver-info.ps1` reports the drivers, device
interfaces, and TASCAM software associated with a connected interface. It only
reads device properties and the registry:

```powershell
powershell -ExecutionPolicy Bypass -File packaging\windows\collect-driver-info.ps1 > us-hr-driver-info.txt
```

## Structure

- `crates/us-hr-core`: platform-independent model and message codec.
- `crates/us-hr-usb`: backend-neutral protocol transport, the libusb and TEAC
  driver control channels, and typed US-HR operations.
- `crates/us-hr-windows`: TEAC driver request encoding and the Windows FFI;
  the only crate that permits `unsafe` code.
- `apps/us-hr-cli`: diagnostics and scriptable settings.
- `apps/us-hr-control`: native desktop UI using egui/eframe.

CI treats warnings as errors, verifies the declared Rust 1.85 minimum, checks
formatting, runs unit/property tests and dependency policy checks, and compiles
separate macOS arm64 and Linux x64 builds. A Windows x64 job builds, lints,
and tests the workspace and uploads a preview artifact that is never attached
to a release. Tagged releases upload each supported
platform build as its own artifact.

### Creating a release

Every successful `main` branch CI run is eligible to become a release
candidate. The serialized release workflow checks that no tagged draft release
is awaiting a decision, then creates an annotated semantic-version tag for the
tested commit. The first automatically generated tag uses the workspace version
from `Cargo.toml`; later candidates increment its patch component.

The CI workflow builds and packages that exact commit for every platform. After
all CI jobs pass, the release workflow reuses those immutable workflow
artifacts, adds the generated version to their filenames, and attaches them with
a `SHA256SUMS` file to a draft GitHub Release. Release Drafter generates its
notes from merged pull requests; feature, bug-fix, and documentation pull
requests are labeled automatically when possible.

Review each draft in GitHub and make one of these decisions before pushing the
next release candidate:

- Publish the draft to approve the release.
- Delete the draft to decline it. The Git tag remains as a permanent marker of
  the successfully tested commit and its version is not reused.

If another `main` build finishes while a tagged draft is pending, its release
workflow records why it was skipped and does not tag that commit. Publishing or
deleting the draft allows the next successful `main` build to repeat the
routine. Rerunning a failed release workflow safely reuses a tag that already
points at the same commit.

Creating or publishing a draft does not imply runtime or hardware validation of
the Linux package.

## Safety

Device writes require an explicit read-write transport. Discovery and status
queries use the read-only transport. Every normal GUI or CLI write is followed by
read-back verification; firmware-update commands are not exposed.

This project is not affiliated with or endorsed by TEAC Corporation or TASCAM.
