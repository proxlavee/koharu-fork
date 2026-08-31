## 2024-05-18 - Optimize Layer Lookups
**Learning:** Found an O(N^2) bottleneck when executing Canvas rendering and interaction logic where `effectiveLayerVisibility` and other loops perform inner `find()` queries inside arrays on every node tick.
**Action:** Always prefer building a Map of `Layer`s for O(1) key lookups instead of executing `Array.find` or `Array.filter` when processing hierarchical elements continuously.
