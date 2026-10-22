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
  const layerMap = new Map(layers.map((layer) => [layer.id, layer]))
  const childrenMap = new Map<string, string[]>()

  for (const layer of layers) {
    if (layer.parent) {
      const siblings = childrenMap.get(layer.parent)
      if (siblings) siblings.push(layer.id)
      else childrenMap.set(layer.parent, [layer.id])
    }
  }

  const result = new Set<string>()
  const visit = (id: string) => {
    const layer = layerMap.get(id)
    if (!layer) return
    if (isGroupLayer(layer)) {
      const children = childrenMap.get(id) ?? []
      for (const child of children) visit(child)
    } else {
      result.add(id)
    }
  }
  for (const id of selected) visit(id)
  return Array.from(result)
}

export function effectiveLayerVisibility(layers: Layer[] | Map<string, Layer>, layer: Layer) {
  let visible = layer.visibility.visible
  let opacity = layer.visibility.opacity
  let parent = layer.parent
  while (parent) {
    const group = layers instanceof Map
      ? layers.get(parent)
      : layers.find((candidate) => candidate.id === parent)
    if (!group || !isGroupLayer(group)) break
    visible &&= group.visibility.visible
    opacity *= group.visibility.opacity
    parent = group.parent
  }
  return { visible, opacity }
}
