import assert from 'node:assert/strict';
import test from 'node:test';
import { storyPhase, storyProgress } from '../story.mjs';

test('story progress starts before the section reaches the header and completes as it leaves', () => {
  assert.equal(storyProgress(900, 1980, 900), 0);
  assert.ok(storyProgress(64, 1980, 900) > 0);
  assert.ok(storyProgress(64, 1980, 900) < 0.25);
  assert.equal(storyProgress(-1080, 1980, 900), 1);
});

test('each quarter of the scroll selects the corresponding stage', () => {
  assert.equal(storyPhase(0), 0);
  assert.equal(storyPhase(0.25), 1);
  assert.equal(storyPhase(0.5), 2);
  assert.equal(storyPhase(0.75), 3);
  assert.equal(storyPhase(1), 3);
});
