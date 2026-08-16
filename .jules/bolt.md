## 2024-08-16 - Prevent unnecessary rendering due to derived state on each update
**Learning:** Found unnecessary object allocations for `Object.values(jobs)` and filtering that happen every render in `ActivityCenter.tsx`.
**Action:** These should be wrapped in `useMemo` to only recompute when `jobs` or `downloads` state changes.
