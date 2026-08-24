#[cfg(target_os = "macos")]
pub const SERVER: &str = concat!("macOS/1.0 UPnP/1.0 JoeDLNA/", env!("CARGO_PKG_VERSION"));

#[cfg(target_os = "linux")]
pub const SERVER: &str = concat!("Linux/1.0 UPnP/1.0 JoeDLNA/", env!("CARGO_PKG_VERSION"));

#[cfg(target_os = "windows")]
pub const SERVER: &str = concat!("Windows/1.0 UPnP/1.0 JoeDLNA/", env!("CARGO_PKG_VERSION"));

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub const SERVER: &str = concat!("Unix/1.0 UPnP/1.0 JoeDLNA/", env!("CARGO_PKG_VERSION"));
