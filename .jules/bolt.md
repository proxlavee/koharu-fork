## 2025-02-18 - Caching lookups for layer hierarchies
**Learning:** Functions that perform frequent traversal over tree-like arrays (such as layer hierarchies using `parent` properties) can be O(N^2) if relying on `Array.find` or `Array.filter`. In React, especially inside `useMemo` hooks mapping over arrays, this can cause significant rendering bottlenecks.
**Action:** When finding bottlenecks related to tree hierarchies, map the flat arrays into `Map` structures before traversal to achieve O(1) lookups, caching the maps where applicable.
