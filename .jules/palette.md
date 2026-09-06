## 2024-08-29 - Missing Tooltips on Critical Action Buttons
**Learning:** Icon-only buttons for critical background task actions (like stopping a job or dismissing a failure in the ActivityCenter) were only using `aria-label`. While accessible to screen readers, sighted users lack context on hover.
**Action:** Always wrap standalone icon buttons in `Tooltip` components to ensure parity between visual and screen-reader context, utilizing the `render` prop on `TooltipTrigger` for `@base-ui/react` compositions.
