//! Shared, transport-independent building blocks for Fut.

mod agent_detection;
pub mod alerts;
pub(crate) mod attachments;
pub mod cli;
pub mod client;
pub(crate) mod command;
pub mod daemon;
pub mod doctor;
pub mod domain;
pub(crate) mod extension_store;
pub(crate) mod extensions;
pub mod layout;
pub(crate) mod machines;
pub mod project;
pub(crate) mod project_definition;
pub mod protocol;
pub mod resources;
pub mod splits;
pub(crate) mod ssh_bridge;
pub(crate) mod state_file;
pub mod terminal;
pub(crate) mod upgrade;
