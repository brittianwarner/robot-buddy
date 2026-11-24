//! Durable Robot Buddy actors, authenticated contracts and model adapters.
//!
//! Without the default `server` feature only the wire contracts, device credentials and
//! audio formats build, which is what the robot links.
#[cfg(feature = "server")]
pub mod actor;
pub mod audio;
pub mod auth;
#[cfg(feature = "server")]
pub mod config;
pub mod context;
#[cfg(feature = "server")]
pub mod conversation;
#[cfg(feature = "server")]
pub mod db;
#[cfg(feature = "server")]
pub mod memory;
#[cfg(feature = "server")]
pub mod model;
pub mod persona;
pub mod protocol;
pub mod sentences;
pub mod voice;
#[cfg(feature = "server")]
pub use actor::{Bot, registry};
