import { fireEvent, render, screen } from '@testing-library/react'
import { useRef } from 'react'
import { describe, expect, it, vi } from 'vitest'

import { SelectionControls } from '@/components/editor/SelectionControls'
import type { Frame, TransformFrame } from '@koharu/bridge/protocol'

const frame: Frame = { x: 10, y: 20, width: 100, height: 50, angle_degrees: 0 }

function Harness({
  onTransformStart,
  onTransformFrame,
  onTransformEnd,
}: {
  onTransformStart: (elements: TransformFrame[]) => void
  onTransformFrame: (elements: TransformFrame[]) => void
  onTransformEnd: () => void
}) {
  const container = useRef<HTMLDivElement>(null)
  return (
    <div ref={container} data-testid='container'>
      <SelectionControls
        container={container}
        element='text'
        frame={frame}
        camera={{ zoom: 1, translation: [0, 0] }}
        edgesOnly
        onTransformStart={onTransformStart}
        onTransformFrame={onTransformFrame}
        onTransformEnd={onTransformEnd}
      />
    </div>
  )
}

function renderControls() {
  const onTransformStart = vi.fn()
  const onTransformFrame = vi.fn()
  const onTransformEnd = vi.fn()
  render(
    <Harness
      onTransformStart={onTransformStart}
      onTransformFrame={onTransformFrame}
      onTransformEnd={onTransformEnd}
    />,
  )

  const container = screen.getByTestId('container')
  vi.spyOn(container, 'getBoundingClientRect').mockReturnValue({
    x: 0,
    y: 0,
    top: 0,
    right: 500,
    bottom: 500,
    left: 0,
    width: 500,
    height: 500,
    toJSON: () => ({}),
  })
  const handle = container.querySelector<HTMLElement>('[data-rotate-handle]')
  expect(handle).toBeInstanceOf(HTMLElement)
  Object.assign(handle!, {
    setPointerCapture: vi.fn(),
    hasPointerCapture: vi.fn(() => true),
    releasePointerCapture: vi.fn(),
  })

  return { handle: handle!, onTransformStart, onTransformFrame, onTransformEnd }
}

describe('selection controls', () => {
  it('uses the release position when a rotate drag has no intermediate move event', () => {
    const { handle, onTransformStart, onTransformFrame, onTransformEnd } = renderControls()

    fireEvent.pointerDown(handle, {
      button: 0,
      pointerId: 7,
      clientX: 60,
      clientY: -5,
    })
    fireEvent.pointerUp(handle, {
      button: 0,
      pointerId: 7,
      clientX: 110,
      clientY: 45,
    })

    expect(onTransformStart).toHaveBeenCalledWith([{ element: 'text', frame }])
    expect(onTransformFrame).toHaveBeenCalledOnce()
    expect(onTransformFrame.mock.calls[0]?.[0]?.[0]?.frame.angle_degrees).toBeCloseTo(90)
    expect(onTransformEnd).toHaveBeenCalledOnce()
  })

  it('does not treat pointer-cancellation coordinates as a final transform', () => {
    const { handle, onTransformFrame, onTransformEnd } = renderControls()

    fireEvent.pointerDown(handle, {
      button: 0,
      pointerId: 7,
      clientX: 60,
      clientY: -5,
    })
    fireEvent.pointerMove(handle, {
      pointerId: 7,
      clientX: 110,
      clientY: 45,
    })
    fireEvent.pointerCancel(handle, {
      pointerId: 7,
      clientX: 500,
      clientY: 500,
    })

    expect(onTransformFrame).toHaveBeenCalledOnce()
    expect(onTransformFrame.mock.calls[0]?.[0]?.[0]?.frame.angle_degrees).toBeCloseTo(90)
    expect(onTransformEnd).toHaveBeenCalledOnce()
  })
})
