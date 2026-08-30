## 2025-02-18 - Optimize Layer Lookups using O(1) Maps
**Learning:** O(N) array lookups (like `.find()`) inside rendering or traversal functions (like `hitTestLayers` and `expandLayerSelection`) can create O(N^2) bottlenecks when working with highly nested layers on a canvas.
**Action:** When working with Koharu layer operations (e.g., `effectiveLayerVisibility`, `hitTestLayers`, `expandLayerSelection`), prefer passing or building a `Map<string, Layer>` for O(1) lookups to avoid O(N^2) complexity bottlenecks during real-time canvas rendering instead of using array methods like `find()`.
