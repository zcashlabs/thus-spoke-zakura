import { z } from 'zod';

/**
 * Runtime contracts for the tsz-server API. These are validated rather than
 * merely declared: if the Rust structs change shape, the UI fails loudly at
 * the boundary instead of rendering `undefined` somewhere deep in a tree.
 *
 * Zatoshi fields arrive as JSON numbers. Total supply (2.1e15 zatoshi) sits
 * well inside Number.MAX_SAFE_INTEGER, so the wire value is exact; it is
 * converted to bigint immediately so no arithmetic ever touches a float.
 */

const zatoshi = z
  .number()
  .int()
  .nonnegative()
  .transform((value) => BigInt(value));

export const poolSchema = z.enum(['transparent', 'orchard']);
export type Pool = z.infer<typeof poolSchema>;

export const accountSchema = z.object({
  id: z.number().int().positive(),
  name: z.string(),
  unified_address: z.string(),
  transparent_address: z.string(),
  unified_full_viewing_key: z.string().optional(),
  transparent_zatoshi: zatoshi,
  orchard_zatoshi: zatoshi,
});
export type Account = z.infer<typeof accountSchema>;

export const activitySchema = z.object({
  id: z.string(),
  kind: z.enum(['send', 'faucet']).catch('send'),
  from_account: z.number().int().nullable().default(null),
  to_account: z.number().int(),
  source_pool: poolSchema,
  destination_pool: poolSchema,
  amount_zatoshi: zatoshi,
  txid: z.string(),
  block_hash: z.string().nullable().default(null),
  status: z.string(),
  created_at: z.string(),
});
export type Activity = z.infer<typeof activitySchema>;

export const chainInfoSchema = z.object({
  chain: z.string(),
  blocks: z.number().int().nonnegative(),
  bestblockhash: z.string().default(''),
  verificationprogress: z.number().default(0),
});
export type ChainInfo = z.infer<typeof chainInfoSchema>;

/** The ports this instance publishes, so you can point your own code at them. */
export const endpointsSchema = z.object({
  dashboard: z.string(),
  zakura_rpc: z.string(),
  lightwalletd: z.string(),
  p2p: z.string(),
});
export type Endpoints = z.infer<typeof endpointsSchema>;

export const walletSyncSchema = z.object({
  state: z.enum(['ready', 'syncing', 'error']),
  fully_scanned_height: z.number().int().nonnegative().nullable(),
  observed_height: z.number().int().nonnegative().nullable(),
  last_success_at: z.number().int().nonnegative().nullable(),
  error: z.string().nullable(),
});
export type WalletSync = z.infer<typeof walletSyncSchema>;

export const statusSchema = z.object({
  instance: z.string(),
  node: chainInfoSchema.nullable().default(null),
  account_count: z.number().int().nonnegative(),
  auto_mine: z.boolean(),
  network: z.string(),
  node_mode: z.enum(['docker', 'local_binary', 'external_rpc']).default('docker'),
  // Optional so a dashboard built before the server grew this field still loads.
  endpoints: endpointsSchema.optional(),
  // Optional so a newer dashboard remains compatible with an older server.
  wallet_sync: walletSyncSchema.optional(),
});
export type Status = z.infer<typeof statusSchema>;

/** A pool's running total, and how this block changed it. Deltas are signed. */
export const valuePoolSchema = z.looseObject({
  id: z.string(),
  chainValueZat: z.number().int().default(0),
  valueDeltaZat: z.number().int().default(0),
  monitored: z.boolean().default(false),
});
export type ValuePool = z.infer<typeof valuePoolSchema>;

/** Node RPC payloads are passed through; only the fields the UI reads are pinned. */
export const blockSchema = z.looseObject({
  hash: z.string(),
  height: z.number().int(),
  time: z.number().int(),
  size: z.number().int(),
  nTx: z.number().int(),
  confirmations: z.number().int(),
  previousblockhash: z.string().optional(),
  tx: z.array(z.unknown()).default([]),
  difficulty: z.number().optional(),
  merkleroot: z.string().optional(),
  finalorchardroot: z.string().optional(),
  blockcommitments: z.string().optional(),
  version: z.number().optional(),
  chainSupply: z.looseObject({ chainValueZat: z.number().int().default(0) }).optional(),
  valuePools: z.array(valuePoolSchema).default([]),
});
export type Block = z.infer<typeof blockSchema>;

export const blockPageSchema = z.object({
  blocks: z.array(blockSchema),
  next_before: z.number().int().nullable(),
});
export type BlockPage = z.infer<typeof blockPageSchema>;

/** Transparent inputs. A coinbase input has no previous output to reference. */
export const txInputSchema = z.looseObject({
  coinbase: z.string().optional(),
  txid: z.string().optional(),
  vout: z.number().int().optional(),
  /** Copied from the spent output when the explorer can resolve the prevout. */
  valueZat: z.number().int().optional(),
  scriptPubKey: z
    .looseObject({
      addresses: z.array(z.string()).default([]),
      type: z.string().optional(),
    })
    .optional(),
});

export const txOutputSchema = z.looseObject({
  n: z.number().int().default(0),
  valueZat: z.number().int().default(0),
  scriptPubKey: z
    .looseObject({
      addresses: z.array(z.string()).default([]),
      type: z.string().optional(),
    })
    .optional(),
});
export type TxOutput = z.infer<typeof txOutputSchema>;
export type TxInput = z.infer<typeof txInputSchema>;

export const transactionSchema = z.looseObject({
  txid: z.string(),
  height: z.number().int().optional(),
  blockhash: z.string().optional(),
  blocktime: z.number().int().optional(),
  confirmations: z.number().int().optional(),
  size: z.number().int().optional(),
  vin: z.array(txInputSchema).default([]),
  vout: z.array(txOutputSchema).default([]),
  vShieldedSpend: z.array(z.unknown()).default([]),
  vShieldedOutput: z.array(z.unknown()).default([]),
  orchard: z
    .looseObject({
      actions: z.array(z.unknown()).default([]),
      /**
       * Net value moving in or out of the Orchard pool. This is public chain
       * data (ZIP 224), and on an otherwise shielded transfer it is the fee.
       */
      valueBalanceZat: z.number().optional(),
    })
    .optional(),
  valueBalanceZat: z.number().optional(),
  version: z.number().optional(),
  versiongroupid: z.string().optional(),
  expiryheight: z.number().int().optional(),
  locktime: z.number().int().optional(),
});
export type Transaction = z.infer<typeof transactionSchema>;

export const mempoolSchema = z.object({ transactions: z.array(z.string()) });

export const addressSchema = z.object({
  address: z.string(),
  balance: z.looseObject({
    balance: z.number().default(0),
    received: z.number().default(0),
  }),
});
export type AddressInfo = z.infer<typeof addressSchema>;

export const seedSchema = z.object({ seed_hex: z.string(), warning: z.string() });

export const apiErrorSchema = z.object({
  error: z.object({ message: z.string(), status: z.number().int() }),
});
