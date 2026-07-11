//! DOM stability instrumentation programs.

pub(crate) const MUTATION_CLOCK: &str = r#"(() => {
    const key = Symbol.for('rdny.waitstable.mutationClock.v1');
    if (window[key] && window[key].observer) return true;
    const state = { last: performance.now(), observer: null };
    state.observer = new MutationObserver(() => { state.last = performance.now(); });
    state.observer.observe(document, { subtree: true, childList: true, attributes: true, characterData: true });
    Object.defineProperty(window, key, { value: state, configurable: true });
    return true;
})()"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutation_clock_is_idempotent_and_observes_all_stability_inputs() {
        assert!(MUTATION_CLOCK.contains("if (window[key] && window[key].observer)"));
        assert!(MUTATION_CLOCK.contains("subtree: true"));
        assert!(MUTATION_CLOCK.contains("childList: true"));
        assert!(MUTATION_CLOCK.contains("attributes: true"));
        assert!(MUTATION_CLOCK.contains("characterData: true"));
    }
}
