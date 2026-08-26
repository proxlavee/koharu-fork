## 2025-02-12 - [O(1) layer lookups to avoid bottlenecks]
**Learning:** In operations that are part of rendering/hit-testing loops (like `effectiveLayerVisibility`), iterating through `layers.find` leads to an O(N^2) complexity bottleneck.
**Action:** Use a `Map<string, Layer>` for O(1) lookups and build it once before the loop rather than repeatedly finding items inside the loop. This significantly speeds up rendering in CanvasOverlay and hit test evaluations.
