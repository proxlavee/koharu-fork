# Koharu Project Rules

Document only durable, repository-specific constraints here. Do not record current file layouts, temporary paths, model inventories, helper names, or other implementation details that may change during a refactor. Normal Rust, TypeScript, testing, formatting, and Git practices are assumed.

## Change Policy

- Never add backward compatibility. When an API, schema, configuration, or ownership boundary changes, update every in-repository consumer and remove the replaced form.
- Prefer a coherent ownership redesign over aliases, forwarding layers, compatibility parsers, or cosmetic renaming.
- Keep responsibilities self-contained. Defaults and provider-specific behavior belong to the component that owns them rather than a central list of special cases.
- Remove dead abstractions and one-use helpers when direct code is clearer.

## Deliberate Execution

- Maintain a multi-step plan for non-trivial work and update it after each material milestone. Append new feedback to the ordered plan unless the user explicitly replaces or reprioritizes the active task.
- Before changing behavior, trace the affected workflow across scene ownership, application commands, generated bridges, desktop UI, retained rendering, persistence, and export as applicable. Resolve ambiguous requirements from repository evidence or ask before choosing behavior that would change the product contract.
- Never suppress, bypass, downgrade, or silently accept formatting, lint, type, compile, test, runtime, or behavioral failures. Fix the cause or report the exact blocker and the unverified claim.
- Static checks do not prove a desktop workflow or rendered result. Exercise the smallest real behavior that crosses the changed ownership boundary, and obtain final-window or exported-pixel evidence for rendering changes when the target environment is available. End-to-end runs remain opt-in under the Verification section.
- Before handoff, inspect the complete diff and worktree, then reread every edited file and user-facing explanation from start to finish. Recheck requirements, dependent flows, edge cases, generated-source boundaries, and every validation result after correcting any issue found.

## Real-Use Completeness

- Keep the page lifecycle connected from import through detection, OCR, translation, typesetting, review, and export. A surfaced control must reach its owning command and scene mutation, provide progress or actionable failure, and affect the retained canvas and export where promised.
- Preserve scene provenance and edit history. Generated processing must not overwrite user-authored content, and user-visible mutations must participate coherently in undo, redo, cancellation, persistence, and incremental rendering.
- Keep ordered chapter translation export and import complete and fail closed on mismatched pages, segment counts, or ordering. Never guess which source segment a translation belongs to.
- Selection and batch actions must operate on the visible intended entities and leave an understandable selection state. Destructive page or layer edits must remain recoverable through the project's history model rather than creating an unrecoverable UI dead end.
- Do not ship placeholder controls, no-op handlers, disconnected settings, or success messages that precede the native operation. When Windows, CEF, WebGPU, or accelerator behavior cannot be exercised locally, provide a reproducible validation path and do not call that behavior verified until its evidence exists.

## Source Boundaries

- Keep safe public APIs separate from unsafe FFI, dynamic loading, and build integration.
- Do not hand-edit generated or derived source. Change its authoritative input and run the generator.
- Do not commit credentials, model weights, datasets, generated outputs, or machine-specific artifacts.

## ML Architecture

- Keep a consistent public lifecycle across models while allowing model-specific inputs and outputs.
- Separate network ownership and weight loading from preprocessing, postprocessing, slicing, and public result types.
- Avoid pass-through types and layers that do not own a real responsibility.
- Accept a device abstraction at the model boundary, convert it once, and avoid unnecessary transfers or synchronization.
- Use the established runtime and variable-store loading paths unless they are proven insufficient.
- Disable gradient tracking during inference.

## Upstream Alignment

- Keep ports structurally traceable to a commit-pinned authoritative implementation.
- Preserve checkpoint-affecting names, construction order, parameter paths, tensor layouts, execution order, and postprocessing semantics.
- Treat missing or unexpected weights as an architecture or parameter-name mismatch before changing the loader.
- Explain intentional divergences next to the affected code.
- Compare ports on identical inputs using structured outputs such as shapes, ranges, boxes, scores, masks, and ordering.

## Performance

- Optimize and benchmark the actual target device with representative inputs.
- Remove redundant transfers, synchronization, allocations, and per-pixel host loops before adding concurrency or caching.
- Account for asynchronous accelerator execution when timing work.
- Load assets and warm models outside measured regions.
- Report the device, input size, baseline, result, and correctness difference.

## Verification

- Optimize for fast development and iteration. By default, run the smallest relevant check or focused test once using the debug profile.
- Do not run full test suites, repeatedly rerun unchanged tests or builds, or build and test profiles other than debug unless the user explicitly requests it.
- Run end-to-end tests only when the user explicitly asks for them.

## Desktop UI Debugging

- Debug builds must expose the CEF remote debugging endpoint at `http://127.0.0.1:4000` through CEF's command-line arguments.
- Connect `chrome-devtools-mcp` with `--browser-url=http://127.0.0.1:4000` and prefer its tools for WebView inspection and automation. Use semantic targets and observable conditions instead of coordinate-only actions or fixed delays.
- Use a lower-level CDP client only when `chrome-devtools-mcp` does not expose a required protocol operation. Use native window capture when CDP cannot observe the final WebGPU output.

## Desktop Rendering

- Koharu presents canvas pixels through the `koharu-canvas` WASM module and WebGPU inside the standard Tauri webview. Keep durable scene preparation and export native, keep transient canvas interaction in the browser, and validate WebGPU presentation through the final desktop window.

## Documentation

- Comments should explain ownership, invariants, upstream mapping, or deliberate divergence; do not narrate straightforward code.
- Keep this file focused on long-lived decision rules rather than the current implementation.
