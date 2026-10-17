import { describe, expect, it } from 'vitest';
import { ApiError } from './errors';

describe('ApiError display', () => {
  it.each([
    'the environment is still starting',
    'faucet treasury remains insufficient after replenishment',
  ])('preserves an expected 503 explanation: %s', (message) => {
    expect(new ApiError(503, message).message).toBe(message);
  });

  it('keeps unexpected server failures generic', () => {
    expect(new ApiError(500, 'internal database detail').message).toBe(
      'The environment hit an unexpected error. Check the service logs for details.',
    );
  });
});
