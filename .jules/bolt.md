## 2025-02-14 - Canvas Layer Performance Optimization
**Learning:** Found multiple $O(N^2)$ bottlenecks in the codebase during rendering, selection, and hit testing. `effectiveLayerVisibility`, `expandLayerSelection`, and `orderedLayers` were repeatedly searching arrays or filtering using `layerChildren` for each layer.
**Action:** Always precompute a `Map<string, Layer>` and `Map<string, Layer[]>` for quick $O(1)$ and $O(N)$ lookups instead of repeatedly filtering or searching arrays in rendering loops.
