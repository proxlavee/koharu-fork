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
  // Performance optimization: Pre-compute a lookup table for layers and their children
  // to achieve O(1) lookup during traversal instead of O(N) array scans.
  const layerMap = new Map<string, Layer>()
  const childrenMap = new Map<string, Layer[]>()

  for (let i = 0; i < layers.length; i++) {
    const layer = layers[i]
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

  const result = new Set<string>()
  const visit = (id: string) => {
    const layer = layerMap.get(id)
    if (!layer) return
    if (isGroupLayer(layer)) {
      const children = childrenMap.get(id)
      if (children) {
        for (let i = 0; i < children.length; i++) visit(children[i].id)
      }
    } else {
      result.add(id)
    }
  }
  for (const id of selected) visit(id)
  return Array.from(result)
}

export function effectiveLayerVisibility(layers: Layer[], layer: Layer, layerMap?: Map<string, Layer>) {
  // Performance optimization: Allow passing a pre-computed layerMap to avoid
  // O(N) Array.find lookups in the parent traversal loop.
  let visible = layer.visibility.visible
  let opacity = layer.visibility.opacity
  let parent = layer.parent
  while (parent) {
    const group = layerMap ? layerMap.get(parent) : layers.find((candidate) => candidate.id === parent)
    if (!group || !isGroupLayer(group)) break
    visible &&= group.visibility.visible
    opacity *= group.visibility.opacity
    parent = group.parent
  }
  return { visible, opacity }
}
