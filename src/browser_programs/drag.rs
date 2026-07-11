//! Native page drag-capture program builders.

pub(crate) fn drag_capture_install(key: &str) -> String {
    format!(
        r#"(() => {{
            const key = Symbol.for({key});
            const prior = window[key];
            if (prior && prior.listener) window.removeEventListener('dragstart', prior.listener);
            const state = {{seen: false, settled: false, canceled: false, effectAllowed: 'none', items: [], listener: null}};
            state.listener = (event) => {{
                const transfer = event.dataTransfer;
                state.seen = true;
                state.effectAllowed = transfer ? String(transfer.effectAllowed || 'none') : 'none';
                state.items = transfer ? Array.from(transfer.types || [])
                    .filter((mimeType) => mimeType !== 'Files')
                    .map((mimeType) => ({{
                        mimeType: String(mimeType),
                        data: String(transfer.getData(mimeType) || ''),
                        title: '',
                        baseURL: String(location.href),
                    }})) : [];
                queueMicrotask(() => {{
                    state.canceled = event.defaultPrevented;
                    state.settled = true;
                }});
            }};
            window.addEventListener('dragstart', state.listener);
            window[key] = state;
            return true;
        }})()"#,
        key = serde_json::to_string(key).expect("capture key serializes"),
    )
}

pub(crate) fn drag_capture_take(key: &str) -> String {
    format!(
        r#"(() => {{
            const key = Symbol.for({key});
            const state = window[key];
            if (!state) return null;
            if (state.listener) window.removeEventListener('dragstart', state.listener);
            delete window[key];
            return {{
                seen: Boolean(state.seen),
                settled: Boolean(state.settled),
                canceled: Boolean(state.canceled),
                effectAllowed: String(state.effectAllowed || 'none'),
                items: Array.isArray(state.items) ? state.items : [],
            }};
        }})()"#,
        key = serde_json::to_string(key).expect("capture key serializes"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_programs_json_encode_identity_and_preserve_data_transfer_fields() {
        let key = "quote'\"\nkey";
        let encoded = serde_json::to_string(key).unwrap();
        let install = drag_capture_install(key);
        assert!(install.contains(&format!("Symbol.for({encoded})")));
        assert!(install.contains("transfer.getData(mimeType)"));
        assert!(install.contains("queueMicrotask"));
        let take = drag_capture_take(key);
        assert!(take.contains(&format!("Symbol.for({encoded})")));
        assert!(take.contains("delete window[key]"));
        assert!(take.contains("effectAllowed"));
    }
}
