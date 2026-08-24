## 2025-02-18 - Optimize Layer Hierarchy Traversals
**Learning:** Layer hierarchy operations like `effectiveLayerVisibility`, `hitTestLayers`, and `expandLayerSelection` frequently performed O(N) array traversals (e.g. `.find()`, `.filter()`), leading to O(N^2) complexity during performance-critical tasks like hit-testing and rendering complex documents.
**Action:** When working with Koharu layer operations that iterate through multiple parents or children, pass or build a `Map<string, Layer>` for O(1) lookups to avoid O(N^2) bottlenecks.
