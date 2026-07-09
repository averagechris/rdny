//! Screenshots: screenshot, screenshot-el.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::session::PageSession;
use crate::state::ViewportOverride;

/// Capture a page screenshot (default file: screenshot.png).
pub fn screenshot(
    sess: &mut PageSession,
    width: Option<u32>,
    height: Option<u32>,
    file: Option<&Path>,
    force: bool,
    persisted_viewport: Option<&ViewportOverride>,
) -> Result<()> {
    let override_set = width.is_some() || height.is_some();
    if override_set {
        let w = match width {
            Some(w) => w,
            None => value_as_u32(sess.eval("window.innerWidth")?, "window.innerWidth")?,
        };
        let h = match height {
            Some(h) => h,
            None => value_as_u32(sess.eval("window.innerHeight")?, "window.innerHeight")?,
        };
        sess.call(
            "Emulation.setDeviceMetricsOverride",
            json!({"width": w, "height": h, "deviceScaleFactor": 1, "mobile": false}),
        )?;
    }

    let result = (|| {
        let result = sess.call("Page.captureScreenshot", json!({"format": "png"}))?;
        save_screenshot(result, file, force)
    })();

    if override_set {
        let _ = crate::commands::viewport::restore_persisted(sess, persisted_viewport);
    }

    result
}

/// Capture a screenshot clipped to the first selector match.
pub fn screenshot_el(
    sess: &mut PageSession,
    selector: &str,
    file: Option<&Path>,
    force: bool,
) -> Result<()> {
    let object_id = sess.element(selector)?;
    let _ = sess.call("DOM.scrollIntoViewIfNeeded", json!({"objectId": object_id}));
    let result = sess.call("DOM.getBoxModel", json!({"objectId": object_id}))?;
    let quad_values = result["model"]["content"]
        .as_array()
        .context("element has no content box")?;
    let quad: Vec<f64> = quad_values
        .iter()
        .map(|v| v.as_f64().unwrap_or(0.0))
        .collect();
    let (x, y, width, height) = quad_to_rect(&quad);
    let result = sess.call(
        "Page.captureScreenshot",
        json!({
            "format": "png",
            "clip": {"x": x, "y": y, "width": width, "height": height, "scale": 1},
            "captureBeyondViewport": true,
        }),
    )?;
    save_screenshot(result, file, force)
}

fn save_screenshot(result: Value, file: Option<&Path>, force: bool) -> Result<()> {
    let data = result["data"]
        .as_str()
        .context("screenshot response missing data")?;
    let bytes = crate::commands::decode_base64(data)?;
    let default = Path::new("screenshot.png");
    let path = file.unwrap_or(default);
    crate::commands::artifacts::write_artifact(path, &bytes, force)
        .with_context(|| format!("writing screenshot {}", path.display()))?;
    println!("saved {}", path.display());
    Ok(())
}

fn value_as_u32(value: Value, name: &str) -> Result<u32> {
    let n = value
        .as_u64()
        .with_context(|| format!("{name} is not a number"))?;
    u32::try_from(n).with_context(|| format!("{name} is too large"))
}

fn quad_to_rect(quad: &[f64]) -> (f64, f64, f64, f64) {
    let xs = [quad[0], quad[2], quad[4], quad[6]];
    let ys = [quad[1], quad[3], quad[5], quad[7]];
    let min_x = xs.into_iter().fold(f64::INFINITY, f64::min);
    let max_x = xs.into_iter().fold(f64::NEG_INFINITY, f64::max);
    let min_y = ys.into_iter().fold(f64::INFINITY, f64::min);
    let max_y = ys.into_iter().fold(f64::NEG_INFINITY, f64::max);
    (min_x, min_y, max_x - min_x, max_y - min_y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quad_to_rect_handles_typical_quad() {
        assert_eq!(
            quad_to_rect(&[10.0, 20.0, 110.0, 20.0, 110.0, 70.0, 10.0, 70.0]),
            (10.0, 20.0, 100.0, 50.0)
        );
    }

    #[test]
    fn quad_to_rect_handles_degenerate_quad() {
        assert_eq!(
            quad_to_rect(&[5.0, 6.0, 5.0, 6.0, 5.0, 6.0, 5.0, 6.0]),
            (5.0, 6.0, 0.0, 0.0)
        );
    }
}
