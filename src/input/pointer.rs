//! Pointer target resolution, hit testing, payloads, and button sequencing.

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::cleanup::{CLEANUP_TIMEOUT, finish_cleanup};
use super::types::{MouseButton, PointerTarget};
use crate::interaction_target::ActionPoint;
use crate::session::{Deadline, PageSession};

#[derive(Debug)]
pub(super) struct ResolvedPointerTarget {
    pub(super) point: ActionPoint,
    pub(super) object_id: Option<String>,
}

#[derive(Debug)]
pub(super) enum PointerTargetIdentity {
    Selector(String),
    Point(ActionPoint),
}

impl PointerTargetIdentity {
    pub(super) fn resolve(sess: &mut PageSession, target: &PointerTarget) -> Result<Self> {
        match target {
            PointerTarget::Selector(selector) => Ok(Self::Selector(sess.element(selector)?)),
            PointerTarget::Point(point) => Ok(Self::Point(ActionPoint {
                x: point.x,
                y: point.y,
            })),
        }
    }

    pub(super) fn action_point(&self, sess: &mut PageSession) -> Result<ResolvedPointerTarget> {
        match self {
            Self::Selector(object_id) => Ok(ResolvedPointerTarget {
                point: sess.element_action_point(object_id)?,
                object_id: Some(object_id.clone()),
            }),
            Self::Point(point) => Ok(ResolvedPointerTarget {
                point: sess.coordinate_action_point(*point)?,
                object_id: None,
            }),
        }
    }
}

impl ResolvedPointerTarget {
    pub(super) fn resolve(sess: &mut PageSession, target: &PointerTarget) -> Result<Self> {
        PointerTargetIdentity::resolve(sess, target)?.action_point(sess)
    }

    pub(super) fn revalidate(&self, sess: &mut PageSession) -> Result<()> {
        if let Some(object_id) = &self.object_id {
            sess.revalidate_element_action_point(object_id, self.point)
        } else {
            sess.revalidate_coordinate_action_point(self.point)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MouseEventKind {
    Moved {
        button: Option<MouseButton>,
        buttons: u8,
    },
    Pressed {
        button: MouseButton,
    },
    Released {
        button: MouseButton,
    },
}

fn mouse_payload(kind: MouseEventKind, point: ActionPoint) -> Value {
    match kind {
        MouseEventKind::Moved { button, buttons } => json!({
            "type": "mouseMoved",
            "x": point.x,
            "y": point.y,
            "button": button.map(MouseButton::as_cdp).unwrap_or("none"),
            "buttons": buttons,
        }),
        MouseEventKind::Pressed { button } => json!({
            "type": "mousePressed",
            "x": point.x,
            "y": point.y,
            "button": button.as_cdp(),
            "buttons": button.buttons_mask(),
            "clickCount": 1,
        }),
        MouseEventKind::Released { button } => json!({
            "type": "mouseReleased",
            "x": point.x,
            "y": point.y,
            "button": button.as_cdp(),
            "buttons": 0,
            "clickCount": 1,
        }),
    }
}

pub(super) fn dispatch_mouse(
    sess: &mut PageSession,
    kind: MouseEventKind,
    point: ActionPoint,
    deadline: Deadline,
) -> Result<()> {
    sess.call_until(
        "Input.dispatchMouseEvent",
        mouse_payload(kind, point),
        deadline,
    )?;
    Ok(())
}

pub(crate) fn pointer_move(sess: &mut PageSession, target: &PointerTarget) -> Result<()> {
    let target = ResolvedPointerTarget::resolve(sess, target)?;
    dispatch_mouse(
        sess,
        MouseEventKind::Moved {
            button: None,
            buttons: 0,
        },
        target.point,
        sess.deadline(),
    )
}

/// Move and press a button.  A successful call intentionally leaves the button
/// down for a later `pointer_up`; an ambiguous failed press is released.
pub(crate) fn pointer_down(
    sess: &mut PageSession,
    target: &PointerTarget,
    button: MouseButton,
) -> Result<()> {
    let target = ResolvedPointerTarget::resolve(sess, target)?;
    dispatch_mouse(
        sess,
        MouseEventKind::Moved {
            button: None,
            buttons: 0,
        },
        target.point,
        sess.deadline(),
    )?;
    target.revalidate(sess)?;
    if let Err(primary) = dispatch_mouse(
        sess,
        MouseEventKind::Pressed { button },
        target.point,
        sess.deadline(),
    ) {
        let cleanup_deadline = Deadline::after(CLEANUP_TIMEOUT);
        let cleanup = dispatch_mouse(
            sess,
            MouseEventKind::Released { button },
            target.point,
            cleanup_deadline,
        )
        .context("releasing mouse button after ambiguous press failure")
        .err()
        .into_iter()
        .collect();
        return finish_cleanup(Some(primary.context("pressing mouse button")), cleanup);
    }
    Ok(())
}

pub(crate) fn pointer_up(
    sess: &mut PageSession,
    target: &PointerTarget,
    button: MouseButton,
) -> Result<()> {
    let target = ResolvedPointerTarget::resolve(sess, target)?;
    target.revalidate(sess)?;
    dispatch_mouse(
        sess,
        MouseEventKind::Released { button },
        target.point,
        sess.deadline(),
    )
}

/// Click helper retained for the existing command while sharing the same typed
/// payload and cleanup guarantees as the new pointer primitives.
pub(crate) fn pointer_click(
    sess: &mut PageSession,
    target: &PointerTarget,
    button: MouseButton,
) -> Result<()> {
    let target = ResolvedPointerTarget::resolve(sess, target)?;
    dispatch_mouse(
        sess,
        MouseEventKind::Moved {
            button: None,
            buttons: 0,
        },
        target.point,
        sess.deadline(),
    )?;
    target.revalidate(sess)?;

    let mut primary_error = dispatch_mouse(
        sess,
        MouseEventKind::Pressed { button },
        target.point,
        sess.deadline(),
    )
    .err()
    .map(|error| error.context("pressing mouse button"));
    // Preserve the established selector-click sequence: the exact selected
    // node is revalidated immediately before press, then press/release are
    // adjacent. Explicit coordinates have no remote identity and are instead
    // revalidated against the live viewport before both state changes.
    if primary_error.is_none()
        && target.object_id.is_none()
        && let Err(error) = target.revalidate(sess)
    {
        primary_error = Some(error.context("revalidating click target before release"));
    }
    let cleanup_deadline = Deadline::after(CLEANUP_TIMEOUT);
    let release = dispatch_mouse(
        sess,
        MouseEventKind::Released { button },
        target.point,
        cleanup_deadline,
    )
    .context("releasing mouse button");

    let mut cleanup_errors = Vec::new();
    if let Err(error) = release {
        if primary_error.is_some() {
            cleanup_errors.push(error);
        } else {
            primary_error = Some(error);
        }
    }
    finish_cleanup(primary_error, cleanup_errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interaction_target::ActionPoint;
    use serde_json::json;
    #[test]
    fn mouse_payloads_include_cdp_button_state() {
        let point = ActionPoint { x: 10.5, y: 20.25 };
        assert_eq!(
            mouse_payload(
                MouseEventKind::Moved {
                    button: None,
                    buttons: 0,
                },
                point
            ),
            json!({
                "type": "mouseMoved", "x": 10.5, "y": 20.25,
                "button": "none", "buttons": 0
            })
        );
        assert_eq!(
            mouse_payload(
                MouseEventKind::Moved {
                    button: Some(MouseButton::Left),
                    buttons: 1,
                },
                point
            ),
            json!({
                "type": "mouseMoved", "x": 10.5, "y": 20.25,
                "button": "left", "buttons": 1
            })
        );
        assert_eq!(
            mouse_payload(
                MouseEventKind::Pressed {
                    button: MouseButton::Back
                },
                point
            ),
            json!({
                "type": "mousePressed", "x": 10.5, "y": 20.25,
                "button": "back", "buttons": 8, "clickCount": 1
            })
        );
        assert_eq!(
            mouse_payload(
                MouseEventKind::Released {
                    button: MouseButton::Back
                },
                point
            ),
            json!({
                "type": "mouseReleased", "x": 10.5, "y": 20.25,
                "button": "back", "buttons": 0, "clickCount": 1
            })
        );
    }
}
