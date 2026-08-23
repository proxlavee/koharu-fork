
## 2024-05-18 - Array.find() inside nested loops causes O(N^2) bottlenecks
**Learning:** Functions like `effectiveLayerVisibility` and `expandLayerSelection` used `Array.find()` inside loops and recursion. With deep layer hierarchies, this led to O(N^2) complexity, causing significant lag during real-time canvas rendering and hit testing.
**Action:** Pre-compute a `Map<string, Layer>` and pass it around when traversing parent-child layer relationships to reduce lookup time to O(1).
