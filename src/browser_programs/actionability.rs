//! Element actionability and coordinate hit-test programs.

pub(crate) const TARGET_ACTIONABILITY: &str = r#"function(mode, pointX, pointY) {
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

pub(crate) const COORDINATE_HIT_TEST: &str = r#"function(pointX, pointY) {
    const x = Number(pointX);
    const y = Number(pointY);
    const viewportWidth = Math.max(0, document.documentElement.clientWidth || window.innerWidth || 0);
    const viewportHeight = Math.max(0, document.documentElement.clientHeight || window.innerHeight || 0);
    if (!Number.isFinite(x) || !Number.isFinite(y) || x < 0 || y < 0 || x >= viewportWidth || y >= viewportHeight) {
        return {status: 'outside_viewport'};
    }
    return document.elementFromPoint(x, y) instanceof Element
        ? {status: 'ready'}
        : {status: 'not_hit_testable'};
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actionability_program_retains_composed_hit_testing_and_revalidation_modes() {
        assert!(TARGET_ACTIONABILITY.starts_with("function(mode, pointX, pointY)"));
        assert!(TARGET_ACTIONABILITY.contains("const isComposedDescendant"));
        assert!(TARGET_ACTIONABILITY.contains("mode === 'validate'"));
        assert!(TARGET_ACTIONABILITY.contains("status: 'ready'"));
        assert!(COORDINATE_HIT_TEST.contains("document.elementFromPoint(x, y)"));
        assert!(COORDINATE_HIT_TEST.contains("outside_viewport"));
    }
}
