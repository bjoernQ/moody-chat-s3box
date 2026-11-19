//! Gemini Live voice companion for the ESP32-S3-BOX-3.
//!
//! - [`app`] — shared state and inter-task channels
//! - [`audio`] — ES7210/ES8311 codecs, mic streaming and speech playback
//! - [`distill`] — turns a conversation transcript into long-term memory
//! - [`live`] — Gemini Live API session over a WebSocket
//! - [`memory`] — persists the distilled memory in flash
//! - [`roboeyes`] — animated robot eyes renderer ([FluxGarage RoboEyes](https://github.com/FluxGarage/RoboEyes) port, GPL-3.0)
//! - [`spi_dma_displayinterface`] — DMA SPI display interface for mipidsi
//! - [`tls`] — TCP + TLS connection helpers
//! - [`touch`] — GT911 touch controller polling
//! - [`ui`] — display loop, snooze and conversation animations
//! - [`ws`] — minimal WebSocket client

#![no_std]

extern crate alloc;

pub mod app;
pub mod audio;
pub mod distill;
pub mod live;
pub mod memory;
pub mod net;
pub mod println;
pub mod roboeyes;
pub mod spi_dma_displayinterface;
pub mod tls;
pub mod touch;
pub mod ui;
pub mod ws;

pub const GEMINI_HOST: &str = "generativelanguage.googleapis.com";
pub const GEMINI_API_KEY: &str = env!("GEMINI_API_KEY");
