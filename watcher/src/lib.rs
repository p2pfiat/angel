//! Watcher: enforces check-in deadlines and alerts contacts.
//!
//! Used by the standalone `warn-watcher` binary (Docker, e.g. on a home server) and built into
//! the desktop agent, so every installation can watch its owner and others.

pub mod config;
pub mod contacts;
pub mod engine;
pub mod ntfy;
pub mod phones;
pub mod service;
pub mod telegram;
pub mod topics;
pub mod vips;
pub mod webpush;

pub use config::Config;
pub use service::{edit_angel, edit_phone, invite_link, load_or_create_key, request_offer, request_pair, run, test_alert, AngelEdit, PhoneEdit};
