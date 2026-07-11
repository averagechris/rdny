//! Bounded pull-based download stream program.

pub(crate) const STREAM_FACTORY: &str = r#"async function(max, chunkSize) {
    const url = this.href || this.currentSrc || this.src;
    const resp = await fetch(url, {credentials: 'include'});
    if (!resp.ok) throw new Error('fetch failed: HTTP ' + resp.status);
    const contentType = resp.headers.get('content-type');
    const len = Number(resp.headers.get('content-length'));
    if (Number.isFinite(len) && len > max) throw new Error('download content-length ' + len + ' exceeds max ' + max);
    const reader = resp.body && resp.body.getReader ? resp.body.getReader() : null;
    if (!reader) throw new Error('download streaming is unavailable in this page');
    let done = false, total = 0, pending = null, pendingOffset = 0;
    const encode = (value) => { let s = ''; for (let i = 0; i < value.length; i += 0x8000) s += String.fromCharCode.apply(null, value.subarray(i, i + 0x8000)); return btoa(s); };
    return {
      contentType,
      async next() {
        if (done) return {done: true, contentType, total};
        let value;
        while (!pending || pendingOffset >= pending.byteLength) {
          const read = await reader.read();
          if (read.done) { done = true; return {done: true, contentType, total}; }
          pending = read.value;
          pendingOffset = 0;
        }
        const end = Math.min(pending.byteLength, pendingOffset + chunkSize);
        value = pending.subarray(pendingOffset, end);
        pendingOffset = end;
        total += value.byteLength;
        if (total > max) { try { await reader.cancel(); } finally { done = true; } throw new Error('download exceeds max ' + max); }
        return {done: false, chunk: encode(value), contentType, total};
      },
      async cancel() { done = true; await reader.cancel(); pending = null; return true; }
    };
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_program_is_pull_based_bounded_and_never_buffers_the_full_body() {
        assert!(STREAM_FACTORY.starts_with("async function(max, chunkSize)"));
        assert!(STREAM_FACTORY.contains("reader.read()"));
        assert!(STREAM_FACTORY.contains("pendingOffset + chunkSize"));
        assert!(STREAM_FACTORY.contains("if (total > max)"));
        assert!(STREAM_FACTORY.contains("reader.cancel()"));
        assert!(!STREAM_FACTORY.contains("arrayBuffer"));
    }
}
