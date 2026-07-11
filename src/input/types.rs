//! Shared framework-independent pointer input primitives.

use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result, bail};

use crate::selector::ElementSelector;

/// A CDP mouse button independent of the CLI framework.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

impl MouseButton {
    pub(crate) const fn buttons_mask(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Right => 2,
            Self::Middle => 4,
            Self::Back => 8,
            Self::Forward => 16,
        }
    }

    pub(super) const fn as_cdp(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Middle => "middle",
            Self::Back => "back",
            Self::Forward => "forward",
        }
    }
}

impl FromStr for MouseButton {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        if value.eq_ignore_ascii_case("left") {
            Ok(Self::Left)
        } else if value.eq_ignore_ascii_case("right") {
            Ok(Self::Right)
        } else if value.eq_ignore_ascii_case("middle") {
            Ok(Self::Middle)
        } else if value.eq_ignore_ascii_case("back") {
            Ok(Self::Back)
        } else if value.eq_ignore_ascii_case("forward") {
            Ok(Self::Forward)
        } else {
            bail!("unknown mouse button `{value}`; expected left, right, middle, back, or forward")
        }
    }
}

impl fmt::Display for MouseButton {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_cdp())
    }
}

/// A finite CSS viewport coordinate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PointerPoint {
    pub(super) x: f64,
    pub(super) y: f64,
}

impl PointerPoint {
    pub(crate) fn new(x: f64, y: f64) -> Result<Self> {
        if !x.is_finite() || !y.is_finite() {
            bail!("pointer coordinates must be finite numbers");
        }
        Ok(Self { x, y })
    }
}

impl FromStr for PointerPoint {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let mut parts = value.split(',');
        let x = parts
            .next()
            .filter(|part| !part.trim().is_empty())
            .context("pointer coordinates must use `x,y`")?;
        let y = parts
            .next()
            .filter(|part| !part.trim().is_empty())
            .context("pointer coordinates must use `x,y`")?;
        if parts.next().is_some() {
            bail!("pointer coordinates must contain exactly two values as `x,y`");
        }
        let x = x
            .trim()
            .parse::<f64>()
            .with_context(|| format!("invalid pointer x coordinate `{}`", x.trim()))?;
        let y = y
            .trim()
            .parse::<f64>()
            .with_context(|| format!("invalid pointer y coordinate `{}`", y.trim()))?;
        Self::new(x, y)
    }
}

/// Selector-backed or explicit-coordinate pointer destination.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PointerTarget {
    Selector(ElementSelector),
    Point(PointerPoint),
}

impl PointerTarget {
    pub(crate) fn selector(selector: ElementSelector) -> Self {
        Self::Selector(selector)
    }

    #[cfg(test)]
    pub(crate) fn coordinates(x: f64, y: f64) -> Result<Self> {
        Ok(Self::Point(PointerPoint::new(x, y)?))
    }

    pub(crate) const fn point(point: PointerPoint) -> Self {
        Self::Point(point)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mouse_buttons_parse_display_and_map_to_cdp_masks() {
        for (name, button, mask) in [
            ("left", MouseButton::Left, 1),
            ("RIGHT", MouseButton::Right, 2),
            ("middle", MouseButton::Middle, 4),
            ("back", MouseButton::Back, 8),
            ("forward", MouseButton::Forward, 16),
        ] {
            let parsed: MouseButton = name.parse().unwrap();
            assert_eq!(parsed, button);
            assert_eq!(parsed.buttons_mask(), mask);
            assert_eq!(parsed.to_string(), name.to_ascii_lowercase());
        }
        assert!("primary".parse::<MouseButton>().is_err());
    }

    #[test]
    fn coordinate_parser_is_strict_and_rejects_nonfinite_values() {
        assert_eq!(
            " 12.5, -4 ".parse::<PointerPoint>().unwrap(),
            PointerPoint { x: 12.5, y: -4.0 }
        );
        for raw in ["", "1", "1,", ",2", "1,2,3", "x,2", "NaN,2", "inf,2"] {
            assert!(
                raw.parse::<PointerPoint>().is_err(),
                "expected `{raw}` to fail"
            );
        }
        assert!(PointerPoint::new(f64::INFINITY, 0.0).is_err());
        assert!(PointerTarget::coordinates(0.0, f64::NAN).is_err());
    }
}
