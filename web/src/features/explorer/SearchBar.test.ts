import { describe, expect, it } from 'vitest';
import { classifyQuery } from './classify-query';

describe('classifyQuery', () => {
  it('routes heights, hashes and transparent addresses', () => {
    expect(classifyQuery('109')).toEqual({ path: '/explorer/block/109' });
    expect(classifyQuery('tmVmUHwsv6bqRvaQPz2azNTrP7taACNGq9F')).toEqual({
      path: '/explorer/address/tmVmUHwsv6bqRvaQPz2azNTrP7taACNGq9F',
    });
  });

  it('refuses to guess between a block hash and a transaction id', () => {
    // Both are 64 hex characters; routing on shape alone sent every block hash
    // to the transaction view, where it 500s.
    const hash = 'cb45d3bb523989b81da2b91ba7c5cdfeeace2f6e1a34a397b8e212729038905c';
    expect(classifyQuery(hash)).toEqual({ hash });
    expect(classifyQuery(hash.toUpperCase())).toEqual({ hash });
    const numericHash = '0'.repeat(64);
    expect(classifyQuery(numericHash)).toEqual({ hash: numericHash });
  });

  it('explains why a unified address has no public history', () => {
    const result = classifyQuery('uregtest155n5yem3ztkhlt9agagqfckzxng40mj7pfdvp4z4ssc048mmxqmw');
    expect(result).toHaveProperty('error');
    expect((result as { error: string }).error).toContain('shielded');
  });

  it('rejects empty and unrecognised input', () => {
    expect(classifyQuery('  ')).toHaveProperty('error');
    expect(classifyQuery('hello')).toHaveProperty('error');
  });
});
