import type { RoomState } from './room'

export function positionAt(state: RoomState, serverNowMs: number): number {
  return state.positionSec + (state.paused ? 0 : Math.max(0, serverNowMs - state.updatedAtMs) / 1000)
}

export function needsSeek(actual: number, target: number, immediate: boolean): boolean {
  return Number.isFinite(actual) && Math.abs(actual - target) > (immediate ? 0.15 : 2)
}
