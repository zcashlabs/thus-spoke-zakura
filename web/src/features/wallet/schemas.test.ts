import { describe, expect, it } from 'vitest';
import { sendSchema } from './schemas';

const base = {
  from_account: '1',
  to_account: '2',
  source_pool: 'ironwood' as const,
  destination_pool: 'ironwood' as const,
  amount: '1',
  memo: '',
};

const memoIssue = (input: Record<string, unknown>) => {
  const result = sendSchema.safeParse(input);
  return result.success
    ? undefined
    : result.error.issues.find((issue) => issue.path[0] === 'memo')?.message;
};

describe('sendSchema', () => {
  it.each([
    ['1', 'ironwood', 'ironwood', false],
    ['1', 'transparent', 'transparent', false],
    ['1', 'ironwood', 'transparent', true],
    ['1', 'transparent', 'ironwood', true],
    ['2', 'ironwood', 'ironwood', true],
    ['2', 'transparent', 'transparent', true],
    ['2', 'ironwood', 'transparent', true],
    ['2', 'transparent', 'ironwood', true],
  ] as const)('validates destination %s and route %s → %s', (to, source, destination, valid) => {
    const result = sendSchema.safeParse({
      ...base,
      to_account: to,
      source_pool: source,
      destination_pool: destination,
    });
    expect(result.success).toBe(valid);
    if (!result.success) {
      expect(result.error.issues.find((issue) => issue.path[0] === 'to_account')?.message).toBe(
        'Choose a different account or a different destination pool.',
      );
    }
  });

  it('keeps memo restrictions for same-account pool transfers', () => {
    expect(
      sendSchema.safeParse({
        ...base,
        to_account: '1',
        source_pool: 'transparent',
        memo: 'private note',
      }).success,
    ).toBe(true);
    expect(
      memoIssue({
        ...base,
        to_account: '1',
        destination_pool: 'transparent',
        memo: 'hi',
      }),
    ).toMatch(/transparent outputs cannot carry a memo/i);
  });

  it('requires an address when sending outside the development accounts', () => {
    const result = sendSchema.safeParse({ ...base, to_account: 'address', to_address: '  ' });
    expect(result.success).toBe(false);
    if (!result.success) {
      expect(result.error.issues[0]?.path).toEqual(['to_address']);
    }
  });

  it('accepts a send to an address', () => {
    const result = sendSchema.safeParse({
      ...base,
      to_account: 'address',
      to_address: ' uregtest1external ',
    });
    expect(result.success).toBe(true);
    if (result.success) {
      expect(result.data.to_account).toBe('address');
      expect(result.data.to_address).toBe('uregtest1external');
    }
  });

  it('limits memos to 512 bytes, counting multi-byte characters', () => {
    expect(sendSchema.safeParse({ ...base, memo: 'a'.repeat(512) }).success).toBe(true);
    // 'é' is two bytes in UTF-8, so 257 of them exceed the limit at 257 characters.
    const result = sendSchema.safeParse({ ...base, memo: 'é'.repeat(257) });
    expect(result.success).toBe(false);
    if (!result.success) {
      expect(result.error.issues[0]?.path).toEqual(['memo']);
    }
  });

  it('accepts a transfer between two different accounts', () => {
    const result = sendSchema.safeParse(base);
    expect(result.success).toBe(true);
    if (result.success) {
      expect(result.data.amount).toBe(100_000_000n);
    }
  });

  it('accepts a memo to an ironwood destination', () => {
    expect(memoIssue({ ...base, memo: 'rent for October' })).toBeUndefined();
    expect(sendSchema.safeParse({ ...base, memo: 'rent for October' }).success).toBe(true);
  });

  it('refuses a memo to a transparent destination', () => {
    expect(memoIssue({ ...base, destination_pool: 'transparent', memo: 'hi' })).toMatch(
      /transparent outputs cannot carry a memo/i,
    );
    expect(sendSchema.safeParse({ ...base, destination_pool: 'transparent' }).success).toBe(true);
  });

  it('limits memos to 512 bytes rather than characters', () => {
    expect(memoIssue({ ...base, memo: 'a'.repeat(512) })).toBeUndefined();
    expect(memoIssue({ ...base, memo: 'a'.repeat(513) })).toMatch(/512 bytes/);
    // Each of these characters is three UTF-8 bytes: 171 * 3 = 513.
    expect(memoIssue({ ...base, memo: '桜'.repeat(171) })).toMatch(/512 bytes/);
  });

  it('refuses a memo ending in NUL', () => {
    expect(memoIssue({ ...base, memo: 'hi\0' })).toMatch(/NUL/);
    expect(memoIssue({ ...base, memo: 'a\0b' })).toBeUndefined();
  });
});
