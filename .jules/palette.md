## 2024-08-17 - Keyboard Focus States
**Learning:** Raw `<button>` elements in this app often lack proper `focus-visible` states, and sometimes have default browser outlines that conflict with Tailwind ring styles.
**Action:** When adding or modifying interactive elements, always explicitly add `outline-none focus-visible:ring-2 focus-visible:ring-ring/25` (or similar custom styles) to ensure keyboard navigation is clear and visually consistent.
