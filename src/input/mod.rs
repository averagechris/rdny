//! Typed trusted-input façade.

mod cleanup;
mod drag;
mod keyboard;
mod pointer;
mod types;

pub(crate) use drag::{
    DEFAULT_DRAG_DURATION_MS, DEFAULT_DRAG_STEPS, MAX_DRAG_DURATION_MS, MAX_DRAG_STEPS,
    MIN_DRAG_DURATION_MS, MIN_DRAG_STEPS, drag,
};
pub(crate) use keyboard::{KeyChord, key};
pub(crate) use pointer::{pointer_click, pointer_down, pointer_move, pointer_up};
pub(crate) use types::{MouseButton, PointerPoint, PointerTarget};
