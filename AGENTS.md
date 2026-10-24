# AGENTS.md

This file applies to the entire repository.

## Project purpose

US-HR Custom Control is an independent Rust controller for the internal mixer
and ordinary device settings of TASCAM US-HR USB audio interfaces. The initial
and hardware-validated target is the US-1x2HR. The application is not an audio
driver and must not take ownership of audio streaming.

Keep firmware updating out of scope. Do not add firmware-upload, bootloader,
driver-replacement, or generic WinUSB installation behavior without an explicit
project decision and a separate safety design.

## Repository map

- `crates/us-hr-core`: platform-independent device models, settings, status
  types, and the USB message codec. Keep this crate free of USB and GUI code.
- `crates/us-hr-usb`: discovery, the backend-neutral `Transport<C:
  ControlChannel>` protocol layer (queries, setting writes, read-back
  verification), and the libusb and TEAC-driver `ControlChannel`s. New
  platform backends implement `ControlChannel` only; do not duplicate protocol
  logic.
- `crates/us-hr-windows`: TEAC Windows driver request encoding (safe, tested on
  every platform) and the SetupAPI/IOCTL FFI (`cfg(windows)`). It is the only
  crate allowed to contain `unsafe` code: its lints copy the workspace policy
  with `unsafe_code = "deny"` instead of `forbid`, the FFI module opts in, and
  every `unsafe` block needs a `SAFETY:` comment. Keep its lint lists in sync
  with the workspace and keep FFI out of every other crate.
- `apps/us-hr-cli`: read-only diagnostics and explicitly requested, scriptable
  setting changes.
- `apps/us-hr-control`: egui/eframe desktop application, undo history, presets,
  and visual theme.
- `docs/protocol.md`: source of truth for reverse-engineered command mappings,
  conversions, supported devices, and platform limitations.
- `packaging`: platform integration and release packaging assets.
- `.github/workflows`: macOS arm64, Linux x64, and Windows x64 CI/release jobs.
- `packaging/windows`: Windows icon and the read-only driver diagnostics script.

## Invariants and device safety

- Use `AccessMode::ReadOnly` for discovery, inspection, status, and every other
  operation that does not need to write. A read-only path must never silently
  escalate to read-write access.
- Device writes must be explicit and use the typed `SettingChange` or complete
  `DeviceSettings` APIs. GUI and normal CLI writes must use
  `apply_setting_verified` or `apply_settings_verified` so the hardware is read
  back after the transfer.
- A successful write means the requested value was observed in the read-back,
  not merely that the USB transfer completed. Surface mismatches as errors and
  resynchronize displayed state when recovery is possible.
- Do not run a write operation against attached hardware merely to test code.
  Read-only probes are acceptable. Only perform a live write when the user
  explicitly requests device-changing validation; minimize the change and
  report the exact setting affected.
- Automated tests must not require a connected interface and must not mutate a
  real device. Test message encoding, decoding, mapping, validation, history,
  and persistence with deterministic unit/property tests or test doubles.
- Preserve the 64-byte protocol limit, typed field encoding, little-endian
  integers, readiness polling, acknowledgement handling, and channel context
  checks described in `docs/protocol.md`.
- Never infer a new command or mapping from a convenient numeric pattern. Record
  evidence in `docs/protocol.md` and add codec/mapping tests when protocol
  behavior changes.

## GUI behavior

- Keep device state separate from in-progress widget state. After a successful
  operation, synchronize the UI from the verified `DeviceStatus`; after failure,
  attempt a status read and display the recovered state.
- Preserve session undo semantics: record only verified prior snapshots, skip
  duplicates, retain the bounded history, and treat a preset application as one
  undoable operation.
- Presets are named, cross-platform JSON snapshots stored through `PresetStore`.
  Validate names, preserve the on-disk format when possible, and test migrations
  if the format changes.
- Keep shared colors, spacing, widget visuals, and reusable presentation helpers
  in `apps/us-hr-control/src/theme.rs` rather than scattering style constants.
- Keep verification, error, offline, and ready feedback in the fixed bottom
  status bar. The status bar must remain visible while the main content scrolls.
- Keep automatic power saving as a checkbox in the Power Management card, and
  keep that card immediately above the Presets card unless a requested redesign
  explicitly changes this structure.
- Maintain usable layouts at the configured minimum window size. Visually check
  both the top-level controls and the scrolled lower panels after material UI
  changes.

## Cross-platform requirements

- Supported build targets are Apple Silicon macOS, 64-bit Linux, and 64-bit
  Windows. Avoid
  platform-specific assumptions in `us-hr-core` and `us-hr-usb`. Isolate
  unavoidable platform behavior behind narrow `cfg` blocks.
- Keep both Wayland and X11 support enabled for Linux GUI builds.
- Do not replace or rebind the Windows audio driver. ASIO buffer sizing belongs
  to the vendor driver and is not a portable device setting.
- Windows control goes through the official TEAC driver's private control
  file and vendor-request IOCTL (see `docs/protocol.md`). Never install,
  replace, or rebind drivers from this project. Status reads and one verified
  write were validated on Windows 11 x64 with the TASCAM driver; changing a
  setting to a different value has not been tested on Windows yet. Windows x64
  release candidates are packaged like the other platforms.
- When adding dependencies, prefer portable Rust crates, avoid wildcard
  versions, update `Cargo.lock`, and ensure the license/source policy in
  `deny.toml` still passes.

## Rust conventions

- Use the pinned toolchain in `rust-toolchain.toml`. The workspace uses Rust
  edition 2024 and declares an MSRV of 1.85.
- Preserve workspace lint policy: no unsafe code, no ignored `must_use` values,
  and no `unwrap`, `expect`, `panic`, `todo`, `unimplemented`, or `dbg!` in
  workspace code.
- Prefer typed domain values and errors over strings or raw protocol numbers.
  Use `thiserror` in libraries and `anyhow` only at application boundaries.
- Keep transport details out of `us-hr-core`, presentation details out of the
  USB crate, and device I/O out of pure model and persistence tests.
- Add focused regression tests with every bug fix. Use property tests for codec
  invariants and boundary-heavy transformations where they add value.
- Format with rustfmt. Do not hand-format against `rustfmt.toml`.

## Required checks

Run the checks relevant to the change, and run the complete Rust suite before
hand-off:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo deny check
cargo llvm-cov --workspace --all-features --summary-only --fail-under-lines 35
```

When workflow or shell files change, also run:

```sh
actionlint .github/workflows/*.yml
shellcheck packaging/macos/bundle.sh
```

CI also parses `packaging/windows/collect-driver-info.ps1` with PowerShell.

If a required external checker is unavailable, say so explicitly instead of
claiming it passed. Keep the working tree free of generated build artifacts;
`target/` is not source.

## Build and manual verification

Useful read-only device probes:

```sh
cargo run -p us-hr-cli -- list
cargo run -p us-hr-cli -- inspect
cargo run -p us-hr-cli -- status
```

Run the development GUI with:

```sh
cargo run -p us-hr-control
```

On macOS, create and verify the Finder-launchable bundle with:

```sh
./packaging/macos/bundle.sh
codesign --verify --deep --strict "target/release/US-HR Custom Control.app"
open "target/release/US-HR Custom Control.app"
```

The raw `target/release/us-hr-control` file is a Unix executable, not the item a
macOS user should double-click. After UI work, inspect the packaged application,
not only a debug build.

## Documentation and hand-off

- Update `docs/protocol.md` with any protocol discovery, mapping correction, or
  change in validation status.
- Update `README.md` when user-facing features, launch instructions, platform
  support, preset behavior, or packaging changes.
- Distinguish clearly among compilation coverage, simulated tests, and live
  hardware validation in documentation and final reports.
- Summarize modified behavior, checks run, and any unverified platform or
  hardware assumptions. Do not report a device write as verified unless an
  actual read-back matched it.
