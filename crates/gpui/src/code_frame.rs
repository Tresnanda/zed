//! Capacity policy for a sharp drop in a frame's dispatch-node workload.
// Keep warm storage during scrolling and small viewport changes. Nodes are
// addressed by indices, so shrinking spare Vec capacity preserves every ID.
pub(crate) fn target(capacity: usize, used: usize) -> Option<usize> {
    (capacity > 4096 && used.saturating_mul(8) < capacity)
        .then(|| used.saturating_mul(2).max(256))
}
pub(crate) fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("KYBERN_CODE_MEMORY").as_deref() != Ok("0")
        && std::env::var("KYBERN_CODE_FRAME_MEMORY").as_deref() != Ok("0"))
}
/// Install a callback that releases unused allocator pages after an arena reset.
pub fn set_code_memory_collector(callback: fn(bool)) {
    let _ = COLLECTOR.set(callback);
}
static COLLECTOR: std::sync::OnceLock<fn(bool)> = std::sync::OnceLock::new();
pub(crate) fn memory_released(contracted: bool) {
    if let Some(callback) = COLLECTOR.get() { callback(contracted); }
}
pub(crate) fn post_memory_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| enabled() && std::env::var("KYBERN_CODE_POST_FRAME_MEMORY").as_deref() != Ok("0"))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_closed_document_returns_capacity_but_a_warm_document_keeps_it() {
        let mut nodes: Vec<usize> = (0..48000).collect();
        let capacity = nodes.capacity();
        assert_eq!(target(capacity, nodes.len()), None);
        nodes.truncate(14);
        nodes.shrink_to(target(capacity, nodes.len()).unwrap());
        assert!(nodes.capacity() <= 256);
        assert_eq!(nodes, (0..14).collect::<Vec<_>>());
    }
    #[test]
    fn backup_frame_uses_the_current_frames_workload_as_its_hint() {
        assert_eq!(target(65536, 12000), None);
        assert_eq!(target(65536, 8192), None);
        assert_eq!(target(65536, 14), Some(256));
        assert_eq!(target(4096, 0), None);
        assert_eq!(target(65536, usize::MAX), None);
    }
}
