## 2025-01-20 - O(N²) Layer Traversals in Canvas
**Learning:** Functions like `effectiveLayerVisibility` and `hitTestLayers` use `Array.find()` internally while being called in $O(N)$ loops over page layers. This results in $O(N^2)$ complexity, which can cause frame drops on complex manga pages during frequent events like `onPointerMove`.
**Action:** Always prefer `Map` for $O(1)$ lookups when performing tree traversals or repeated lookups in $O(N)$ array loops, especially for real-time canvas operations.
