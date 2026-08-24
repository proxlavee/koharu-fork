# Koharu Typesetting Investigation Checkpoint

Last updated: 2026-08-25

This file records material evidence and decisions for the active root-cause
typesetting investigation. It is a continuation aid, not a substitute for
reading `AGENTS.md`, the affected implementation, or the current diff.

## Operating constraints

- Follow `AGENTS.md` and preserve the existing dirty worktree. Several edited
  files contain user-owned or earlier in-progress changes.
- The user owns the running development server and watcher. Do not run build,
  check, test, dev, start, serve, watch, preview, or restart commands. Source
  edits may trigger the user's watcher automatically; that is not permission to
  start or replace it.
- Use Git Bash or WSL for command-line work and the live CEF CDP endpoint at
  `http://127.0.0.1:4000` for desktop inspection. Use final rendered pixels as
  evidence, not DOM state alone.
- Always compare questionable output against all three sources: the current
  `test123` render, the full-resolution raw source extracted from `test123`, and
  the manually typeset `test123-good` reference. Inspect pages individually.
- Page 1 may be observed but must not be used as a tuning target or ground
  truth; its original composition is unusually difficult.
- Existing project translations do not change when renderer code or translator
  prompting changes. Renderer changes are visible after retained rendering is
  refreshed; producer-prompt changes require rerunning translation.
- The current chapter contains 43 pages: labels `1.webp` through `42.webp` and
  `45.webp`. Labels 43 and 44 are absent.

## Product requirements accumulated from user feedback

- Fix the algorithm globally. Do not hardcode page, image, string, or
  coordinate-specific behavior.
- Prefer intact words when they fit at a visually acceptable size. A modest
  reduction of roughly a few pixels is preferable to an unnecessary
  discretionary hyphen. Do not eliminate hyphenation categorically: a valid
  language-aware split is acceptable when keeping the word intact would make
  the text unreasonably small or impossible to fit.
- Readability outranks avoiding every hyphen. Tiny text is a worse failure than
  an occasional necessary split.
- Preserve semantic hyphens such as stutters (`B-bugün`, `S-Sakin`) and
  honorifics (`Haruto-kun`, `Aoki-san`). Do not repair stored translation or OCR
  artifacts by stripping punctuation during rendering.
- Balloon text should be optically centered, with small downward optical
  adjustments where the painted block otherwise appears high. It must remain
  inside the intended balloon/flow cell and must not jump into an adjacent
  bubble or caption.
- Free text and SFX may be horizontal even when the Japanese source is vertical.
  They should use available surrounding space without colliding with other
  semantic regions. Page 36's `FUU...` / `HAA...` are the main calibration
  example: the enlarged result is acceptable, though slightly smaller would
  also be acceptable.
- Text placed over artwork needs an appropriate white halo/background for
  readability and to cover cleaning remnants. It must not grow until it merges
  with source art or another translated region.
- Compare against the raw page to understand the source text footprint and
  available space, and against `test123-good` to calibrate visually reasonable
  size, density, line count, centering, and halo behavior.
- Cross-project review accepts balloon placement, sizing, centering, containment,
  and multi-text ownership as fixed. Remaining avoidable discretionary word
  splits are a separate text-fitting invariant, not an unresolved balloon
  geometry failure.
- For text over artwork, layouts that cannot be moved or enlarged because the
  source regions are intrinsically crowded are acceptable. The remaining
  user-visible defect in this class is missing or weak opposing halo/stroke
  treatment.
- The user accepts the current work as an approximately 80% interim milestone
  once that halo/stroke defect is fixed and validated. At that boundary,
  preserve this checkpoint and reusable inspection scripts, record every known
  remaining split case, commit the cohesive typesetting work, and clean only
  task-owned disposable Git artifacts. Never discard unrelated user work to
  manufacture a clean status.

## Traced ownership and rendering path

The affected path has been traced across the following boundaries:

1. Detection creates text, balloon, panel, and other regions, then relates a
   generated text layer with `FlowsIn` when it belongs to a balloon or `FitsTo`
   when it remains tied to a text region.
2. OCR and translation populate the text content. Translation text is persisted
   in the project; renderer changes do not rewrite it.
3. `koharu-renderer` resolves balloon flows from retained scene relations. It
   uses the full balloon geometry plus an optional per-text flow cell and the
   OCR source anchor when more than one text belongs to a balloon.
4. `TextLayout` shapes real glyphs, derives painted ink bands and outline air,
   builds line-specific contour profiles, composes the paragraph, searches font
   size and block position, and returns retained vector glyph runs.
5. Generated free text uses page/panel/obstacle geometry to generate physical
   footprint candidates. The same-direction path now participates in measured
   candidate selection instead of bypassing it.
6. The retained scene is prepared for the `koharu-canvas` WASM/WebGPU path. The
   same prepared data drives desktop presentation and export, so live final
   canvas pixels and decoded retained manifests are the relevant evidence.

## Confirmed root causes

### 1. OCR text boxes are not sufficient dialogue containers

The source OCR box often describes only the Japanese glyph footprint and is too
thin for a horizontal translation. Dialogue must flow in the detected speech
balloon. This is the central concern of upstream issue #119. Koharu's current
path now uses `FlowsIn` balloon geometry, and multiple text regions in one
balloon receive separate flow cells so they cannot collapse into one block.

### 2. Balloon width is line- and position-dependent

A balloon is not a rectangle. The usable interval changes for every baseline,
and moving a block vertically can change whether the same font size and line
breaks fit. Fit is therefore non-monotonic with font size. A binary search that
assumes smaller always fits can miss larger valid layouts or preserve a poor
split.

The current renderer addresses this with full visible-pixel font-size search,
per-line contour profiles, a search over feasible block origins, and refinement
near fit/miss transitions.

### 3. Greedy line breaking cannot optimize a shaped balloon paragraph

Locally choosing each line can create an unnecessary later split. The current
comic layout uses dynamic programming across the entire paragraph with the
actual width of each candidate line. It penalizes slack, overflow, natural
pause quality, and discretionary hyphens. This is a custom paragraph-composer
formulation analogous to the useful parts of Knuth-Plass/Adobe Paragraph
Composer, without replacing Koharu's shaping and retained-rendering stack.

### 4. Hyphen legality and layout preference are separate problems

ICU4X supplies legal Unicode line-break opportunities. `hypher` supplies
language-aware discretionary points, including `Lang::Turkish`, with
language-specific prefix/suffix bounds. Hyphenation is configured as
last-resort. There is no silent English fallback for an unknown translation
language.

Legal Turkish hyphenation alone does not produce the desired visual result. The
remaining problem is choosing among fitted layouts at different font sizes:
the largest valid layout can contain a legal but unattractive split even when a
somewhat smaller clean layout is better.

### 5. Font size, line breaks, centering, and source locality are coupled

Choosing the largest fitting size first and centering afterward is insufficient.
The current candidate selector considers painted center error, discretionary
hyphen count, font-size loss, and whether the block remains local to its OCR
anchor. Source locality is important for touching narration boxes and for
multiple captions in one concave region.

### 6. Generated free text needs a physical footprint, not just a text box

For free text/SFX, source and target writing direction may differ. The renderer
now evaluates measured source, intermediate area-preserving aspect, and logical
transpose candidates against panel/page containers and other detected regions.
Same-direction free text now also receives an original-footprint candidate so
it participates in outline-aware fitting and quality selection.

### 7. Some apparent layout failures are upstream stored-text failures

Page 19's malformed `Be-klemekteyimdir.` is already present in the stored
translation and stems from OCR/translation text, not a discretionary renderer
break. Rendering must not remove the hyphen because other persisted hyphens are
semantic. The translator prompt has been tightened for future translation runs,
but existing projects need the translation stage rerun to receive that change.

### 8. Duplicate/nested detections can merge or overlap generated text graphs

Detection non-maximum suppression previously missed strongly nested text
detections when ordinary IoU was low. Detection now uses containment and mask
containment for text regions in addition to IoU. Reprocessing also replaces the
complete detector-owned text graph while failing closed if user-authored text,
geometry, visibility, typography, or related state would be overwritten.

### 9. Generated free-text halo fallback was one-sided

Fresh retained-scene decoding after a complete rerun of both projects proved
that the questioned artwork text has no persisted stroke. Koharu's source-style
estimator deliberately rejects a candidate outline when its color matches the
sampled background, although that is the ordinary manga construction: black
text with a white halo on light art or white text with a black halo on dark art.
The renderer already supplied a white fallback halo for dark generated free
text, but supplied nothing for light generated free text. That asymmetry is why
some apparently similar captions remained unprotected.

The corrected presentation invariant is symmetric: machine-generated free text
without an explicit measured stroke receives whichever opaque black or white
halo opposes its foreground luminance, and layout reserves the same halo
clearance. Dialogue and user-authored typography are excluded. Because desktop,
PNG, and PSD consume the same retained renderer result, the correction crosses
all final presentation paths without mutating authored scene state.

The fallback and its layout clearance must use the same definition of an
effective explicit stroke. A retained `stroke_width` of `Some(0.0)` is not a
painted outline: presentation therefore applies the generated fallback and
placement must reserve that fallback width as well. Treating zero as explicit
only in the clearance path could otherwise make the final halo clip or collide
even though the renderer correctly painted it.

## Material implementation already present in the dirty worktree

- `crates/koharu-renderer/src/layout.rs`
  - ICU4X segmentation plus last-resort language-aware hyphenation integration.
  - Hard-fit and least-bad line-breaking passes so overflow cannot win merely
    because it has finite aesthetic cost.
  - Dynamic programming over line-specific contour profiles.
  - Search over line count, block origin, non-monotonic font-size candidates,
    and refined fit boundaries.
  - Painted glyph-ink measurement, outline-aware wall air, area-weighted balloon
    centering, OCR-anchor locality, and one coherent visual axis per paragraph.
  - Cross-font-size quality selection that compares discretionary split count,
    centering, locality, and font-size loss.
- `crates/koharu-renderer/src/text_renderer.rs`
  - Uses balloon and flow-cell constraints and mirrors generated free-text
    candidate quality selection.
- `crates/koharu-renderer/src/free_text.rs`
  - New script-aware physical footprint candidates with obstacle/container
    checks and area-preserving aspect exploration.
- `crates/koharu-renderer/src/renderer.rs`
  - Resolves retained balloon flows, source anchors, flow cells, and free-text
    candidates for every generated paragraph path.
  - Resolves a symmetric black-or-white fallback halo for generated free text
    whose retained typography has no measured stroke, and includes that halo in
    physical placement clearance. Explicit/inferred strokes and user-authored
    no-stroke intent remain authoritative. Zero-width retained strokes use the
    fallback consistently in both painting and clearance.
- `crates/koharu-pipeline/src/stages/detection.rs`
  - Nested text NMS and provenance-safe replacement of complete generated text
    graphs on rerun.
- `crates/koharu-translator/src/prompt.rs`
  - Translator output contract now requests semantic translation without manual
    wraps, soft hyphens, or layout-only hyphens while preserving meaningful
    stutters, honorifics, and established forms.
- `crates/koharu-app/src/app.rs`
  - Debug-only app-owned CDP navigation watchdog for the black-window startup
    failure. It respects a completed app-origin load and does not take ownership
    of the user's dev server.

The temporary `koharu_typesetting_probe` tracing and one-off `TAMAMEN` font
measurement sample were removed after the final live audit. Reusable CDP page
capture, retained-manifest inspection, raw-source extraction, and typography
summary scripts remain available under the ignored `temp/` workspace.

## Investigated research and disposition

- Upstream issue #119 (find/fill the speech-bubble edge): its central ownership
  model is implemented through balloon `FlowsIn` geometry and line-specific
  contour profiles. Exact contour stability remains under review, but dialogue
  is no longer limited to the OCR text bbox.
- Upstream issue #117 (Knuth-Liang hyphenation): language-aware discretionary
  points are implemented. Its old 50% fill-ratio idea was tested and rejected;
  it made page 3 and several page 21 captions much too small.
- Knuth-Plass / Adobe Paragraph Composer: the useful global-breakpoint and
  weighted-cost ideas are implemented as a custom dynamic program compatible
  with Koharu's shaped glyph metrics and variable balloon widths.
- Parley's advanced line-specific offset/max-advance API: inspected as design
  confirmation, but not adopted. Koharu already needs retained glyph output,
  its own source-locality objective, and shape-specific candidate search.
- `text_layout` and `oxitext-layout`: not adopted or benchmarked. Replacing the
  current layout stack has not been justified because the current code already
  has ICU4X breaks, bidi/shaping integration, Turkish hyphenation, and global
  variable-width DP. They remain references if the current formulation proves
  structurally insufficient.
- Geometry/composition separation: implemented. Balloon/flow geometry produces
  profiles; the composer optimizes shaped text against them.
- Morphological contour closing, polygon buffering, signed/distance transforms,
  and medial-region construction: not implemented. They remain valid candidates
  only if raw/current/gold evidence proves that detector noise, rather than
  font-size candidate selection, is the remaining constraint.
- Largest-inscribed rectangle: not used as the primary layout because it wastes
  useful irregular capacity. A rectangular layout is used only as a preflight
  search bound/fallback aid.
- `manga-image-translator` / `manga-translator-ui`: inspected for evidence of
  balloon-mask layout, optional hyphen disabling, adjustable spacing, and AI
  line-break retries. These validate the problem formulation but no code has
  been copied or dependency introduced.
- A strict no-hyphen pass exists at each fixed font size, but the second research
  note exposed a distinct missing proof: the renderer does not yet search the
  complete acceptable font-size/position/aspect candidate space without
  hyphenation before comparing that frontier with larger hyphenated layouts.
  This cross-size whole-word-first formulation is genuinely new relative to the
  current bounded quality window and is an active deterministic hypothesis.
- Role-specific objectives are a useful extension, not a request for unrelated
  special cases. Dialogue, narration, free text, and SFX already follow partly
  separate geometry paths; verify which role/classification survives to final
  candidate selection before deciding whether their aesthetic weights should
  differ while retaining shared safety invariants.
- AI/LLM/VLM line breaking has not been attempted. The least risky future form
  would be deterministic generation and validation of a small diverse candidate
  set followed by optional VLM reranking only for low-confidence regions. Model
  newline hints or measured layout suggestions are secondary possibilities.
  Full generative page rendering is currently rejected because it cannot
  reliably preserve exact Turkish text, punctuation, artwork, retained editing,
  determinism, and export semantics. All model assistance remains deferred until
  geometry and deterministic candidate selection reach a demonstrated ceiling.

## Rejected approaches and why

- Treating a 50% source-relative fill ratio as proof of readability: rejected
  after it shrank clean captions on pages 3 and 21 far below the manual visual
  baseline.
- Removing or suppressing rendered hyphens after layout: rejected because it
  would hide overflow and damage semantic stutters/honorifics.
- Always keeping the largest fitting font: rejected because it favors legal but
  avoidable splits.
- Always choosing the smallest clean font: rejected because small text can be
  less readable than a necessary language-aware split.
- Using page-specific size/coordinate overrides: prohibited by the product goal
  and unsupported by adjacent-page evidence.
- Judging from a current screenshot alone: rejected. Raw geometry and the manual
  reference have repeatedly changed the correct classification.
- The old `temp/p36-source-raw-v2/36-source.webp` artifact: rejected because it
  was only a 91x128 thumbnail. Fresh retained-manifest extraction produced the
  authoritative 1280x1808 source.

## Fresh pixel evidence

The complete post-pipeline `test123` baseline is in
`temp/fresh-test123-postpipeline-v2/`. It contains all 43 individual
final-canvas PNGs plus retained `audit.json` evidence.

The independent `test` project is now a second regression dataset. It contains
56 pages. After the user's complete fresh pipeline run, 55 pages contain
translated retained text; page `050.jpg` contains zero text layers. All 56
post-pipeline pages and retained data are captured in
`temp/fresh-test-postpipeline-v2/`. Full-resolution raw sources for pages 1
through 14 remain in `temp/fresh-test-raw-v1/`; extend that extraction only for
newly questioned pages. Raw/current inspection through page 14 confirms the
source frequently uses opposing black/white outlines over artwork. Retained
decoding proves every questioned layer currently stores a null stroke, and the
renderer inspection proves that only dark foregrounds received a fallback.

Across `test123`, `test123-good`, raw sources, and the translated portion of
`test`, balloon geometry is accepted as fixed: translated dialogue remains in
the intended container, uses the available balloon body, and is optically
centered at an acceptable scale. The split cases listed below remain evidence
for cross-size whole-word candidate selection only.

The symmetric halo correction was then exercised on a new post-edit retained
frame. All 56 pages of `test` and all 43 pages of `test123` were captured from
the running desktop CEF canvas and inspected individually at revision 234 and
607 respectively. Both capture manifests report zero new console messages and
zero page errors. Light generated text over dark artwork now has a black halo,
and dark generated text over artwork has a white halo, while dialogue and
explicitly styled text remain unchanged. Representative retained-manifest
decoding on `test` pages 6, 7, 23, and 46 independently shows both foreground
and opposing halo colors in the final glyph/path data. The same prepared
renderer frame is consumed by desktop presentation, PNG export, and PSD export;
no scene typography was mutated to obtain the fallback.

No halo or accepted balloon-layout regression was found in either complete
page set. Important visual controls include `test` pages 6, 7, 8, 14, 23, 29,
36, 37, 39, 44, 46, 49, 51, 52, and 56, plus `test123` pages 2, 3, 5-9, 18,
20, 21, 23, 36, 39, and 45. Page `050.jpg` in `test` correctly remains the
only page with zero retained translated text.

After the zero-width clearance audit correction, `test` pages `006.jpg` and
`023.jpg` were recaptured individually from the freshly rerun project in
`temp/halo-edge-final-v1/`. Page 6 confirms light generated text retains its
black halo; page 23 confirms dark generated text retains its white halo; both
retain edge clearance. The final capture recorded no new console messages and
no page errors, and the helper restored the previously closed-project state.

Fresh three-way controls already inspected:

- Page 2: current blocks are centered in the full balloon. Character occlusion
  created a false impression of top bias. Pass.
- Page 3: the long caption is no longer undersized and is comparable to or
  larger than the manual reference. The tiny inset split is necessary for its
  very narrow region. Pass.
- Pages 5 and 6: visually good; only very small downward optical adjustments
  might improve some blocks.
- Page 8: generally good; only small optical adjustments might improve some
  blocks. Page 7 remains visually acceptable in several respects, but its
  long-word split is no longer classified as necessarily constrained. Telemetry
  reportedly allows the large setting only with multiple splits and a fully
  clean setting only near half that size. Re-extract its raw/manual controls and
  compare the actual per-baseline profile with the visible balloon before
  deciding whether the contour/interior or candidate selector is responsible.
- Page 9 after a fresh pipeline rerun: the previously merged upper-right texts
  are separate and readable. Pass.
- Page 18: the small secondary thought is intentional hierarchy and agrees with
  the raw/manual composition. Pass.
- Page 20: free captions wrap downward and retain a readable halo, matching the
  raw/manual composition. Pass.
- Page 21: the center caption is compact but matches the manual reference; the
  narrow inset split is necessary. Pass.
- Page 22: `2 MİLYON!!` is clean and readable. Pass.
- Page 23: the very small aside is intentional hierarchy and comparable to the
  manual reference. Pass.
- Page 30: confirmed failure. `Teyze de tamamen gaza geldi ha♡` renders at 48
  with `TA-MAMEN`; `TAMAMEN` measures about 244 px at 48 and 224 px at 44, so a
  2-4 px reduction cannot solve it. The manual reference uses visibly smaller,
  clean, still-readable text. This is a cross-size candidate-selection defect.
- Page 32: confirmed concern. The lower balloon and bottom-left caption preserve
  oversized text by splitting `SIKIŞTIRABİLECEKSİN` and
  `SÜRTÜNEBİLİYOR`; raw and manual layouts show that smaller readable
  settings are legitimate candidates.
- Page 33: confirmed failure. `Aa♡ Harikaa♡` is rendered at 31 over three
  lines with an intra-word split. The manual reference uses smaller clean text
  in the same balloon.
- Page 34: confirmed failure. `Ne yapacağım!` is rendered at 21 over three
  lines with an intra-word split. The manual reference uses smaller clean text.
- Page 36: the enlarged vertical-source SFX uses the available space and is
  acceptable; the user said it could be slightly smaller but does not require a
  special correction. Pass.
- Page 38: confirmed failure. The manual reference uses substantially smaller,
  clean text in the narrow right balloons. Koharu preserves larger settings and
  splits `ÜRETİYORUM` and `BOŞALACAĞIM`.
- Page 39: pass for layout. The raw page itself contains very large haloed SFX
  in the questioned locations and the manual edition preserves their hierarchy.
  Questionable phonetic wording is OCR/translation content, not a geometry or
  scale failure.
- Page 40: confirmed failure. The raw page has a tall narration box and the
  manual reference uses small clean horizontal text with substantial white
  space. Koharu expands `HARİKA` to 39 px and splits it.
- Page 45: pass for layout from raw evidence. The small blue `Bekar Anne` text
  is not a duplicate; it corresponds to a separate small blue Japanese subtitle
  beside the larger cover title and retains its white outline/source hierarchy.
  `test123-good` has no page labeled `45.webp`, so no manual equivalent exists.

## Deferred line-break inventory at the accepted interim boundary

The halo and balloon fixes are validated, but the complete whole-word-first
candidate frontier described below is not implemented. These are the known
renderer-inserted discretionary breaks observed during the final two-project
audit; each corresponding persisted translation contains the intact word:

- `test123`: page 4 `BAKA-LIM`; page 7 `BOŞALI-YORUM`; page 15
  `TEŞEK-KÜRLER`; page 28 `UNUTMAYA-CAKSIN`; page 30 `TA-MAMEN`; page 32
  `SİKİŞEBİLE-CEKSİN` and `SÜRTÜNE-BİLİYOR`; page 33 `HARİ-KAA`; page 34
  `YAPA-CAĞIM`; page 38 `ÜRETİ-YORUM` and `BOŞALA-CAĞIM`; page 40
  `HARİ-KA`.
- `test`: page 3 `PE-KALA`; page 9 `SEVİN-DİM`; page 11 `BAKA-LIM`; page
  12 `BAŞLI-YORUM` and `EMİYOR-SUN`; page 13 `MAC-HETTE`; page 26
  `ABART-MAYIN` and `SÜRMESİN-DEN`; page 27 `DESTEK-LEYECEK`; page 28
  `DU-RUMDA`; page 30 `SAOS-HI` and `CA-LIBA`; page 32
  `SAĞLAYA-MAYACAĞIMI` and `İSTİYO-RUM`; page 33 `TEŞEK-KÜRLER` and
  `CALI-BA`; page 34 `ÖYLEY-SE`; page 40 `TEŞEK-KÜRLER` and `TA-MAMDIR`.

Do not conflate this list with persisted punctuation. `test` page 2
`Ada-ruto`, page 4 `Ten-seks`, page 19 `Be-klemekteyimdir`, and page 47
`Ka... Caliba...` already contain their hyphen/stutter in translated scene
text. The translator contract should prevent new layout-only punctuation, but
existing stored content must be corrected at translation ownership rather than
silently rewritten during rendering.

## Current algorithmic gap and next investigation

`fragmentation_quality_font_reduction` currently expands the normal 2-4 px
quality window only when split density crosses a word-count heuristic. A simple
change from three to six words per hyphen was considered after page 30, but the
new three-way evidence disproves that as a complete fix: pages 33 and 34 already
qualify for the expanded path and still split. Page 32 also shows both a short
high-fragmentation case and a longer one-split case.

Therefore do not merely tune `COMIC_HIGH_FRAGMENTATION_WORDS_PER_HYPHEN`. Trace
why clean candidates are absent or rejected for page 7 and pages 30/32-34.
Candidate causes to distinguish with retained metrics/probes are:

1. the clean word still does not fit before the proportional search floor;
2. the no-hyphen paragraph cannot use enough lines because legal segments bound
   the line-count search;
3. the clean candidate exists but fails source-locality/center eligibility;
4. exact contour/flow-cell profiles are narrower than the visually safe region;
5. the selector's discrete split-count-first comparison needs a continuous
   readability-versus-fragmentation objective over a broader candidate set.

The second research note makes the geometry/selection boundary explicit. Record
the physical balloon contour, optional flow-cell contour, sampled glyph-ink band,
line interval, common paragraph axis, final profile width, actual word advance,
and rejection reason for each candidate. If those intervals visibly exclude a
safe part of the raw balloon, investigate a derived typesetting body through
small-spike removal, contour regularization, buffered/offset polygons, or a
distance-clearance field. Do not introduce that machinery if the measured
profile already agrees with the raw balloon.

If geometry is sound, the preferred general direction is a two-frontier search:
generate all materially distinct whole-word layouts over a defensible readable
range first, then generate language-aware hyphenated fallbacks. Retain the
Pareto frontier for font size, discretionary fragmentation, centering, locality,
boundary air, and visual density, and select with an explicit quality objective.
This must be validated against raw/current/manual pixels and must not recreate
the rejected 50% fill-floor behavior or choose unreadably small clean text.

## Validation status and pending work

- No build, check, test, or dev command has been run under the current user
  constraint. Existing focused tests were edited but remain unexecuted.
- The user's existing watcher compiled and loaded the edited renderer before
  the post-halo captures; the live endpoint remained responsive throughout.
  This proves the actual debug desktop path compiled and rendered, but it is not
  a substitute for the unexecuted focused unit tests.
- Post-halo final-window validation is complete across all 99 pages in the two
  projects, with retained-vector inspection on representative opposing-color
  controls and no capture-time runtime errors. The two opposing-color controls
  were rechecked after the final zero-width clearance correction.
- Rust formatting and `git diff --check` were run after earlier implementation
  phases; repeat the allowed static checks after final cleanup.
- At the user's accepted approximately 80% milestone, commit the coherent
  balloon/free-text/halo work and preserve the reusable diagnostics. The active
  follow-up is the whole-word-first cross-size frontier for the inventory above;
  do not reopen accepted balloon geometry or source-constrained overlay
  positioning without contrary pixel evidence.
