//! Minimal Chrome DevTools Protocol layer.
//!
//! See docs/decisions/0001-cdp-client.md: hand-rolled blocking client
//! over tungstenite + serde_json; HTTP /json/* helpers over
//! std::net::TcpStream.

pub mod client;
pub mod http;
