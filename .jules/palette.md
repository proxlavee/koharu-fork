## 2024-08-21 - Custom toggle button accessibility
**Learning:** Custom UI buttons that function as toggles (like the tool selection, sidebar tabs, and transparent color option in Koharu) need `aria-pressed` to correctly announce their active state to screen readers. Checking existing components like `@koharu/ui/components/toggle.tsx` reveals that `aria-pressed` is used there, but often missed in one-off custom layouts.
**Action:** Always verify if a `Button` component visually acts as a tab or a toggle and ensure it gets an `aria-pressed` attribute reflecting its active state.
