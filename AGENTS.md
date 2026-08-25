# Koharu Project Rules

Document only durable, repository-specific constraints here. Do not record current file layouts, temporary paths, model inventories, helper names, or other implementation details that may change during a refactor. Normal Rust, TypeScript, testing, formatting, and Git practices are assumed.

## Change Policy

- Never add backward compatibility. When an API, schema, configuration, or ownership boundary changes, update every in-repository consumer and remove the replaced form.
- Prefer a coherent ownership redesign over aliases, forwarding layers, compatibility parsers, or cosmetic renaming.
- Keep responsibilities self-contained. Defaults and provider-specific behavior belong to the component that owns them rather than a central list of special cases.
- Remove dead abstractions and one-use helpers when direct code is clearer.

## Deliberate Execution (MUST)

- Operate with maximum diligence. Correctness, completeness, stability, and evidence outrank speed, convenience, token cost, or minimizing effort. Never rush, cut investigation short, or stop at the first plausible solution.
- Use the strongest available reasoning and investigation. For difficult, unfamiliar, architectural, debugging, ML, rendering, performance, or cross-system work, investigate deeply before committing to an implementation; never reduce reasoning depth merely because the task initially appears simple.
- Treat every unverified belief as unknown. Do not assume requirements, code behavior, APIs, data shapes, ownership, platform behavior, model behavior, or user intent when they can be established from repository evidence, runtime evidence, authoritative documentation, upstream source, or available tools.
- Distinguish verified facts from inference. Never present an inference as established fact. When definitive verification is impossible, identify exactly what is proven, what remains uncertain, and why.
- For every non-trivial task, establish and maintain a multi-step plan before editing. Keep it synchronized with actual progress and incorporate later user feedback without losing unresolved requirements unless explicitly replaced or reprioritized.
- Planning must support execution, not replace it. Once sufficient evidence exists, proceed autonomously through investigation, implementation, validation, and review until the requested outcome is complete or a genuine blocker is reached.
- Perform a context sweep before changing behavior. Trace relevant producers, consumers, ownership boundaries, adjacent modules, contracts, types, generated boundaries, persistence, UI, i18n, rendering, model/runtime integration, tests, and export paths as applicable.
- Read enough surrounding implementation to understand the existing design and its invariants. Do not make isolated edits to the named file or symbol without checking affected callers, dependents, and parallel flows.
- Prefer repository evidence over guesses and authoritative upstream evidence over memory. For behavior that may have changed, verify against current primary documentation, specifications, releases, source code, or reproducible runtime behavior.
- Resolve ambiguity through investigation first. Ask the user only when a material product decision remains genuinely unresolved after examining available evidence and different interpretations would produce meaningfully different behavior.
- Never silently choose among materially different interpretations of a requirement when evidence cannot establish the intended contract.
- When multiple solutions are viable, evaluate their consequences and choose based on correctness, architectural coherence, maintainability, user-visible behavior, and measured performance rather than implementation convenience.
- Prefer root-cause fixes over symptom patches. Determine why the problem exists and whether the same defect affects adjacent paths, states, platforms, ownership boundaries, or data flows.
- Do not accept a fix merely because one reproduction passes or a failing check becomes green. Verify that the underlying invariant and intended behavior are restored.
- Do not speculate, fabricate context, or deliver half-verified findings. Continue investigating until evidence supports the conclusion or a genuine external limitation prevents further verification.
- Never suppress, bypass, downgrade, ignore, or silently accept formatting, lint, type, compile, test, runtime, rendering, performance, or behavioral failures to make progress appear complete. Fix the cause or report the exact blocker.
- Validate at the level that can actually prove the claim. Static analysis proves static properties only; compilation proves compilation only; unit tests prove only their tested contracts. Exercise real integration, UI, rendering, accelerator, persistence, or export behavior when the change depends on it and the required environment is available.
- Do not confuse successful execution with semantic correctness. A command completing, application starting, model loading, inference returning tensors, UI action not crashing, or test passing is insufficient unless the resulting behavior matches the intended contract.
- For regressions, reproduce the failure or establish equivalent objective evidence before fixing it whenever practical, then verify the same condition after the fix.
- For ML or numerical work, verify semantic outputs against the authoritative implementation or established baseline when one exists, including relevant shapes, ranges, ordering, scores, masks, or generated pixels.
- For UI and rendering work, inspect the actual user-visible result when possible. DOM state, component state, successful commands, or intermediate buffers do not prove final rendering correctness.
- For performance work, measure before and after under comparable conditions, identify the actual bottleneck before optimizing, and verify that correctness is preserved.
- After each material implementation phase, compare the result against the original request and all accumulated feedback. Keep every requested behavior and constraint traceable through completion.
- Before handoff, inspect the complete diff and worktree and reread every edited file. Check for accidental changes, missed consumers, incomplete migrations, duplicated ownership, stale code, dead abstractions, placeholders, debug artifacts, temporary workarounds, and inconsistencies introduced during iteration.
- Perform a final self-audit covering requirements, later feedback, edge cases, failure paths, ownership boundaries, UI/UX effects, persistence/history where applicable, automated checks, manual validation, runtime evidence, performance implications, and remaining risks.
- After correcting anything found during review, rerun the smallest validation necessary to prove the corrected behavior.
- Continue through recoverable failures autonomously. Inspect logs, surrounding code, alternate approaches, and available tooling before declaring a blocker.
- Escalate only for genuine blockers, unavailable required resources, destructive or irreversible decisions requiring user authority, security-sensitive actions requiring approval, or material product ambiguity that evidence cannot resolve.
- If investigation reveals broader correctness or architectural implications than the task initially suggested, expand the investigation rather than artificially limiting scope.
- Keep final communication concise even when the underlying investigation is deep. Report what changed, what was actually verified, and any exact remaining blocker or uncertainty.
- For Koharu behavior changes, trace the affected workflow across scene ownership, application commands, generated bridges, desktop UI, retained rendering, persistence, and export as applicable. Resolve ambiguous requirements from repository evidence or ask before choosing behavior that would change the product contract.
- Static checks do not prove a desktop workflow or rendered result. Exercise the smallest real behavior that crosses the changed ownership boundary, and obtain final-window or exported-pixel evidence for rendering changes when the target environment is available. End-to-end runs remain opt-in under the Verification section.
- Before handoff, reread every user-facing explanation from start to finish and recheck requirements, dependent flows, edge cases, generated-source boundaries, and every validation result after correcting any issue found.

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
