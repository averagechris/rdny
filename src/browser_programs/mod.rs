//! Leaf registry for page-side JavaScript programs.
//!
//! This module depends only on serialization. It performs no CDP I/O and owns
//! every substantial script passed by selector, input, wait, and command hosts.

mod actionability;
mod download;
mod drag;
mod selector;
mod wait;

pub(crate) use actionability::{COORDINATE_HIT_TEST, TARGET_ACTIONABILITY};
pub(crate) use download::STREAM_FACTORY as DOWNLOAD_STREAM_FACTORY;
pub(crate) use drag::{drag_capture_install, drag_capture_take};
pub(crate) use selector::selector_traversal;
pub(crate) use wait::MUTATION_CLOCK as WAIT_MUTATION_CLOCK;
