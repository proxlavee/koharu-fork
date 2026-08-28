import { describe, expect, it, vi } from 'vitest'

import {
  activateCanvas,
  cancelCanvasPrefetch,
  loadCanvasFrame,
  prefetchCanvasPages,
  previewCanvasOpacity,
  showCanvasPage,
  type Canvas,
} from '@koharu/bridge/canvas'

const commands = vi.hoisted(() => ({
  getCanvasManifest: vi.fn(),
  getCanvasResource: vi.fn(),
  prepareCanvasPage: vi.fn(),
  getCanvasPageManifest: vi.fn(),
  getCanvasPageResource: vi.fn(),
}))

vi.mock('@koharu/bridge/protocol', () => ({ commands }))

describe('canvas runtime', () => {
  it('routes inspector previews only to the active canvas', () => {
    const previewOpacity = vi.fn()
    const canvas = { previewOpacity } as unknown as Canvas
    const deactivate = activateCanvas(canvas)

    previewCanvasOpacity('layer', 0.5)
    deactivate()
    previewCanvasOpacity('layer', null)

    expect(previewOpacity).toHaveBeenCalledOnce()
    expect(previewOpacity).toHaveBeenCalledWith('layer', 0.5)
  })

  it('shows a cached page immediately on the active canvas', () => {
    const activatePage = vi.fn(() => true)
    const canvas = { activatePage } as unknown as Canvas
    const deactivate = activateCanvas(canvas)

    expect(showCanvasPage('page', 4)).toBe(true)
    deactivate()

    expect(activatePage).toHaveBeenCalledWith('page', 4)
  })

  it('prepares and installs adjacent page resources before caching the frame', async () => {
    const order: string[] = []
    const prepared = {
      revision: 4,
      page: { id: 'page' },
    }
    commands.prepareCanvasPage.mockResolvedValue(prepared)
    commands.getCanvasPageManifest.mockResolvedValue(new ArrayBuffer(1))
    commands.getCanvasPageResource.mockResolvedValue(new ArrayBuffer(1))
    const canvas = {
      stageManifest: vi.fn(() => ({ token: 7, missing: ['resource'] })),
      installResource: vi.fn(async () => void order.push('resource')),
      cacheFrame: vi.fn(() => {
        order.push('frame')
        return true
      }),
    } as unknown as Canvas
    const deactivate = activateCanvas(canvas)

    const result = await prefetchCanvasPages(['page'])
    deactivate()

    expect(commands.prepareCanvasPage).toHaveBeenCalledWith('page')
    expect(commands.getCanvasPageManifest).toHaveBeenCalledWith('page', 4)
    expect(commands.getCanvasPageResource).toHaveBeenCalledWith('page', 4, 'resource')
    expect(canvas.cacheFrame).toHaveBeenCalledWith(7, 'page')
    expect(order).toEqual(['resource', 'frame'])
    expect(result).toEqual([prepared])
  })

  it('does not expose page data when the frame was rejected by the cache', async () => {
    commands.prepareCanvasPage.mockResolvedValue({ revision: 4, page: { id: 'page' } })
    commands.getCanvasPageManifest.mockResolvedValue(new ArrayBuffer(1))
    const canvas = {
      stageManifest: vi.fn(() => ({ token: 7, missing: [] })),
      cacheFrame: vi.fn(() => false),
    } as unknown as Canvas
    const deactivate = activateCanvas(canvas)

    const result = await prefetchCanvasPages(['page'])
    deactivate()

    expect(result).toEqual([])
  })

  it('coalesces queued native prefetches to the newest page intent', async () => {
    let releaseFirst: ((prepared: { revision: number; page: { id: string } }) => void) | undefined
    let markFirstStarted: (() => void) | undefined
    const firstStarted = new Promise<void>((resolve) => {
      markFirstStarted = resolve
    })
    commands.prepareCanvasPage.mockImplementation((page: string) =>
      page === 'first'
        ? new Promise<{ revision: number; page: { id: string } }>((resolve) => {
            releaseFirst = resolve
            markFirstStarted?.()
          })
        : Promise.resolve({ revision: 4, page: { id: page } }),
    )
    commands.getCanvasPageManifest.mockResolvedValue(new ArrayBuffer(1))
    const canvas = {
      stageManifest: vi.fn(() => ({ token: 7, missing: [] })),
      cacheFrame: vi.fn(() => true),
    } as unknown as Canvas
    const deactivate = activateCanvas(canvas)

    const first = prefetchCanvasPages(['first'])
    await firstStarted
    const middle = prefetchCanvasPages(['middle'])
    const latest = prefetchCanvasPages(['latest'])

    expect(commands.prepareCanvasPage).toHaveBeenCalledTimes(1)
    releaseFirst?.({ revision: 4, page: { id: 'first' } })
    await expect(first).resolves.toEqual([])
    await expect(middle).resolves.toEqual([])
    await expect(latest).resolves.toEqual([{ revision: 4, page: { id: 'latest' } }])
    deactivate()

    expect(commands.prepareCanvasPage.mock.calls.map(([page]) => page)).toEqual(['first', 'latest'])
  })

  it('drops queued native prefetches after foreground selection cancels them', async () => {
    let releaseFirst: ((prepared: { revision: number; page: { id: string } }) => void) | undefined
    let markFirstStarted: (() => void) | undefined
    const firstStarted = new Promise<void>((resolve) => {
      markFirstStarted = resolve
    })
    commands.prepareCanvasPage.mockImplementation(
      (page: string) =>
        new Promise<{ revision: number; page: { id: string } }>((resolve) => {
          if (page === 'first') {
            releaseFirst = resolve
            markFirstStarted?.()
          }
        }),
    )
    const canvas = {} as Canvas
    const deactivate = activateCanvas(canvas)

    const first = prefetchCanvasPages(['first'])
    await firstStarted
    const queued = prefetchCanvasPages(['queued'])
    cancelCanvasPrefetch()
    releaseFirst?.({ revision: 4, page: { id: 'first' } })

    await expect(first).resolves.toEqual([])
    await expect(queued).resolves.toEqual([])
    deactivate()

    expect(commands.prepareCanvasPage).toHaveBeenCalledTimes(1)
    expect(commands.prepareCanvasPage).toHaveBeenCalledWith('first')
  })

  it('serializes foreground activation against background prefetch staging', async () => {
    const order: string[] = []
    let releaseActiveResource: (() => void) | undefined
    let activeResourceStarted: (() => void) | undefined
    const activeStarted = new Promise<void>((resolve) => {
      activeResourceStarted = resolve
    })
    commands.getCanvasManifest.mockResolvedValue(new Uint8Array([1]).buffer)
    commands.getCanvasResource.mockImplementation(
      () =>
        new Promise<ArrayBuffer>((resolve) => {
          order.push('fetch:active')
          activeResourceStarted?.()
          releaseActiveResource = () => resolve(new Uint8Array([1]).buffer)
        }),
    )
    commands.prepareCanvasPage.mockResolvedValue({ revision: 4, page: { id: 'page' } })
    commands.getCanvasPageManifest.mockResolvedValue(new Uint8Array([2]).buffer)
    commands.getCanvasPageResource.mockImplementation(async () => {
      order.push('fetch:prefetch')
      return new Uint8Array([2]).buffer
    })
    let token = 0
    const canvas = {
      hasActiveManifest: vi.fn(() => false),
      stageManifest: vi.fn(() => {
        token += 1
        order.push(`stage:${token}`)
        return { token, missing: [token === 1 ? 'active' : 'prefetch'] }
      }),
      installResource: vi.fn(async (resource: string) => {
        order.push(`install:${resource}`)
      }),
      activateFrame: vi.fn((activeToken: number) => {
        order.push(`activate:${activeToken}`)
        return true
      }),
      cacheFrame: vi.fn((cachedToken: number) => {
        order.push(`cache:${cachedToken}`)
        return true
      }),
    } as unknown as Canvas
    const deactivate = activateCanvas(canvas)

    const foreground = loadCanvasFrame(canvas, 7, () => true)
    await activeStarted
    const background = prefetchCanvasPages(['page'])
    await Promise.resolve()
    await Promise.resolve()

    expect(canvas.stageManifest).toHaveBeenCalledTimes(1)
    releaseActiveResource?.()
    await expect(foreground).resolves.toBe(true)
    await expect(background).resolves.toEqual([{ revision: 4, page: { id: 'page' } }])
    deactivate()

    expect(order).toEqual([
      'stage:1',
      'fetch:active',
      'install:active',
      'activate:1',
      'stage:2',
      'fetch:prefetch',
      'install:prefetch',
      'cache:2',
    ])
  })

  it('runs a newly selected page before queued background prefetch staging', async () => {
    const order: string[] = []
    let releaseFirstResource: (() => void) | undefined
    let firstResourceStarted: (() => void) | undefined
    const firstStarted = new Promise<void>((resolve) => {
      firstResourceStarted = resolve
    })
    commands.getCanvasManifest.mockImplementation(
      async (generation: number) => new Uint8Array([generation]).buffer,
    )
    commands.getCanvasResource.mockImplementation(
      (generation: number) =>
        new Promise<ArrayBuffer>((resolve) => {
          order.push(`fetch:foreground:${generation}`)
          if (generation === 7) {
            firstResourceStarted?.()
            releaseFirstResource = () => resolve(new Uint8Array([generation]).buffer)
          } else {
            resolve(new Uint8Array([generation]).buffer)
          }
        }),
    )
    commands.prepareCanvasPage.mockResolvedValue({ revision: 4, page: { id: 'page' } })
    commands.getCanvasPageManifest.mockResolvedValue(new Uint8Array([3]).buffer)
    commands.getCanvasPageResource.mockImplementation(async () => {
      order.push('fetch:prefetch')
      return new Uint8Array([3]).buffer
    })
    let token = 0
    const canvas = {
      hasActiveManifest: vi.fn(() => false),
      stageManifest: vi.fn(() => {
        token += 1
        order.push(`stage:${token}`)
        return {
          token,
          missing: [token === 1 ? 'foreground:7' : token === 2 ? 'foreground:8' : 'prefetch'],
        }
      }),
      installResource: vi.fn(async (resource: string) => {
        order.push(`install:${resource}`)
      }),
      activateFrame: vi.fn((activeToken: number) => {
        order.push(`activate:${activeToken}`)
        return true
      }),
      cacheFrame: vi.fn((cachedToken: number) => {
        order.push(`cache:${cachedToken}`)
        return true
      }),
    } as unknown as Canvas
    const deactivate = activateCanvas(canvas)

    const firstForeground = loadCanvasFrame(canvas, 7, () => true)
    await firstStarted
    const background = prefetchCanvasPages(['page'])
    await Promise.resolve()
    await Promise.resolve()
    const selectedForeground = loadCanvasFrame(canvas, 8, () => true)
    await Promise.resolve()
    await Promise.resolve()

    expect(canvas.stageManifest).toHaveBeenCalledTimes(1)
    releaseFirstResource?.()
    await expect(firstForeground).resolves.toBe(true)
    await expect(selectedForeground).resolves.toBe(true)
    await expect(background).resolves.toEqual([{ revision: 4, page: { id: 'page' } }])
    deactivate()

    expect(order).toEqual([
      'stage:1',
      'fetch:foreground:7',
      'install:foreground:7',
      'activate:1',
      'stage:2',
      'fetch:foreground:8',
      'install:foreground:8',
      'activate:2',
      'stage:3',
      'fetch:prefetch',
      'install:prefetch',
      'cache:3',
    ])
  })

  it('lets foreground activation take over from a staged background prefetch', async () => {
    const order: string[] = []
    let releasePrefetchResource: (() => void) | undefined
    let prefetchResourceStarted: (() => void) | undefined
    const prefetchStarted = new Promise<void>((resolve) => {
      prefetchResourceStarted = resolve
    })
    commands.prepareCanvasPage.mockResolvedValue({ revision: 4, page: { id: 'page' } })
    commands.getCanvasPageManifest.mockResolvedValue(new Uint8Array([1]).buffer)
    commands.getCanvasPageResource.mockImplementation(
      () =>
        new Promise<ArrayBuffer>((resolve) => {
          order.push('fetch:prefetch')
          prefetchResourceStarted?.()
          releasePrefetchResource = () => resolve(new Uint8Array([1]).buffer)
        }),
    )
    commands.getCanvasManifest.mockResolvedValue(new Uint8Array([2]).buffer)
    commands.getCanvasResource.mockImplementation(async () => {
      order.push('fetch:active')
      return new Uint8Array([2]).buffer
    })
    let token = 0
    const canvas = {
      hasActiveManifest: vi.fn(() => false),
      stageManifest: vi.fn(() => {
        token += 1
        order.push(`stage:${token}`)
        return { token, missing: [token === 1 ? 'prefetch' : 'active'] }
      }),
      installResource: vi.fn(async (resource: string) => {
        order.push(`install:${resource}`)
      }),
      activateFrame: vi.fn((activeToken: number) => {
        order.push(`activate:${activeToken}`)
        return true
      }),
      cacheFrame: vi.fn((cachedToken: number) => {
        order.push(`cache:${cachedToken}`)
        return true
      }),
    } as unknown as Canvas
    const deactivate = activateCanvas(canvas)

    const background = prefetchCanvasPages(['page'])
    await prefetchStarted
    cancelCanvasPrefetch()
    const foreground = loadCanvasFrame(canvas, 8, () => true)
    await Promise.resolve()
    await Promise.resolve()

    expect(canvas.stageManifest).toHaveBeenCalledTimes(1)
    releasePrefetchResource?.()
    await expect(background).resolves.toEqual([])
    await expect(foreground).resolves.toBe(true)
    deactivate()

    expect(order).toEqual([
      'stage:1',
      'fetch:prefetch',
      'stage:2',
      'fetch:active',
      'install:active',
      'activate:2',
    ])
  })
})
