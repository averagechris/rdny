//! Native drag capture, interception, dispatch, and cleanup.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::cleanup::{CLEANUP_TIMEOUT, finish_cleanup};
use super::pointer::{MouseEventKind, PointerTargetIdentity, dispatch_mouse};
use super::types::{MouseButton, PointerTarget};
use crate::interaction_target::ActionPoint;
use crate::session::{Deadline, PageSession};

pub(crate) const MIN_DRAG_STEPS: u32 = 1;
pub(crate) const MAX_DRAG_STEPS: u32 = 1_000;
pub(crate) const DEFAULT_DRAG_STEPS: u32 = 20;
pub(crate) const MIN_DRAG_DURATION_MS: u64 = 1;
pub(crate) const MAX_DRAG_DURATION_MS: u64 = 30_000;
pub(crate) const DEFAULT_DRAG_DURATION_MS: u64 = 500;

fn set_drag_interception(sess: &mut PageSession, enabled: bool, deadline: Deadline) -> Result<()> {
    sess.call_until(
        "Input.setInterceptDrags",
        json!({"enabled": enabled}),
        deadline,
    )?;
    Ok(())
}

fn take_intercepted_drag_data(sess: &mut PageSession) -> Result<Option<Value>> {
    while let Some(event) = sess.next_buffered_event(sess.deadline())? {
        if event.method == "Input.dragIntercepted" {
            return event
                .params
                .get("data")
                .cloned()
                .context("Input.dragIntercepted event omitted drag data")
                .map(Some);
        }
    }
    Ok(None)
}

fn wait_for_intercepted_drag_data(
    sess: &mut PageSession,
    maximum: Duration,
) -> Result<Option<Value>> {
    if let Some(data) = take_intercepted_drag_data(sess)? {
        return Ok(Some(data));
    }
    let wait_until = Instant::now()
        .checked_add(maximum)
        .unwrap_or_else(|| sess.deadline().instant())
        .min(sess.deadline().instant());
    let deadline = Deadline::at(wait_until);
    while let Some(event) = sess.next_event_until(deadline)? {
        if event.method == "Input.dragIntercepted" {
            return event
                .params
                .get("data")
                .cloned()
                .context("Input.dragIntercepted event omitted drag data")
                .map(Some);
        }
    }
    Ok(None)
}

fn dispatch_drag_event(
    sess: &mut PageSession,
    event_type: &str,
    point: ActionPoint,
    data: &Value,
) -> Result<()> {
    sess.call(
        "Input.dispatchDragEvent",
        json!({
            "type": event_type,
            "x": point.x,
            "y": point.y,
            "data": data,
        }),
    )?;
    Ok(())
}

const DRAG_CAPTURE_KEY: &str = "rdny.input.drag-capture.v1";

enum PageDragCapture {
    NotSeen,
    Canceled,
    Data(Value),
}

fn install_page_drag_capture(sess: &mut PageSession) -> Result<()> {
    let expression = crate::browser_programs::drag_capture_install(DRAG_CAPTURE_KEY);
    if sess.eval(&expression)?.as_bool() != Some(true) {
        bail!("browser did not install dragstart data capture");
    }
    Ok(())
}

fn take_page_drag_data(sess: &mut PageSession, deadline: Deadline) -> Result<PageDragCapture> {
    let expression = crate::browser_programs::drag_capture_take(DRAG_CAPTURE_KEY);
    let capture = sess.eval_until(&expression, deadline)?;
    if capture.is_null() || capture["seen"].as_bool() != Some(true) {
        return Ok(PageDragCapture::NotSeen);
    }
    if capture["settled"].as_bool() != Some(true) || capture["canceled"].as_bool() != Some(false) {
        return Ok(PageDragCapture::Canceled);
    }
    let items = capture["items"]
        .as_array()
        .context("browser returned invalid captured drag items")?
        .clone();
    let mask = match capture["effectAllowed"].as_str().unwrap_or("none") {
        "copy" => 1,
        "link" => 2,
        "move" => 16,
        "copyLink" => 1 | 2,
        "copyMove" => 1 | 16,
        "linkMove" => 2 | 16,
        "all" | "uninitialized" => 1 | 2 | 16,
        "none" => 0,
        _ => 0,
    };
    Ok(PageDragCapture::Data(
        json!({"items": items, "dragOperationsMask": mask}),
    ))
}

/// Drag between two independently trusted targets.
pub(crate) fn drag(
    sess: &mut PageSession,
    from: &PointerTarget,
    to: &PointerTarget,
    button: MouseButton,
    steps: u32,
    duration: Duration,
) -> Result<()> {
    validate_drag_options(steps, duration)?;

    // Resolve selector identities before pressing, then prove the destination
    // is actionable. Resolve the source point last because destination
    // validation may scroll. After press, derive the destination point again
    // from the same exact node so distant endpoints and pointer handlers cannot
    // leave us moving toward stale geometry.
    let from_identity = PointerTargetIdentity::resolve(sess, from)?;
    let to_identity = PointerTargetIdentity::resolve(sess, to)?;
    to_identity.action_point(sess)?;
    let from = from_identity.action_point(sess)?;

    // A preceding pointer primitive may have been sent through a now-detached
    // CDP session. Clear Chromium's drag controller before this self-contained
    // convenience action; this does not release or press any mouse button.
    sess.call("Input.cancelDragging", json!({}))
        .context("resetting prior browser drag state")?;

    if let Err(primary) = set_drag_interception(sess, true, sess.deadline()) {
        let cleanup_deadline = Deadline::after(CLEANUP_TIMEOUT);
        let cleanup = set_drag_interception(sess, false, cleanup_deadline)
            .context("disabling drag interception after ambiguous enable failure")
            .err()
            .into_iter()
            .collect();
        return finish_cleanup(
            Some(primary.context("enabling native drag interception")),
            cleanup,
        );
    }
    if let Err(primary) = install_page_drag_capture(sess) {
        let mut cleanup = Vec::new();
        let cleanup_deadline = Deadline::after(CLEANUP_TIMEOUT);
        if let Err(error) = take_page_drag_data(sess, cleanup_deadline)
            .context("removing possibly installed dragstart capture after setup failure")
        {
            cleanup.push(error);
        }
        if let Err(error) = set_drag_interception(sess, false, cleanup_deadline)
            .context("disabling drag interception after capture setup failure")
        {
            cleanup.push(error);
        }
        return finish_cleanup(
            Some(primary.context("installing native dragstart data capture")),
            cleanup,
        );
    }

    let source_setup = (|| {
        dispatch_mouse(
            sess,
            MouseEventKind::Moved {
                button: None,
                buttons: 0,
            },
            from.point,
            sess.deadline(),
        )?;
        // This must remain the final browser round trip before press: hover
        // handlers can detach, move, hide, or cover the exact selected node.
        from.revalidate(sess)
    })();
    if let Err(primary) = source_setup {
        let mut cleanup = Vec::new();
        let cleanup_deadline = Deadline::after(CLEANUP_TIMEOUT);
        if let Err(error) = take_page_drag_data(sess, cleanup_deadline)
            .context("removing dragstart capture after source setup failure")
        {
            cleanup.push(error);
        }
        if let Err(error) = set_drag_interception(sess, false, cleanup_deadline)
            .context("disabling drag interception after source setup failure")
        {
            cleanup.push(error);
        }
        return finish_cleanup(
            Some(primary.context("preparing and revalidating drag source")),
            cleanup,
        );
    }

    let mut last_safe_point = from.point;
    let mut primary_error = dispatch_mouse(
        sess,
        MouseEventKind::Pressed { button },
        from.point,
        sess.deadline(),
    )
    .err()
    .map(|error| error.context("pressing mouse button to begin drag"));

    let mut destination = None;
    let mut drag_data = None;
    let mut drag_entered = false;
    let mut drop_dispatched = false;
    if primary_error.is_none() {
        match to_identity.action_point(sess) {
            Ok(target) => destination = Some(target),
            Err(error) => {
                primary_error = Some(error.context("resolving drag destination after press"));
            }
        }
    }

    if primary_error.is_none()
        && let Some(to) = &destination
    {
        let started = Instant::now();
        let threshold_steps = steps.saturating_sub(1).min(5);
        for (index, point) in interpolation_points(from.point, to.point, steps)
            .into_iter()
            .enumerate()
        {
            let target_elapsed = duration.mul_f64((index + 1) as f64 / steps as f64);
            let target_time = started + target_elapsed;
            if let Some(wait) = target_time.checked_duration_since(Instant::now()) {
                sess.deadline().sleep(wait);
            }
            match dispatch_mouse(
                sess,
                MouseEventKind::Moved {
                    // Chromium needs the held button on the initial movements
                    // to cross the native drag threshold. Later movement is
                    // related to no button transition while `buttons` retains
                    // the held-state bitmask.
                    button: ((index as u32) < threshold_steps).then_some(button),
                    buttons: button.buttons_mask(),
                },
                point,
                sess.deadline(),
            ) {
                Ok(()) => {
                    last_safe_point = point;
                    if drag_data.is_none() {
                        match take_intercepted_drag_data(sess) {
                            Ok(data) => drag_data = data,
                            Err(error) => {
                                primary_error =
                                    Some(error.context("reading intercepted native drag data"));
                                break;
                            }
                        }
                    }
                    if let Some(data) = &drag_data {
                        let event_type = if drag_entered {
                            "dragOver"
                        } else {
                            drag_entered = true;
                            "dragEnter"
                        };
                        if let Err(error) = dispatch_drag_event(sess, event_type, point, data) {
                            primary_error = Some(error.context(format!(
                                "dispatching native {event_type} at drag movement step {} of {steps}",
                                index + 1
                            )));
                            break;
                        }
                    }
                }
                Err(error) => {
                    primary_error = Some(error.context(format!(
                        "dispatching drag movement step {} of {steps}",
                        index + 1
                    )));
                    break;
                }
            }
        }
    }

    // Chromium may emit dragIntercepted just after the final movement response;
    // wait briefly for actual browser drag data before deciding whether this is
    // an HTML drop or an ordinary held-button pointer drag.
    if primary_error.is_none() && drag_data.is_none() {
        match wait_for_intercepted_drag_data(sess, Duration::from_millis(250)) {
            Ok(data) => drag_data = data,
            Err(error) => {
                primary_error = Some(error.context("reading final intercepted native drag data"));
            }
        }
    }
    if primary_error.is_none() {
        match take_page_drag_data(sess, sess.deadline()) {
            Ok(PageDragCapture::Data(captured)) => {
                if drag_data.is_none() {
                    drag_data = Some(captured);
                }
            }
            Ok(PageDragCapture::Canceled) => drag_data = None,
            Ok(PageDragCapture::NotSeen) => {}
            Err(error) => {
                primary_error = Some(error.context("reading trusted page dragstart data"));
            }
        }
    }

    if primary_error.is_none()
        && let Some(data) = &drag_data
    {
        if !drag_entered
            && let Err(error) = dispatch_drag_event(sess, "dragEnter", last_safe_point, data)
        {
            primary_error = Some(error.context("dispatching native dragEnter before drop"));
        }
        if primary_error.is_none()
            && let Err(error) = dispatch_drag_event(sess, "dragOver", last_safe_point, data)
        {
            primary_error = Some(error.context("dispatching native dragOver before drop"));
        }
        if primary_error.is_none()
            && let Some(to) = &destination
            && let Err(error) = to.revalidate(sess)
        {
            primary_error =
                Some(error.context("revalidating drag destination immediately before native drop"));
        }
        if primary_error.is_none()
            && let Err(error) = dispatch_drag_event(sess, "drop", last_safe_point, data)
        {
            primary_error = Some(error.context("dispatching native drop"));
        } else if primary_error.is_none() {
            drop_dispatched = true;
        }
    }
    if primary_error.is_none()
        && drag_data.is_none()
        && let Some(to) = &destination
        && let Err(error) = to.revalidate(sess)
    {
        primary_error =
            Some(error.context("revalidating drag destination immediately before release"));
    }

    // The press request may have reached Chromium even when its response did
    // not.  Always send release on a fresh, short budget and at the last point
    // whose move was successfully dispatched.
    let cleanup_deadline = Deadline::after(CLEANUP_TIMEOUT);
    let release = dispatch_mouse(
        sess,
        MouseEventKind::Released { button },
        last_safe_point,
        cleanup_deadline,
    )
    .context("releasing mouse button after drag");
    let mut cleanup_errors = Vec::new();
    if let Err(error) = release {
        if primary_error.is_some() {
            cleanup_errors.push(error);
        } else {
            primary_error = Some(error);
        }
    }
    if let Err(error) = take_page_drag_data(sess, cleanup_deadline) {
        let error = error.context("removing dragstart capture after drag");
        if primary_error.is_some() {
            cleanup_errors.push(error);
        } else {
            primary_error = Some(error);
        }
    }
    if !drop_dispatched
        && let Err(error) = sess
            .call_until("Input.cancelDragging", json!({}), cleanup_deadline)
            .context("canceling intercepted drag after unsuccessful drop")
    {
        if primary_error.is_some() {
            cleanup_errors.push(error);
        } else {
            primary_error = Some(error);
        }
    }
    if let Err(error) = set_drag_interception(sess, false, cleanup_deadline)
        .context("disabling drag interception after drag")
    {
        if primary_error.is_some() {
            cleanup_errors.push(error);
        } else {
            primary_error = Some(error);
        }
    }
    finish_cleanup(primary_error, cleanup_errors)
}

fn validate_drag_options(steps: u32, duration: Duration) -> Result<()> {
    if !(MIN_DRAG_STEPS..=MAX_DRAG_STEPS).contains(&steps) {
        bail!("drag steps must be between {MIN_DRAG_STEPS} and {MAX_DRAG_STEPS}");
    }
    if duration < Duration::from_millis(MIN_DRAG_DURATION_MS)
        || duration > Duration::from_millis(MAX_DRAG_DURATION_MS)
    {
        bail!(
            "drag duration must be between {MIN_DRAG_DURATION_MS} and {MAX_DRAG_DURATION_MS} milliseconds"
        );
    }
    Ok(())
}

fn interpolation_points(from: ActionPoint, to: ActionPoint, steps: u32) -> Vec<ActionPoint> {
    (1..=steps)
        .map(|step| {
            if step == steps {
                return to;
            }
            let fraction = f64::from(step) / f64::from(steps);
            ActionPoint {
                x: from.x + (to.x - from.x) * fraction,
                y: from.y + (to.y - from.y) * fraction,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::PageSession;
    use serde_json::{Value, json};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;
    use tungstenite::{Message, accept};
    #[test]
    fn interpolation_is_exact_and_bounded_by_step_count() {
        let points = interpolation_points(
            ActionPoint { x: 0.0, y: 10.0 },
            ActionPoint { x: 10.0, y: -10.0 },
            4,
        );
        assert_eq!(points.len(), 4);
        assert_eq!(points[0], ActionPoint { x: 2.5, y: 5.0 });
        assert_eq!(points[1], ActionPoint { x: 5.0, y: 0.0 });
        assert_eq!(points[3], ActionPoint { x: 10.0, y: -10.0 });
    }

    #[test]
    fn drag_limits_include_only_documented_bounds() {
        assert!(
            validate_drag_options(MIN_DRAG_STEPS, Duration::from_millis(MIN_DRAG_DURATION_MS))
                .is_ok()
        );
        assert!(
            validate_drag_options(MAX_DRAG_STEPS, Duration::from_millis(MAX_DRAG_DURATION_MS))
                .is_ok()
        );
        assert!(validate_drag_options(0, Duration::from_millis(1)).is_err());
        assert!(validate_drag_options(MAX_DRAG_STEPS + 1, Duration::from_millis(1)).is_err());
        assert!(validate_drag_options(1, Duration::ZERO).is_err());
        assert!(validate_drag_options(1, Duration::from_millis(MAX_DRAG_DURATION_MS + 1)).is_err());
    }

    #[test]
    fn drag_preserves_press_and_cleanup_release_failures_at_last_safe_point() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = accept(stream).unwrap();
            loop {
                let command = read_json_message(&mut socket);
                let id = command["id"].clone();
                match command["method"].as_str().unwrap() {
                    "Runtime.evaluate" => {
                        let expression = command["params"]["expression"].as_str().unwrap();
                        let value = if expression.contains("window.addEventListener('dragstart'") {
                            json!(true)
                        } else if expression.contains("delete window[key]") {
                            Value::Null
                        } else {
                            json!({"status": "ready"})
                        };
                        socket
                            .send(Message::Text(
                                json!({
                                    "id": id,
                                    "result": {"result": {
                                        "type": "object",
                                        "value": value
                                    }}
                                })
                                .to_string()
                                .into(),
                            ))
                            .unwrap();
                    }
                    "Input.dispatchMouseEvent" if command["params"]["type"] == "mousePressed" => {
                        assert_eq!(command["params"]["x"], 1.0);
                        assert_eq!(command["params"]["y"], 2.0);
                        socket
                            .send(Message::Text(
                                json!({
                                    "id": id,
                                    "error": {"code": -32000, "message": "injected press failure"}
                                })
                                .to_string()
                                .into(),
                            ))
                            .unwrap();
                    }
                    "Input.dispatchMouseEvent" if command["params"]["type"] == "mouseReleased" => {
                        assert_eq!(command["params"]["x"], 1.0);
                        assert_eq!(command["params"]["y"], 2.0);
                        assert_eq!(command["params"]["buttons"], 0);
                        socket
                            .send(Message::Text(
                                json!({
                                    "id": id,
                                    "error": {"code": -32000, "message": "injected release failure"}
                                })
                                .to_string()
                                .into(),
                            ))
                            .unwrap();
                    }
                    "Input.setInterceptDrags" => {
                        let disabling = command["params"]["enabled"] == false;
                        socket
                            .send(Message::Text(
                                json!({"id": id, "result": {}}).to_string().into(),
                            ))
                            .unwrap();
                        if disabling {
                            break;
                        }
                    }
                    "Input.cancelDragging" => socket
                        .send(Message::Text(
                            json!({"id": id, "result": {}}).to_string().into(),
                        ))
                        .unwrap(),
                    "Input.dispatchMouseEvent" => {
                        socket
                            .send(Message::Text(
                                json!({"id": id, "result": {}}).to_string().into(),
                            ))
                            .unwrap();
                    }
                    method => panic!("unexpected CDP method {method}"),
                }
            }
        });

        let mut session =
            PageSession::connect_for_input_test(&format!("ws://127.0.0.1:{port}")).unwrap();
        let from = PointerTarget::coordinates(1.0, 2.0).unwrap();
        let to = PointerTarget::coordinates(11.0, 12.0).unwrap();
        let error = drag(
            &mut session,
            &from,
            &to,
            MouseButton::Left,
            2,
            Duration::from_millis(2),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("pressing mouse button to begin drag"));
        assert!(format!("{error:#}").contains("injected press failure"));
        assert!(format!("{error:#}").contains("input cleanup also failed"));
        assert!(format!("{error:#}").contains("injected release failure"));
        server.join().unwrap();
    }

    #[test]
    fn drag_releases_after_an_intermediate_movement_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = accept(stream).unwrap();
            let mut pressed = false;
            let mut failed_move = false;
            loop {
                let command = read_json_message(&mut socket);
                let id = command["id"].clone();
                match command["method"].as_str().unwrap() {
                    "Runtime.evaluate" => socket
                        .send(Message::Text({
                            let expression = command["params"]["expression"].as_str().unwrap();
                            let value =
                                if expression.contains("window.addEventListener('dragstart'") {
                                    json!(true)
                                } else if expression.contains("delete window[key]") {
                                    Value::Null
                                } else {
                                    json!({"status": "ready"})
                                };
                            json!({
                                "id": id,
                                "result": {"result": {"type": "object", "value": value}}
                            })
                            .to_string()
                            .into()
                        }))
                        .unwrap(),
                    "Input.dispatchMouseEvent" if command["params"]["type"] == "mousePressed" => {
                        pressed = true;
                        socket
                            .send(Message::Text(
                                json!({"id": id, "result": {}}).to_string().into(),
                            ))
                            .unwrap();
                    }
                    "Input.dispatchMouseEvent"
                        if pressed
                            && !failed_move
                            && command["params"]["type"] == "mouseMoved"
                            && command["params"]["buttons"] == 1 =>
                    {
                        failed_move = true;
                        socket
                        .send(Message::Text(
                            json!({
                                "id": id,
                                "error": {"code": -32000, "message": "injected movement failure"}
                            })
                            .to_string()
                            .into(),
                        ))
                        .unwrap();
                    }
                    "Input.dispatchMouseEvent" if command["params"]["type"] == "mouseReleased" => {
                        assert!(pressed);
                        assert!(failed_move);
                        assert_eq!(command["params"]["x"], 1.0);
                        assert_eq!(command["params"]["y"], 2.0);
                        assert_eq!(command["params"]["buttons"], 0);
                        socket
                            .send(Message::Text(
                                json!({"id": id, "result": {}}).to_string().into(),
                            ))
                            .unwrap();
                    }
                    "Input.setInterceptDrags" => {
                        let disabling = command["params"]["enabled"] == false;
                        socket
                            .send(Message::Text(
                                json!({"id": id, "result": {}}).to_string().into(),
                            ))
                            .unwrap();
                        if disabling {
                            break;
                        }
                    }
                    "Input.cancelDragging" => socket
                        .send(Message::Text(
                            json!({"id": id, "result": {}}).to_string().into(),
                        ))
                        .unwrap(),
                    "Input.dispatchMouseEvent" => socket
                        .send(Message::Text(
                            json!({"id": id, "result": {}}).to_string().into(),
                        ))
                        .unwrap(),
                    method => panic!("unexpected CDP method {method}"),
                }
            }
        });

        let mut session =
            PageSession::connect_for_input_test(&format!("ws://127.0.0.1:{port}")).unwrap();
        let from = PointerTarget::coordinates(1.0, 2.0).unwrap();
        let to = PointerTarget::coordinates(11.0, 12.0).unwrap();
        let error = drag(
            &mut session,
            &from,
            &to,
            MouseButton::Left,
            2,
            Duration::from_millis(2),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("dispatching drag movement step 1 of 2"));
        assert!(format!("{error:#}").contains("injected movement failure"));
        server.join().unwrap();
    }
    fn read_json_message(socket: &mut tungstenite::WebSocket<std::net::TcpStream>) -> Value {
        match socket.read().unwrap() {
            Message::Text(text) => serde_json::from_str(&text).unwrap(),
            message => panic!("unexpected command message {message:?}"),
        }
    }
}
