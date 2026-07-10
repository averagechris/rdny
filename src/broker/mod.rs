//! Broker protocol/security foundation.
//!
//! This module is intentionally not wired into browser lifecycle yet. It
//! provides the local Unix-socket transport, protocol framing/handshake, and a
//! pure in-memory CDP multiplexer for the broker work tracked in #129.

#![allow(dead_code)]

pub(crate) mod mux;
pub(crate) mod protocol;
pub(crate) mod security;
