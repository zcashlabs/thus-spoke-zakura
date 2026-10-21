import type { z } from 'zod';
import { apiErrorSchema } from './schemas';
import { ApiError } from './errors';

const BASE = '/api/v1';

async function readError(response: Response): Promise<never> {
  let message = `Request failed (${response.status})`;
  try {
    const parsed = apiErrorSchema.safeParse(await response.json());
    if (parsed.success) message = parsed.data.error.message;
  } catch {
    // Body was empty or not JSON; the status-derived message stands.
  }
  throw new ApiError(response.status, message);
}

export async function requestWithResponse<T>(
  path: string,
  schema: z.ZodType<T>,
  init?: RequestInit,
): Promise<{ data: T; response: Response }> {
  const response = await fetch(`${BASE}${path}`, {
    ...init,
    headers: { 'content-type': 'application/json', ...init?.headers },
  });

  if (!response.ok) await readError(response);

  const parsed = schema.safeParse(await response.json());
  if (!parsed.success) {
    throw new ApiError(
      response.status,
      `Unexpected response shape for ${path}: ${parsed.error.issues[0]?.message ?? 'unknown'}`,
    );
  }
  return { data: parsed.data, response };
}

export async function request<T>(
  path: string,
  schema: z.ZodType<T>,
  init?: RequestInit,
): Promise<T> {
  return (await requestWithResponse(path, schema, init)).data;
}

export function post<T>(path: string, schema: z.ZodType<T>, body: unknown): Promise<T> {
  return request(path, schema, { method: 'POST', body: JSON.stringify(body) });
}

/** Idempotency keys must be 8-128 characters; randomUUID satisfies that. */
export function idempotencyKey(): string {
  return crypto.randomUUID();
}
