//! Shared, transport-independent building blocks for Fut.

mod agent_detection;
pub mod alerts;
pub mod cli;
pub mod client;
pub(crate) mod command;
pub mod daemon;
pub mod doctor;
pub mod domain;
pub(crate) mod extension_store;
pub(crate) mod extensions;
pub mod project;
pub(crate) mod project_definition;
pub mod protocol;
pub mod resources;
pub mod splits;
pub mod terminal;
