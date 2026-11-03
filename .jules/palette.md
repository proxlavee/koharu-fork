
## 2026-08-23 - Async Action Feedback Pattern
**Learning:** Adding explicit loading indicators (like `Spinner`) inside buttons or interactive elements for async actions (e.g., project creation, project opening) significantly improves perceived performance and prevents users from being confused when interactions are disabled during `busy` states without feedback.
**Action:** Always provide an inline `Spinner` or loading state for any user-initiated async operation that transitions to a "busy" or "disabled" state in UI components.
