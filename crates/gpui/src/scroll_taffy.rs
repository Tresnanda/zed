// Included in GPUI's taffy module. Never retain an AnyElement or an arena ID.
// Snapshot only root-relative geometry and measurement inputs, indexed by the
// newly-built tree's preorder. Re-run each measure once to initialize this
// frame's TextLayout/inline flow state; changed results take the normal path.
#[derive(Clone, Copy)]
struct ScrollMeasure {
    known: Size<Option<Pixels>>,
    available: Size<AvailableSpace>,
    result: Size<Pixels>,
}
struct ScrollNode {
    style: taffy::Style,
    children: usize,
    measured: Option<ScrollMeasure>,
    bounds: Bounds<Pixels>,
    origin: Point<f32>,
}
struct ScrollSnapshot {
    available: Size<AvailableSpace>,
    scale: f32,
    nodes: Vec<ScrollNode>,
    frame: u64,
}
#[derive(Default)]
struct ScrollLayouts {
    keys: FxHashMap<LayoutId, [u64; 3]>,
    entries: FxHashMap<[u64; 3], ScrollSnapshot>,
    frame: u64,
    nodes: usize,
    hits: usize,
    reused_nodes: usize,
}
impl ScrollLayouts {
    fn next_frame(&mut self) {
        self.keys.clear();
        self.frame += 1;
        self.hits = 0;
        self.reused_nodes = 0;
        // Only the current/previous visible windows survive. Also hard-bound
        // retained geometry independently of a document's size or scroll history.
        self.entries.retain(|_, v| v.frame + 2 >= self.frame);
        self.nodes = self.entries.values().map(|v| v.nodes.len()).sum();
    }
}
impl TaffyLayoutEngine {
    pub(crate) fn scroll_layout_stats(&self) -> (usize, usize) {
        (self.scroll_layouts.hits, self.scroll_layouts.reused_nodes)
    }
    pub(crate) fn cache_scroll_layout(&mut self, id: LayoutId, key: [u64; 3]) {
        self.scroll_layouts.keys.insert(id, key);
    }
    fn scroll_nodes(&self, root: LayoutId) -> Vec<LayoutId> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            out.push(id);
            let children = self.taffy.children(id.into()).expect(EXPECT_MESSAGE);
            stack.extend(children.into_iter().rev().map(LayoutId::from));
            if out.len() > 8192 {
                return Vec::new();
            }
        }
        out
    }
    fn reuse_scroll_layout(&mut self, root: LayoutId, available: Size<AvailableSpace>, window: &mut Window, cx: &mut App) -> bool {
        let Some(key) = self.scroll_layouts.keys.get(&root).copied() else {
            return false;
        };
        let ids = self.scroll_nodes(root);
        let Some(mut snapshot) = self.scroll_layouts.entries.remove(&key) else {
            return false;
        };
        self.scroll_layouts.nodes -= snapshot.nodes.len();
        let debug = std::env::var_os("KYBERN_DEBUG_SCROLL_LAYOUT").is_some();
        if snapshot.available != available || snapshot.scale != window.scale_factor() || ids.len() != snapshot.nodes.len() {
            if debug {eprintln!("[scroll-layout-miss] root={:?} available={:?}/{:?} nodes={}/{}",key,available,snapshot.available,ids.len(),snapshot.nodes.len());}
            return false;
        }
        // Validate the entire tree before touching any frame-local measure state.
        for (&id, cached) in ids.iter().zip(&snapshot.nodes) {
            if self.taffy.style(id.into()).expect(EXPECT_MESSAGE) != &cached.style || self.taffy.child_count(id.into()) != cached.children {
                if debug {eprintln!("[scroll-layout-miss] style/topology root={:?}",key);}
                return false;
            }
        }
        for (&id, cached) in ids.iter().zip(&snapshot.nodes) {
            match (self.taffy.get_node_context_mut(id.into()), cached.measured) {
                (Some(context), Some(measure)) => {
                    let result = (context.measure)(measure.known, measure.available, window, cx);
                    context.last_measure = Some(ScrollMeasure { result, ..measure });
                    if result != measure.result {
                        if debug {eprintln!("[scroll-layout-miss] measure root={:?} size={:?}/{:?}",key,result,measure.result);}
                        return false;
                    }
                }
                // Explicitly sized measured leaves need no callback. Taffy did
                // not measure them in the reference computation either.
                (_, None) => {}
                _ => return false,
            }
        }
        for (&id, cached) in ids.iter().zip(&snapshot.nodes) {
            self.absolute_layout_bounds.insert(id, cached.bounds);
            self.absolute_outer_origins.insert(id, cached.origin);
        }
        self.scroll_layouts.hits += 1;
        self.scroll_layouts.reused_nodes += ids.len();
        self.computed_layouts.insert(root);
        static DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *DEBUG.get_or_init(|| std::env::var_os("KYBERN_DEBUG_SCROLL_LAYOUT").is_some()) {
            eprintln!("[scroll-layout] hit nodes={}", ids.len());
        }
        snapshot.frame = self.scroll_layouts.frame;
        self.scroll_layouts.nodes += snapshot.nodes.len();
        self.scroll_layouts.entries.insert(key, snapshot);
        true
    }
    fn remember_scroll_layout(&mut self, root: LayoutId, available: Size<AvailableSpace>, scale: f32) {
        let Some(key) = self.scroll_layouts.keys.get(&root).copied() else {
            return;
        };
        let ids = self.scroll_nodes(root);
        if ids.is_empty() || self.scroll_layouts.entries.len() >= 128 || self.scroll_layouts.nodes + ids.len() > 16384 {
            return;
        }
        let mut nodes = Vec::with_capacity(ids.len());
        for id in ids {
            let bounds = self.layout_bounds(id, scale);
            nodes.push(ScrollNode {
                bounds,
                origin: self.absolute_outer_origins[&id],
                style: self.taffy.style(id.into()).expect(EXPECT_MESSAGE).clone(),
                children: self.taffy.child_count(id.into()),
                measured: self.taffy.get_node_context(id.into()).and_then(|c| c.last_measure),
            });
        }
        if let Some(old) = self.scroll_layouts.entries.remove(&key) {
            self.scroll_layouts.nodes -= old.nodes.len();
        }
        self.scroll_layouts.nodes += nodes.len();
        self.scroll_layouts.entries.insert(key, ScrollSnapshot { available, scale, nodes, frame: self.scroll_layouts.frame });
    }
}
