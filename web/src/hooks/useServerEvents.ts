import { useEffect } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { queryKeys } from './queries';

/**
 * The server pushes a topic string ("wallet" or "chain") over SSE. Rather than
 * refetching everything on every event, each topic invalidates only the queries
 * it affects; React Query then refetches just what is actually mounted.
 */
const TOPICS: Record<string, readonly unknown[][]> = {
  wallet: [[...queryKeys.accounts], ['activity'], ['send-quote']],
  chain: [[...queryKeys.status], ['blocks'], [...queryKeys.mempool]],
  sync: [[...queryKeys.status]],
};

export function useServerEvents(): void {
  const queryClient = useQueryClient();

  useEffect(() => {
    const source = new EventSource('/api/v1/events');

    const onUpdate = (event: MessageEvent<string>) => {
      const keys = TOPICS[event.data] ?? Object.values(TOPICS).flat();
      for (const key of keys) {
        void queryClient.invalidateQueries({ queryKey: key });
      }
    };

    source.addEventListener('update', onUpdate as EventListener);
    return () => {
      source.removeEventListener('update', onUpdate as EventListener);
      source.close();
    };
  }, [queryClient]);
}
