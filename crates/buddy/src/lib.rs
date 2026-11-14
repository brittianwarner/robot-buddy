//! Durable Robot Buddy actors, authenticated contracts and model adapters.
pub mod actor;
pub mod audio;
pub mod auth;
pub mod context;
pub mod conversation;
pub mod memory;
pub mod model;
pub mod protocol;
pub mod voice;
pub use actor::{Bot, registry};

pub mod persona;

pub mod sentences;
