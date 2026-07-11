//! Geometry and hit-testing for selector-backed pointer interactions.
//!
//! The browser computes viewport-clipped client rectangles and validates the
//! chosen point against the exact remote object that selector resolution
//! returned. Keeping this here gives click, hover, and future drag operations
//! one actionability policy without changing selector or screenshot semantics.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::session::PageSession;

/// A viewport coordinate that was hit-tested against a selected element.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ActionPoint {
    pub(crate) x: f64,
    pub(crate) y: f64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
struct ElementSummary {
    tag: String,
    id: String,
    classes: Vec<String>,
}

impl ElementSummary {
    fn display(&self) -> String {
        let tag = safe_token(&self.tag, 32);
        let mut result = if tag.is_empty() {
            "element".to_string()
        } else {
            tag
        };
        if !self.id.is_empty() {
            result.push('#');
            result.push_str(&safe_token(&self.id, 80));
        }
        for class in self
            .classes
            .iter()
            .filter(|class| !class.is_empty())
            .take(4)
        {
            result.push('.');
            result.push_str(&safe_token(class, 48));
        }
        result
    }
}

fn safe_token(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .take(max_chars)
        .flat_map(char::escape_default)
        .collect()
}

#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum TargetCheck {
    Ready {
        x: f64,
        y: f64,
        selected: ElementSummary,
    },
    Detached {
        selected: ElementSummary,
    },
    Hidden {
        selected: ElementSummary,
    },
    NoGeometry {
        selected: ElementSummary,
    },
    OutsideViewport {
        selected: ElementSummary,
    },
    PointMoved {
        selected: ElementSummary,
    },
    Intercepted {
        selected: ElementSummary,
        intercepting: Option<ElementSummary>,
    },
    InvalidTarget {
        selected: ElementSummary,
    },
}

impl TargetCheck {
    fn into_point(self) -> Result<ActionPoint> {
        match self {
            Self::Ready { x, y, selected } => {
                if !x.is_finite() || !y.is_finite() {
                    bail!(
                        "browser returned a non-finite action point for selected target {}",
                        selected.display()
                    );
                }
                Ok(ActionPoint { x, y })
            }
            Self::Detached { selected } => bail!(
                "selected target {} became detached before input dispatch",
                selected.display()
            ),
            Self::Hidden { selected } => bail!(
                "selected target {} is not visibly rendered",
                selected.display()
            ),
            Self::NoGeometry { selected } => bail!(
                "selected target {} has no nondegenerate visible client rect",
                selected.display()
            ),
            Self::OutsideViewport { selected } => bail!(
                "selected target {} has no visible client rect inside the viewport",
                selected.display()
            ),
            Self::PointMoved { selected } => bail!(
                "selected target {} moved away from the chosen point before input dispatch",
                selected.display()
            ),
            Self::Intercepted {
                selected,
                intercepting: Some(intercepting),
            } => bail!(
                "selected target {} could not find or retain an unobstructed hit-tested point; observed interceptor: {}",
                selected.display(),
                intercepting.display()
            ),
            Self::Intercepted {
                selected,
                intercepting: None,
            } => bail!(
                "selected target {} is not hit-testable at any visible candidate point",
                selected.display()
            ),
            Self::InvalidTarget { selected } => bail!(
                "selected target {} is not an element and cannot receive pointer input",
                selected.display()
            ),
        }
    }
}

impl PageSession {
    /// Scroll the exact selected node into view, derive viewport-clipped client
    /// rectangles, and return the first deterministic point whose composed-tree
    /// hit target is the node or one of its descendants.
    pub(crate) fn element_action_point(&mut self, object_id: &str) -> Result<ActionPoint> {
        self.check_element_action_point(object_id, "resolve", None)
            .context("checking selected target geometry and hit test")
    }

    /// Revalidate an already chosen point against the same remote node. Click
    /// calls this immediately after mouse movement and before mouse press so a
    /// hover handler cannot silently detach, move, or cover the target.
    pub(crate) fn revalidate_element_action_point(
        &mut self,
        object_id: &str,
        point: ActionPoint,
    ) -> Result<()> {
        self.check_element_action_point(object_id, "validate", Some(point))
            .map(|_| ())
            .context("revalidating selected target immediately before input dispatch")
    }

    fn check_element_action_point(
        &mut self,
        object_id: &str,
        mode: &str,
        point: Option<ActionPoint>,
    ) -> Result<ActionPoint> {
        let (x, y) = point
            .map(|point| (json!(point.x), json!(point.y)))
            .unwrap_or((Value::Null, Value::Null));
        let value = self.call_on(object_id, TARGET_POINT_FUNCTION, &[json!(mode), x, y])?;
        serde_json::from_value::<TargetCheck>(value)
            .context("browser returned an invalid target hit-test result")?
            .into_point()
    }
}

// One browser-side implementation serves initial point selection and exact
// point revalidation. All values crossing CDP are bounded plain data; node
// identity and composed-tree ancestry stay in the page's JavaScript realm.
const TARGET_POINT_FUNCTION: &str = r#"function(mode, pointX, pointY) {
    const target = this;
    const summary = (node) => {
        if (!(node instanceof Element)) return {tag: 'element', id: '', classes: []};
        return {
            tag: String(node.localName || node.tagName || 'element').toLowerCase().slice(0, 32),
            id: String(node.id || '').slice(0, 80),
            classes: Array.from(node.classList || []).slice(0, 4).map((value) => String(value).slice(0, 48)),
        };
    };
    const result = (status, intercepting) => ({
        status,
        selected: summary(target),
        intercepting: intercepting ? summary(intercepting) : null,
    });
    const composedParent = (node) => {
        if (!node) return null;
        if (node.assignedSlot) return node.assignedSlot;
        const parent = node.parentNode;
        return parent instanceof ShadowRoot ? parent.host : parent;
    };
    const isComposedDescendant = (node) => {
        for (let current = node; current; current = composedParent(current)) {
            if (current === target) return true;
        }
        return false;
    };
    const deepestHitAt = (x, y) => {
        let hit = document.elementFromPoint(x, y);
        const seen = new Set();
        while (hit instanceof Element && hit.shadowRoot && !seen.has(hit.shadowRoot)) {
            seen.add(hit.shadowRoot);
            const deeper = hit.shadowRoot.elementFromPoint(x, y);
            if (!(deeper instanceof Element) || deeper === hit) break;
            hit = deeper;
        }
        return hit instanceof Element ? hit : null;
    };
    const visibleRects = () => {
        const viewportWidth = Math.max(0, document.documentElement.clientWidth || window.innerWidth || 0);
        const viewportHeight = Math.max(0, document.documentElement.clientHeight || window.innerHeight || 0);
        let hadGeometry = false;
        const rects = [];
        Array.from(target.getClientRects()).forEach((rect, index) => {
            if (![rect.left, rect.top, rect.right, rect.bottom].every(Number.isFinite)) return;
            if (rect.right <= rect.left || rect.bottom <= rect.top) return;
            hadGeometry = true;
            let left = Math.max(0, rect.left);
            let top = Math.max(0, rect.top);
            let right = Math.min(viewportWidth, rect.right);
            let bottom = Math.min(viewportHeight, rect.bottom);
            if (right > left && bottom > top) {
                rects.push({left, top, right, bottom, area: (right - left) * (bottom - top), index});
            }
        });
        rects.sort((a, b) => (b.area - a.area) || (a.index - b.index));
        return {rects, hadGeometry};
    };

    if (!(target instanceof Element)) return result('invalid_target', null);
    if (!target.isConnected) return result('detached', null);
    if (mode === 'resolve') {
        Element.prototype.scrollIntoView.call(target, {block: 'center', inline: 'center', behavior: 'instant'});
        if (!target.isConnected) return result('detached', null);
    }
    const targetStyle = getComputedStyle(target);
    if (targetStyle.display === 'none' || targetStyle.visibility === 'hidden' || targetStyle.visibility === 'collapse') {
        return result('hidden', null);
    }
    for (let node = target; node; node = composedParent(node)) {
        if (node instanceof Element && Number.parseFloat(getComputedStyle(node).opacity) === 0) {
            return result('hidden', null);
        }
    }

    const geometry = visibleRects();
    if (!geometry.hadGeometry) return result('no_geometry', null);
    if (geometry.rects.length === 0) return result('outside_viewport', null);

    if (mode === 'validate') {
        const x = Number(pointX);
        const y = Number(pointY);
        const containsPoint = Number.isFinite(x) && Number.isFinite(y) && geometry.rects.some((rect) =>
            x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
        );
        if (!containsPoint) return result('point_moved', null);
        const hit = deepestHitAt(x, y);
        if (!target.isConnected) return result('detached', null);
        return isComposedDescendant(hit)
            ? {status: 'ready', x, y, selected: summary(target)}
            : result('intercepted', hit);
    }

    const factors = [
        [0.5, 0.5],
        [0.2, 0.2], [0.5, 0.2], [0.8, 0.2],
        [0.2, 0.5],             [0.8, 0.5],
        [0.2, 0.8], [0.5, 0.8], [0.8, 0.8],
        [0.1, 0.1], [0.9, 0.1], [0.1, 0.9], [0.9, 0.9],
    ];
    let firstIntercepting = null;
    for (const rect of geometry.rects) {
        for (const [fx, fy] of factors) {
            const x = rect.left + (rect.right - rect.left) * fx;
            const y = rect.top + (rect.bottom - rect.top) * fy;
            const hit = deepestHitAt(x, y);
            if (!firstIntercepting && hit) firstIntercepting = hit;
            if (isComposedDescendant(hit)) {
                if (!target.isConnected) return result('detached', null);
                return {status: 'ready', x, y, selected: summary(target)};
            }
        }
    }
    return result('intercepted', firstIntercepting);
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ready_action_point() {
        let check: TargetCheck = serde_json::from_value(json!({
            "status": "ready",
            "x": 12.5,
            "y": 44.0,
            "selected": {"tag": "button", "id": "save", "classes": []}
        }))
        .unwrap();
        assert_eq!(
            check.into_point().unwrap(),
            ActionPoint { x: 12.5, y: 44.0 }
        );
    }

    #[test]
    fn interception_error_safely_summarizes_both_elements() {
        let check: TargetCheck = serde_json::from_value(json!({
            "status": "intercepted",
            "selected": {"tag": "button", "id": "save\u{1b}[2J", "classes": ["primary"]},
            "intercepting": {"tag": "div", "id": "", "classes": ["modal", "cover\nline"]}
        }))
        .unwrap();
        let error = check.into_point().unwrap_err().to_string();
        assert_eq!(
            error,
            "selected target button#save\\u{1b}[2J.primary could not find or retain an unobstructed hit-tested point; observed interceptor: div.modal.cover\\nline"
        );
        assert!(!error.contains('\u{1b}'));
        assert!(!error.contains('\n'));
    }

    #[test]
    fn detached_and_geometry_errors_are_actionable() {
        let detached: TargetCheck = serde_json::from_value(json!({
            "status": "detached",
            "selected": {"tag": "button", "id": "gone", "classes": []}
        }))
        .unwrap();
        assert_eq!(
            detached.into_point().unwrap_err().to_string(),
            "selected target button#gone became detached before input dispatch"
        );

        let geometry: TargetCheck = serde_json::from_value(json!({
            "status": "no_geometry",
            "selected": {"tag": "span", "id": "empty", "classes": []}
        }))
        .unwrap();
        assert_eq!(
            geometry.into_point().unwrap_err().to_string(),
            "selected target span#empty has no nondegenerate visible client rect"
        );
    }
}
