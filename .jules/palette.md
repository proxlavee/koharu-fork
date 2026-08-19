## 2024-03-24 - Added async loading states to StartView actions
**Learning:** Users often lack immediate feedback when triggering async project actions (Create/Open/Delete), which can lead to double-clicks or confusion.
**Action:** Add `<Spinner />` components conditionally when async actions are active (`busy` state) to provide visual feedback and prevent user uncertainty.
