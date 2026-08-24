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
  const layerMap = new Map<string, Layer>()
  const childrenMap = new Map<string, Layer[]>()

  for (const layer of layers) {
    layerMap.set(layer.id, layer)
    if (layer.parent) {
      const children = childrenMap.get(layer.parent)
      if (children) {
        children.push(layer)
      } else {
        childrenMap.set(layer.parent, [layer])
      }
    }
  }

  const result: string[] = []
  const resultSet = new Set<string>()

  const visit = (id: string) => {
    const layer = layerMap.get(id)
    if (!layer) return
    if (isGroupLayer(layer)) {
      const children = childrenMap.get(id)
      if (children) {
        for (const child of children) visit(child.id)
      }
    } else if (!resultSet.has(id)) {
      resultSet.add(id)
      result.push(id)
    }
  }
  for (const id of selected) visit(id)
  return result
}

export function effectiveLayerVisibility(
  layers: Layer[],
  layer: Layer,
  layerMap?: Map<string, Layer>,
) {
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
