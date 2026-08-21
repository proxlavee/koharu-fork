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
  const layerMap = new Map(layers.map(l => [l.id, l]))
  const childrenMap = new Map<string, Layer[]>()
  for (const layer of layers) {
    if (layer.parent) {
      if (!childrenMap.has(layer.parent)) {
        childrenMap.set(layer.parent, [])
      }
      childrenMap.get(layer.parent)!.push(layer)
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

export function effectiveLayerVisibility(layers: Layer[] | Map<string, Layer>, layer: Layer) {
  let visible = layer.visibility.visible
  let opacity = layer.visibility.opacity
  let parent = layer.parent
  const layerMap = Array.isArray(layers) ? new Map(layers.map(l => [l.id, l])) : layers

  while (parent) {
    const group = layerMap.get(parent)
    if (!group || !isGroupLayer(group)) break
    visible &&= group.visibility.visible
    opacity *= group.visibility.opacity
    parent = group.parent
  }
  return { visible, opacity }
}
