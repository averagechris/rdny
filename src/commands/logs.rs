//! Console and browser log capture.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{Value, json};

use crate::cdp::client::Event;
use crate::session::PageSession;

/// Enable log domains and print console/log events.
#[allow(dead_code)]
pub fn logs(sess: &mut PageSession, follow: bool) -> Result<()> {
    logs_format(sess, follow, false)
}

pub fn logs_format(sess: &mut PageSession, follow: bool, structured: bool) -> Result<()> {
    sess.call("Runtime.enable", json!({}))?;
    sess.call("Log.enable", json!({}))?;

    let session_id = sess.session_id().to_string();
    if follow {
        loop {
            if let Some(event) = sess.next_event_follow(Duration::from_secs(1))?
                && event.session_id.as_deref() == Some(session_id.as_str())
                && let Some(line) = event_line_format(&event, structured)
            {
                println!("{line}");
            }
        }
    }

    let deadline = sess.deadline();
    while !deadline.expired() {
        if let Some(event) = sess.next_event_until(deadline)?
            && event.session_id.as_deref() == Some(session_id.as_str())
            && let Some(line) = event_line_format(&event, structured)
        {
            println!("{line}");
        }
    }
    Ok(())
}

/// Convert a CDP event into the single stdout line rdny logs should print.
#[allow(dead_code)]
pub fn event_line(event: &Event) -> Option<String> {
    event_line_format(event, false)
}

pub fn event_line_format(event: &Event, structured: bool) -> Option<String> {
    if structured {
        return structured_event(event).map(|v| v.to_string());
    }
    match event.method.as_str() {
        "Runtime.consoleAPICalled" => console_api_line(&event.params),
        "Runtime.exceptionThrown" => exception_line(&event.params),
        "Log.entryAdded" => log_entry_line(&event.params),
        _ => None,
    }
}

fn structured_event(event: &Event) -> Option<Value> {
    let (severity, text) = match event.method.as_str() {
        "Runtime.consoleAPICalled" => (
            event
                .params
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("log"),
            console_api_line(&event.params)?,
        ),
        "Runtime.exceptionThrown" => ("error", exception_line(&event.params)?),
        "Log.entryAdded" => (
            event
                .params
                .get("entry")?
                .get("level")
                .and_then(Value::as_str)
                .unwrap_or("info"),
            log_entry_line(&event.params)?,
        ),
        _ => return None,
    };
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs_f64();
    Some(
        json!({"schemaVersion":1,"kind":"log","timestamp":ts,"instance":event.session_id,"target":event.session_id,"severity":severity,"message":text}),
    )
}

fn console_api_line(params: &Value) -> Option<String> {
    let typ = params.get("type").and_then(Value::as_str)?;
    let args = params
        .get("args")
        .and_then(Value::as_array)
        .map(|args| {
            args.iter()
                .map(remote_object_text)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    Some(format!("[{typ}] {args}"))
}

fn remote_object_text(object: &Value) -> String {
    if let Some(value) = object.get("value") {
        return match value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
    }
    if let Some(description) = object.get("description").and_then(Value::as_str) {
        return description.to_string();
    }
    format!(
        "<{}>",
        object
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    )
}

fn exception_line(params: &Value) -> Option<String> {
    let details = params.get("exceptionDetails")?;
    let text = details
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("exception");
    let description = details
        .get("exception")
        .and_then(|exception| exception.get("description"))
        .and_then(Value::as_str);
    Some(match description {
        Some(description) if !description.is_empty() => format!("[error] {text} {description}"),
        _ => format!("[error] {text}"),
    })
}

fn log_entry_line(params: &Value) -> Option<String> {
    let entry = params.get("entry")?;
    let level = entry.get("level").and_then(Value::as_str)?;
    let source = entry
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let text = entry.get("text").and_then(Value::as_str).unwrap_or("");
    Some(format!("[{level}] ({source}) {text}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(method: &str, params: Value) -> Event {
        Event {
            method: method.to_string(),
            params,
            session_id: Some("s1".to_string()),
        }
    }

    #[test]
    fn formats_console_api_args() {
        let line = event_line(&event(
            "Runtime.consoleAPICalled",
            json!({
                "type": "warn",
                "args": [
                    {"type": "string", "value": "hello"},
                    {"type": "number", "value": 3},
                    {"type": "object", "description": "Object"},
                    {"type": "undefined"}
                ]
            }),
        ));
        assert_eq!(line.as_deref(), Some("[warn] hello 3 Object <undefined>"));
    }

    #[test]
    fn formats_exception_thrown() {
        let line = event_line(&event(
            "Runtime.exceptionThrown",
            json!({
                "exceptionDetails": {
                    "text": "Uncaught",
                    "exception": {"description": "Error: boom"}
                }
            }),
        ));
        assert_eq!(line.as_deref(), Some("[error] Uncaught Error: boom"));
    }

    #[test]
    fn formats_log_entry_and_ignores_other_events() {
        let line = event_line(&event(
            "Log.entryAdded",
            json!({"entry": {"level": "error", "source": "javascript", "text": "failed"}}),
        ));
        assert_eq!(line.as_deref(), Some("[error] (javascript) failed"));
        assert!(event_line(&event("Page.loadEventFired", json!({}))).is_none());
    }
}
