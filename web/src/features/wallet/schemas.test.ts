import { describe, expect, it } from 'vitest';
import { sendSchema } from './schemas';

const base = {
  from_account: '1',
  to_account: '2',
  source_pool: 'orchard' as const,
  destination_pool: 'orchard' as const,
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
  it('rejects a send to the same account', () => {
    // The dialog used to default both sides to the account it was opened from,
    // so this was reachable with two clicks and cost a fee to discover.
    const result = sendSchema.safeParse({ ...base, from_account: '3', to_account: '3' });
    expect(result.success).toBe(false);
    if (!result.success) {
      const issue = result.error.issues.find((candidate) => candidate.path[0] === 'to_account');
      expect(issue?.message).toMatch(/different account/i);
    }
  });

  it('accepts a transfer between two different accounts', () => {
    const result = sendSchema.safeParse(base);
    expect(result.success).toBe(true);
    if (result.success) {
      expect(result.data.amount).toBe(100_000_000n);
    }
  });

  it('accepts a memo to an orchard destination', () => {
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
