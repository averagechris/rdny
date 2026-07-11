//! Open-shadow selector traversal program builder.

pub(crate) fn selector_traversal(
    segments: &[String],
    raw: &str,
    pierce: bool,
    probe: bool,
) -> String {
    let segments = serde_json::to_string(segments).expect("selector segments serialize");
    let raw = serde_json::to_string(raw).expect("selector serializes");
    format!(
        r#"(() => {{
    const segments = {segments};
    const original = {raw};
    const pierce = {pierce};
    const probe = {probe};
    const role = (index) => !pierce
        ? 'selector'
        : (index === segments.length - 1
            ? 'target'
            : (index === 0 ? 'shadow host' : 'nested shadow host'));
    const quoted = (value) => '`' + value + '`';
    if (pierce) {{
        const validationRoot = document.createDocumentFragment();
        for (let index = 0; index < segments.length; index += 1) {{
            const css = segments[index];
            try {{
                validationRoot.querySelector(css);
            }} catch (error) {{
                const detail = error && error.message ? error.message : String(error);
                throw new Error('invalid CSS in ' + role(index) + ' segment ' + (index + 1) + ' ' + quoted(css) + ': ' + detail);
            }}
        }}
    }}
    let root = document;
    for (let index = 0; index < segments.length; index += 1) {{
        const css = segments[index];
        let element;
        try {{
            element = root.querySelector(css);
        }} catch (error) {{
            const detail = error && error.message ? error.message : String(error);
            if (!pierce) throw new Error('invalid CSS selector ' + quoted(css) + ': ' + detail);
            throw new Error('invalid CSS in ' + role(index) + ' segment ' + (index + 1) + ' ' + quoted(css) + ': ' + detail);
        }}
        if (element === null) {{
            if (probe) return {{ found: false, kind: 'missing', index, css }};
            if (!pierce) return null;
            throw new Error(role(index) + ' segment ' + (index + 1) + ' ' + quoted(css) + ' matched no element while resolving pierced selector ' + quoted(original));
        }}
        if (index === segments.length - 1) {{
            return probe ? {{ found: true }} : element;
        }}
        if (!element.shadowRoot) {{
            if (probe) return {{ found: false, kind: 'shadow-root', index, css, tag: element.localName || element.tagName || 'element' }};
            throw new Error(role(index) + ' segment ' + (index + 1) + ' ' + quoted(css) + ' matched <' + (element.localName || element.tagName || 'element') + '>, but it does not expose an open shadow root; the root may be absent or closed, and closed shadow roots are unsupported');
        }}
        root = element.shadowRoot;
    }}
    return probe ? {{ found: false, kind: 'missing', index: 0, css: '' }} : null;
}})()"#,
        pierce = pierce,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_serializes_segments_and_switches_probe_result_mode() {
        let segments = vec!["outer-host".into(), "button.save".into()];
        let resolve = selector_traversal(&segments, "outer-host >>> button.save", true, false);
        assert!(resolve.contains(r#"const segments = ["outer-host","button.save"];"#));
        assert!(resolve.contains("root = element.shadowRoot"));
        assert!(resolve.contains("const probe = false"));
        let probe = selector_traversal(&segments, "outer-host >>> button.save", true, true);
        assert!(probe.contains("const probe = true"));
        assert!(probe.contains("return probe ? { found: true } : element"));
    }
}
