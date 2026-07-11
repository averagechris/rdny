//! Element-selector parsing and shared page-side traversal.

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// Delimiter interpreted between CSS segments only when `--pierce` is set.
pub const SHADOW_DELIMITER: &str = ">>>";

/// A selector for one element, optionally traversing nested open shadow roots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElementSelector {
    raw: String,
    segments: Vec<String>,
    pierce: bool,
}

impl ElementSelector {
    /// Parse a CLI selector. Without `pierce`, the value is preserved as one
    /// ordinary CSS selector, including any `>>>` text it contains.
    pub fn parse(raw: impl Into<String>, pierce: bool) -> Result<Self> {
        let raw = raw.into();
        let segments = if pierce {
            raw.split(SHADOW_DELIMITER)
                .map(str::trim)
                .enumerate()
                .map(|(index, segment)| {
                    if segment.is_empty() {
                        let position = index + 1;
                        bail!(
                            "pierced selector segment {position} is empty; provide a CSS selector on both sides of every `{SHADOW_DELIMITER}` delimiter"
                        );
                    }
                    Ok(segment.to_string())
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            vec![raw.clone()]
        };

        Ok(Self {
            raw,
            segments,
            pierce,
        })
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// Build the single traversal used by both required element resolution and
    /// wait probes. Every segment is passed separately to `querySelector`, so
    /// Chromium validates CSS in the document/shadow-root context where it is
    /// used.
    pub(crate) fn resolution_expression(&self, probe: bool) -> String {
        crate::browser_programs::selector_traversal(&self.segments, &self.raw, self.pierce, probe)
    }

    pub(crate) fn describe_unresolved(&self, probe: &ElementProbe) -> String {
        match probe {
            ElementProbe::Found => format!("selector `{}` resolved", self.raw),
            ElementProbe::Missing { index, css } if self.pierce => format!(
                "{} segment {} `{css}` matched no element while waiting for pierced selector `{}`",
                self.segment_role(*index),
                index + 1,
                self.raw
            ),
            ElementProbe::Missing { .. } => {
                format!("no element matches CSS selector `{}`", self.raw)
            }
            ElementProbe::ShadowRoot { index, css, tag } => format!(
                "{} segment {} `{css}` matched <{tag}>, but it does not expose an open shadow root while waiting for pierced selector `{}`; the root may be absent or closed, and closed shadow roots are unsupported",
                self.segment_role(*index),
                index + 1,
                self.raw
            ),
        }
    }

    fn segment_role(&self, index: usize) -> &'static str {
        if index == self.segments.len().saturating_sub(1) {
            "target"
        } else if index == 0 {
            "shadow host"
        } else {
            "nested shadow host"
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ElementProbe {
    Found,
    Missing {
        index: usize,
        css: String,
    },
    ShadowRoot {
        index: usize,
        css: String,
        tag: String,
    },
}

impl ElementProbe {
    pub(crate) fn from_value(value: &Value) -> Result<Self> {
        if value["found"].as_bool() == Some(true) {
            return Ok(Self::Found);
        }
        let kind = value["kind"]
            .as_str()
            .context("selector probe response missing kind")?;
        let index = value["index"]
            .as_u64()
            .and_then(|index| usize::try_from(index).ok())
            .context("selector probe response missing segment index")?;
        let css = value["css"]
            .as_str()
            .context("selector probe response missing CSS segment")?
            .to_string();
        match kind {
            "missing" => Ok(Self::Missing { index, css }),
            "shadow-root" => Ok(Self::ShadowRoot {
                index,
                css,
                tag: value["tag"]
                    .as_str()
                    .context("selector probe response missing host tag")?
                    .to_string(),
            }),
            other => bail!("selector probe returned unknown result kind `{other}`"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ordinary_css_preserves_delimiter_and_whitespace_as_one_selector() {
        let selector = ElementSelector::parse("  x-host >>> button  ", false).unwrap();
        assert_eq!(selector.raw(), "  x-host >>> button  ");
        assert_eq!(selector.segments, ["  x-host >>> button  "]);
        assert!(!selector.pierce);
        let expression = selector.resolution_expression(false);
        assert!(expression.contains(r#"const segments = ["  x-host >>> button  "];"#));
        assert!(expression.contains("const pierce = false"));
    }

    #[test]
    fn pierced_selector_splits_and_trims_nested_segments() {
        let selector =
            ElementSelector::parse("outer-host >>> section.inner >>> button.save", true).unwrap();
        assert_eq!(
            selector.segments,
            ["outer-host", "section.inner", "button.save"]
        );
        assert!(selector.pierce);
        let expression = selector.resolution_expression(false);
        assert!(
            expression
                .contains(r#"const segments = ["outer-host","section.inner","button.save"];"#)
        );
        assert!(expression.contains("root.querySelector(css)"));
        assert!(expression.contains("validationRoot.querySelector(css)"));
        assert!(expression.contains("root = element.shadowRoot"));
    }

    #[test]
    fn pierced_selector_rejects_every_empty_segment_position() {
        for (raw, position) in [
            (">>> button", 1),
            ("host >>> >>> button", 2),
            ("host >>>", 2),
        ] {
            let error = ElementSelector::parse(raw, true).unwrap_err().to_string();
            assert!(
                error.contains(&format!("segment {position} is empty")),
                "{error}"
            );
            assert!(error.contains("both sides"), "{error}");
        }
    }

    #[test]
    fn probe_results_produce_segment_specific_wait_errors() {
        let selector = ElementSelector::parse("outer >>> inner >>> button", true).unwrap();
        let missing = ElementProbe::from_value(&json!({
            "found": false,
            "kind": "missing",
            "index": 1,
            "css": "inner"
        }))
        .unwrap();
        assert_eq!(
            selector.describe_unresolved(&missing),
            "nested shadow host segment 2 `inner` matched no element while waiting for pierced selector `outer >>> inner >>> button`"
        );

        let closed = ElementProbe::from_value(&json!({
            "found": false,
            "kind": "shadow-root",
            "index": 0,
            "css": "outer",
            "tag": "outer-host"
        }))
        .unwrap();
        let message = selector.describe_unresolved(&closed);
        assert!(message.contains("shadow host segment 1 `outer`"));
        assert!(message.contains("does not expose an open shadow root"));
        assert!(message.contains("closed shadow roots are unsupported"));
    }
}
