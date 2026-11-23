import assert from 'node:assert/strict';
import test from 'node:test';
import { shuffleContributors } from '../contributor-order.mjs';

test('contributors get a different slot order without changing membership or the source list', () => {
  const contributors = ['a', 'b', 'c', 'd'];
  const shuffled = shuffleContributors(contributors, () => 0);
  assert.deepEqual(shuffled, ['b', 'c', 'd', 'a']);
  assert.deepEqual([...shuffled].sort(), [...contributors].sort());
  assert.deepEqual(contributors, ['a', 'b', 'c', 'd']);
});
