'use client'

import { commands, type CanvasPagePreparation, type Point, type TransformFrame } from './protocol'

export type CanvasColor = [number, number, number, number]
export type CanvasStrokeKind = 'paint' | 'erase' | 'inpaint'
export type WorkspaceColor = [number, number, number]

export interface CanvasStroke {
  kind: CanvasStrokeKind
  layer: string | null
  point: Point
  diameter: number
  color?: CanvasColor
}

export interface Canvas {
  resize(width: number, height: number, dpr: number, background: WorkspaceColor): void
  setView(zoom: number, translation: [number, number]): void
  stageManifest(manifest: Uint8Array): { token: number; missing: string[] }
  installResource(resource: string, packet: Uint8Array): Promise<void>
  hasActiveManifest(manifest: Uint8Array): boolean
  activateFrame(token: number): boolean
  activatePage(page: string, expectedRevision: number): boolean
  cacheFrame(token: number, page: string): boolean
  clear(): void
  previewOpacity(element: string, opacity: number | null): void
  beginTransform(elements: TransformFrame[]): void
  updateTransform(elements: TransformFrame[]): void
  finishTransform(): void
  cancelTransform(): void
  beginStroke(stroke: CanvasStroke): void
  extendStroke(points: Point[]): void
  finishStroke(): void
  cancelStroke(): void
  sampleColor(point: Point): Promise<CanvasColor>
  dispose(): void
}

interface CanvasTransformFrame {
  element: string
  frame: {
    x: number
    y: number
    width: number
    height: number
    angleDegrees: number
  }
}

interface CanvasHandle {
  resize(width: number, height: number, dpr: number, background: Uint8Array): void
  setView(zoom: number, translationX: number, translationY: number): void
  stageManifest(manifest: Uint8Array): { token: number; missing: string[] }
  installResource(resource: string, packet: Uint8Array): Promise<void>
  activateFrame(token: number): boolean
  activatePage(page: string, expectedRevision: bigint): boolean
  cacheFrame(token: number): boolean
  clear(): void
  previewOpacity(element: string, opacity: number | null): void
  beginTransform(elements: CanvasTransformFrame[]): void
  updateTransform(sequence: number, elements: CanvasTransformFrame[]): void
  finishTransform(): unknown
  cancelTransform(): void
  beginStroke(
    kind: CanvasStrokeKind,
    layer: string | null,
    point: Point,
    diameter: number,
    color: Uint8Array,
  ): void
  extendStroke(points: Point[]): void
  finishStroke(): unknown
  cancelStroke(): void
  sampleColor(x: number, y: number): Promise<Uint8Array>
  setDeviceLostCallback(callback: ((reason: string) => void) | null): void
  dispose(): void
  free(): void
}

interface CanvasModule {
  default(): Promise<unknown>
  createCanvas(element: HTMLCanvasElement): Promise<CanvasHandle>
}

let modulePromise: Promise<CanvasModule> | null = null
let activeCanvas: Canvas | null = null
let prefetchGeneration = 0
let nativePrefetchTail: Promise<void> = Promise.resolve()

type CanvasStagePriority = 'foreground' | 'background'

interface CanvasStageQueue {
  running: boolean
  foreground: Array<() => Promise<void>>
  background: Array<() => Promise<void>>
}

const canvasStageQueues = new WeakMap<Canvas, CanvasStageQueue>()

export async function createCanvas(
  element: HTMLCanvasElement,
  onDeviceLost: (reason: string) => void,
): Promise<Canvas> {
  const module = await loadCanvasModule()
  const canvas = await module.createCanvas(element)
  let transformSequence = 0
  let disposed = false
  let stagedManifest: { token: number; bytes: Uint8Array } | null = null
  let activeManifest: Uint8Array | null = null
  const cachedManifests = new Map<string, Uint8Array>()
  canvas.setDeviceLostCallback(onDeviceLost)

  return {
    resize: (width, height, dpr, background) =>
      canvas.resize(width, height, dpr, new Uint8Array([...background, 255])),
    setView: (zoom, translation) => canvas.setView(zoom, translation[0], translation[1]),
    stageManifest: (manifest) => {
      const staged = canvas.stageManifest(manifest)
      stagedManifest = { token: staged.token, bytes: manifest.slice() }
      return staged
    },
    installResource: (resource, packet) => canvas.installResource(resource, packet),
    hasActiveManifest: (manifest) =>
      activeManifest !== null && equalBytes(activeManifest, manifest),
    activateFrame: (token) => {
      const activated = canvas.activateFrame(token)
      if (activated && stagedManifest?.token === token) {
        activeManifest = stagedManifest.bytes
        stagedManifest = null
      }
      return activated
    },
    activatePage: (page, expectedRevision) => {
      const activated = canvas.activatePage(page, BigInt(expectedRevision))
      if (activated) activeManifest = cachedManifests.get(page) ?? null
      return activated
    },
    cacheFrame: (token, page) => {
      const cached = canvas.cacheFrame(token)
      if (stagedManifest?.token === token) {
        if (cached) cachedManifests.set(page, stagedManifest.bytes)
        stagedManifest = null
      }
      return cached
    },
    clear: () => {
      canvas.clear()
      stagedManifest = null
      activeManifest = null
      cachedManifests.clear()
    },
    previewOpacity: (element, opacity) => canvas.previewOpacity(element, opacity),
    beginTransform: (elements) => {
      transformSequence = 0
      canvas.beginTransform(canvasTransformFrames(elements))
    },
    updateTransform: (elements) =>
      canvas.updateTransform(++transformSequence, canvasTransformFrames(elements)),
    finishTransform: () => void canvas.finishTransform(),
    cancelTransform: () => canvas.cancelTransform(),
    beginStroke: (stroke) =>
      canvas.beginStroke(
        stroke.kind,
        stroke.layer,
        stroke.point,
        stroke.diameter,
        new Uint8Array(stroke.color ?? [0, 0, 0, 0]),
      ),
    extendStroke: (points) => canvas.extendStroke(points),
    finishStroke: () => void canvas.finishStroke(),
    cancelStroke: () => canvas.cancelStroke(),
    sampleColor: async (point) => {
      const color = await canvas.sampleColor(point.x, point.y)
      if (color.length !== 4) throw new Error('The WebGPU canvas returned an invalid color.')
      return [color[0], color[1], color[2], color[3]]
    },
    dispose: () => {
      if (disposed) return
      disposed = true
      try {
        canvas.setDeviceLostCallback(null)
      } finally {
        try {
          canvas.dispose()
        } finally {
          canvas.free()
        }
      }
    },
  }
}

function loadCanvasModule(): Promise<CanvasModule> {
  // @ts-ignore -- wasm-pack output is created by bridge build tasks and absent in clean checkouts.
  modulePromise ??= import('./wasm/koharu_canvas.js')
    .then(async (module) => {
      const canvas = module as CanvasModule
      await canvas.default()
      return canvas
    })
    .catch((error: unknown) => {
      modulePromise = null
      throw error
    })
  return modulePromise
}

export async function fetchCanvasManifest(generation: number): Promise<Uint8Array> {
  return canvasBytesView(await commands.getCanvasManifest(generation))
}

export async function fetchCanvasResource(
  generation: number,
  resource: string,
): Promise<Uint8Array> {
  return canvasBytesView(await commands.getCanvasResource(generation, resource))
}

export async function loadCanvasFrame(
  canvas: Canvas,
  generation: number,
  current: () => boolean,
): Promise<boolean> {
  const manifest = await fetchCanvasManifest(generation)
  if (!current()) return false
  if (canvas.hasActiveManifest(manifest)) return true
  const activated = await enqueueCanvasStage(canvas, 'foreground', async () => {
    if (!current()) return false
    if (canvas.hasActiveManifest(manifest)) return true
    return installCanvasManifest(
      canvas,
      manifest,
      current,
      (resource) => fetchCanvasResource(generation, resource),
      (token) => canvas.activateFrame(token),
    )
  })
  if (!current()) return false
  if (!activated) throw new Error('The prepared canvas frame could not be activated.')
  return true
}

// Specta exposes CanvasBytes as number[], while its IpcResponse sends an ArrayBuffer at runtime.
function canvasBytesView(bytes: number[] | ArrayBuffer): Uint8Array {
  return new Uint8Array(bytes)
}

function equalBytes(left: Uint8Array, right: Uint8Array): boolean {
  return (
    left.byteLength === right.byteLength && left.every((value, index) => value === right[index])
  )
}

export function activateCanvas(canvas: Canvas): () => void {
  prefetchGeneration++
  activeCanvas = canvas
  return () => {
    if (activeCanvas === canvas) {
      prefetchGeneration++
      activeCanvas = null
    }
  }
}

export function previewCanvasOpacity(element: string, opacity: number | null): void {
  activeCanvas?.previewOpacity(element, opacity)
}

export function showCanvasPage(page: string, expectedRevision: number | null): boolean {
  prefetchGeneration++
  return expectedRevision === null
    ? false
    : (activeCanvas?.activatePage(page, expectedRevision) ?? false)
}

export function cancelCanvasPrefetch(): void {
  prefetchGeneration++
}

export function prefetchCanvasPages(pages: string[]): Promise<CanvasPagePreparation[]> {
  const canvas = activeCanvas
  if (!canvas || pages.length === 0) return Promise.resolve([])
  const generation = ++prefetchGeneration
  const current = () => activeCanvas === canvas && prefetchGeneration === generation
  return enqueueNativePrefetch(async () => {
    const preparedPages: CanvasPagePreparation[] = []
    if (!current()) return preparedPages
    for (const page of pages) {
      const prepared = await commands.prepareCanvasPage(page)
      if (!current() || prepared === null) return preparedPages
      const revision = prepared.revision
      const manifest = canvasBytesView(await commands.getCanvasPageManifest(page, revision))
      if (!current()) return preparedPages
      const cached = await enqueueCanvasStage(canvas, 'background', () =>
        installCanvasManifest(
          canvas,
          manifest,
          current,
          async (resource) =>
            canvasBytesView(await commands.getCanvasPageResource(page, revision, resource)),
          (token) => canvas.cacheFrame(token, page),
        ),
      )
      if (!current()) return preparedPages
      if (cached) preparedPages.push(prepared)
    }
    return preparedPages
  })
}

function enqueueNativePrefetch<Result>(operation: () => Promise<Result>): Promise<Result> {
  // The desktop renderer serializes page preparation internally. Keep speculative
  // requests serialized here too, so pointer movement can leave at most the
  // active request plus the newest still-current intent instead of a native queue.
  const result = nativePrefetchTail.then(operation, operation)
  nativePrefetchTail = result.then(
    () => undefined,
    () => undefined,
  )
  return result
}

async function installCanvasManifest(
  canvas: Canvas,
  manifest: Uint8Array,
  current: () => boolean,
  fetchResource: (resource: string) => Promise<Uint8Array>,
  commit: (token: number) => boolean,
): Promise<boolean> {
  if (!current()) return false
  const staged = canvas.stageManifest(manifest)
  for (let offset = 0; offset < staged.missing.length; offset += 4) {
    const resources = staged.missing.slice(offset, offset + 4)
    const packets = await Promise.all(
      resources.map(async (resource) => [resource, await fetchResource(resource)] as const),
    )
    if (!current()) return false
    await Promise.all(packets.map(([resource, packet]) => canvas.installResource(resource, packet)))
  }
  if (!current()) return false
  return commit(staged.token)
}

function enqueueCanvasStage<Result>(
  canvas: Canvas,
  priority: CanvasStagePriority,
  operation: () => Promise<Result>,
): Promise<Result> {
  const existing = canvasStageQueues.get(canvas)
  const queue: CanvasStageQueue = existing ?? { running: false, foreground: [], background: [] }
  if (!existing) canvasStageQueues.set(canvas, queue)
  return new Promise<Result>((resolve, reject) => {
    queue[priority].push(async () => {
      try {
        resolve(await operation())
      } catch (error) {
        reject(error)
      }
    })
    void drainCanvasStageQueue(canvas, queue)
  })
}

async function drainCanvasStageQueue(canvas: Canvas, queue: CanvasStageQueue): Promise<void> {
  if (queue.running) return
  queue.running = true
  try {
    for (;;) {
      // WebCanvas owns one staged manifest. An operation already touching that
      // slot must finish, but a selected page takes the next turn instead of
      // waiting behind speculative cache warming.
      const operation = queue.foreground.shift() ?? queue.background.shift()
      if (!operation) break
      await operation()
    }
  } finally {
    queue.running = false
    if (queue.foreground.length > 0 || queue.background.length > 0) {
      void drainCanvasStageQueue(canvas, queue)
    } else if (canvasStageQueues.get(canvas) === queue) {
      canvasStageQueues.delete(canvas)
    }
  }
}

export function workspaceColor(): WorkspaceColor {
  const raw = window
    .getComputedStyle(document.documentElement)
    .getPropertyValue('--workspace-background-rgb')
  const values = raw.trim().split(/\s+/).map(Number)
  if (values.length !== 3 || values.some((value) => !Number.isFinite(value))) {
    return [183, 180, 174]
  }
  return values.map((value) => Math.min(255, Math.max(0, Math.round(value)))) as WorkspaceColor
}

function canvasTransformFrames(elements: TransformFrame[]): CanvasTransformFrame[] {
  return elements.map(({ element, frame }) => ({
    element,
    frame: {
      x: frame.x,
      y: frame.y,
      width: frame.width,
      height: frame.height,
      angleDegrees: frame.angle_degrees,
    },
  }))
}
