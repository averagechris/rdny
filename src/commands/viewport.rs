//! Viewport and mobile emulation.

use anyhow::{Context, Result};
use serde_json::json;

use crate::session::PageSession;
use crate::state::{self, ViewportOverride};

pub fn viewport(
    sess: &mut PageSession,
    width: Option<u32>,
    height: Option<u32>,
    scale: f64,
    mobile: bool,
    reset: bool,
) -> Result<()> {
    if reset {
        let tx = state::transaction()?;
        let mut state = tx.require()?;
        sess.call("Emulation.clearDeviceMetricsOverride", json!({}))?;
        state.viewport = None;
        tx.save(&state)?;
        return Ok(());
    }

    match (width, height) {
        (Some(width), Some(height)) => {
            let tx = state::transaction()?;
            let mut state = tx.require()?;
            let override_ = ViewportOverride {
                width,
                height,
                scale,
                mobile,
            };
            sess.call("Emulation.setDeviceMetricsOverride", override_.cdp_params())?;
            state.viewport = Some(override_);
            tx.save(&state)?;
        }
        (None, None) => {
            if let Some(override_) = state::require()?.viewport {
                println!("{}", format_viewport(&override_));
            } else {
                let width = value_as_u32(sess.eval("window.innerWidth")?, "window.innerWidth")?;
                let height = value_as_u32(sess.eval("window.innerHeight")?, "window.innerHeight")?;
                println!("{width}x{height}");
            }
        }
        _ => unreachable!("clap requires width and height together"),
    }

    Ok(())
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
