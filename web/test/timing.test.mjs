import assert from 'node:assert/strict'
import test from 'node:test'
import { needsSeek, positionAt } from '../src/timing.ts'

test('paused and playing snapshots use authoritative room time', () => {
  const state = { paused: false, positionSec: 10, updatedAtMs: 20_000 }
  assert.equal(positionAt(state, 23_500), 13.5)
  assert.equal(positionAt(state, 19_950), 10)
  assert.equal(positionAt({ ...state, paused: true }, 23_500), 10)
})

test('commands correct small seeks, periodic checks only correct major drift', () => {
  assert.equal(needsSeek(11, 10, true), true)
  assert.equal(needsSeek(11, 10, false), false)
  assert.equal(needsSeek(12.1, 10, false), true)
  assert.equal(needsSeek(NaN, 10, true), false)
})
