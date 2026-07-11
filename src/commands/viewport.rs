//! Viewport and mobile emulation.

use anyhow::{Context, Result};
use serde_json::json;

use crate::commands::OutputFormat;
use crate::session::PageSession;
use crate::state::{self, ViewportOverride};

#[allow(dead_code)]
pub fn viewport(
    sess: &mut PageSession,
    width: Option<u32>,
    height: Option<u32>,
    scale: f64,
    mobile: bool,
    reset: bool,
) -> Result<()> {
    viewport_format(
        sess,
        width,
        height,
        scale,
        mobile,
        reset,
        OutputFormat::Human,
    )
}

pub fn viewport_format(
    sess: &mut PageSession,
    width: Option<u32>,
    height: Option<u32>,
    scale: f64,
    mobile: bool,
    reset: bool,
    format: OutputFormat,
) -> Result<()> {
    if reset {
        sess.call("Emulation.clearDeviceMetricsOverride", json!({}))?;
        state::update(|state| {
            state.viewport = None;
            Ok(())
        })?;
        if format.is_structured() {
            format.emit_json(&serde_json::json!({"schemaVersion":1,"kind":"viewport","reset":true,"viewport":null}))?;
        }
        return Ok(());
    }

    match (width, height) {
        (Some(width), Some(height)) => {
            let override_ = ViewportOverride {
                width,
                height,
                scale,
                mobile,
            };
            sess.call("Emulation.setDeviceMetricsOverride", override_.cdp_params())?;
            state::update(|state| {
                state.viewport = Some(override_.clone());
                Ok(())
            })?;
            if format.is_structured() {
                format.emit_json(&viewport_json(&override_, true))?;
            }
        }
        (None, None) => {
            if let Some(override_) = state::require()?.viewport {
                if format.is_structured() {
                    format.emit_json(&viewport_json(&override_, false))?;
                } else {
                    println!("{}", format_viewport(&override_));
                }
            } else {
                let width = value_as_u32(sess.eval("window.innerWidth")?, "window.innerWidth")?;
                let height = value_as_u32(sess.eval("window.innerHeight")?, "window.innerHeight")?;
                if format.is_structured() {
                    format.emit_json(&serde_json::json!({"schemaVersion":1,"kind":"viewport","applied":false,"width":width,"height":height,"scale":1.0,"mobile":false}))?;
                } else {
                    println!("{width}x{height}");
                }
            }
        }
        _ => unreachable!("clap requires width and height together"),
    }

    Ok(())
}

fn viewport_json(viewport: &ViewportOverride, applied: bool) -> serde_json::Value {
    serde_json::json!({"schemaVersion":1,"kind":"viewport","applied":applied,"width":viewport.width,"height":viewport.height,"scale":viewport.scale,"mobile":viewport.mobile})
}

pub fn restore_persisted(
    sess: &mut PageSession,
    viewport: Option<&ViewportOverride>,
) -> Result<()> {
    if let Some(viewport) = viewport {
        sess.call("Emulation.setDeviceMetricsOverride", viewport.cdp_params())?;
    } else {
        sess.call("Emulation.clearDeviceMetricsOverride", json!({}))?;
    }
    Ok(())
}

pub fn format_viewport(viewport: &ViewportOverride) -> String {
    let mut parts = vec![format!("{}x{}", viewport.width, viewport.height)];
    if viewport.scale != 1.0 {
        parts.push(format!("scale={}", viewport.scale));
    }
    if viewport.mobile {
        parts.push("mobile".to_string());
    }
    parts.join(" ")
}

fn value_as_u32(value: serde_json::Value, name: &str) -> Result<u32> {
    let n = value
        .as_u64()
        .with_context(|| format!("{name} is not a number"))?;
    u32::try_from(n).with_context(|| format!("{name} is too large"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn override_(scale: f64, mobile: bool) -> ViewportOverride {
        ViewportOverride {
            width: 375,
            height: 812,
            scale,
            mobile,
        }
    }

    #[test]
    fn formats_viewport_description() {
        assert_eq!(format_viewport(&override_(1.0, false)), "375x812");
        assert_eq!(format_viewport(&override_(2.0, false)), "375x812 scale=2");
        assert_eq!(format_viewport(&override_(1.0, true)), "375x812 mobile");
        assert_eq!(
            format_viewport(&override_(2.0, true)),
            "375x812 scale=2 mobile"
        );
    }
}
