//! Embeds the application icon and version metadata in Windows executables.

// Only Windows builds can fail; other targets keep the same signature.
#[cfg_attr(not(windows), allow(clippy::unnecessary_wraps))]
fn main() -> std::io::Result<()> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../packaging/windows/app-icon.ico");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource
            .set_icon("../../packaging/windows/app-icon.ico")
            .set("ProductName", "US-HR Custom Control")
            .set("FileDescription", "US-HR Custom Control");
        resource.compile()?;
    }
    Ok(())
}
