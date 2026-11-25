import assert from 'node:assert/strict'
import test from 'node:test'
import { joinMessage } from '../src/room.ts'

test('an explicit invitation never reads or sends the host token, even when empty', () => {
  for (const guestToken of ['guest-token', '']) {
    assert.deepEqual(joinMessage(guestToken, () => assert.fail('read host token')), { type: 'join', guestToken })
  }
})

test('without an invitation, the client joins only with a stored host token', () => {
  assert.deepEqual(joinMessage(null, () => 'host-token'), { type: 'join', hostToken: 'host-token' })
  assert.equal(joinMessage(null, () => null), null)
})
