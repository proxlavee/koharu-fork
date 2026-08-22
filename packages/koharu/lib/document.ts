import type { Layer } from '@koharu/bridge/protocol'

export function isTextLayer(layer: Layer): layer is Extract<Layer, { type: 'text' }> {
  return layer.type === 'text'
}

export function isGroupLayer(layer: Layer): layer is Extract<Layer, { type: 'group' }> {
  return layer.type === 'group'
}

export function isLockedLayer(layer: Layer): boolean {
  return layer.type === 'artwork'
}

export function layerChildren(layers: Layer[], parent: string): Layer[] {
  return layers.filter((layer) => layer.parent === parent)
}

export function expandLayerSelection(layers: Layer[], selected: string[]): string[] {
  const result: string[] = []
  // Build a map of layers and a map of children to change O(n) traversals into O(1) lookups
  // during selection expansion, significantly improving real-time rendering performance.
  const layerMap = new Map<string, Layer>()
  const childrenMap = new Map<string, Layer[]>()
  for (const layer of layers) {
    layerMap.set(layer.id, layer)
    if (layer.parent) {
      let children = childrenMap.get(layer.parent)
      if (!children) {
        children = []
        childrenMap.set(layer.parent, children)
      }
      children.push(layer)
    }
  }

  const visit = (id: string) => {
    const layer = layerMap.get(id)
    if (!layer) return
    if (isGroupLayer(layer)) {
      const children = childrenMap.get(id) || []
      for (const child of children) visit(child.id)
    } else if (!result.includes(id)) {
      result.push(id)
    }
  }
  for (const id of selected) visit(id)
  return result
}

export function effectiveLayerVisibility(layers: Layer[] | ReadonlyMap<string, Layer>, layer: Layer) {
  let visible = layer.visibility.visible
  let opacity = layer.visibility.opacity
  let parent = layer.parent
  // Provide backwards compatibility for arrays, but prefer ReadonlyMap to avoid O(n) lookup bottlenecks.
  const getLayer = (id: string) => {
    return Array.isArray(layers) ? layers.find((l) => l.id === id) : layers.get(id)
  }
  while (parent) {
    const group = getLayer(parent)
    if (!group || !isGroupLayer(group)) break
    visible &&= group.visibility.visible
    opacity *= group.visibility.opacity
    parent = group.parent
  }
  return { visible, opacity }
}
