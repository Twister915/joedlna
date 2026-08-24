#![forbid(unsafe_code)]

mod bookmarks;
pub mod catalog;
pub mod config;
pub mod description;
mod events;
pub mod http;
mod platform;
pub mod soap;
pub mod ssdp;

mod xml;

pub use config::{Config, ConfigError};
pub use http::{ServerError, serve};
