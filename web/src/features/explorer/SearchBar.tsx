import { Search } from 'lucide-react';
import { useEffect, useRef, useState, type FormEvent } from 'react';
import { useLocation, useNavigate } from 'react-router-dom';
import { classifyQuery } from './classify-query';
import { Button } from '@/components/ui/Button';
import { api, ApiError, errorMessage } from '@/lib/api';

export function SearchBar() {
  const navigate = useNavigate();
  const { pathname } = useLocation();
  const requestId = useRef(0);
  const [query, setQuery] = useState('');
  const [error, setError] = useState('');
  const [resolving, setResolving] = useState<{ id: number; path: string } | null>(null);
  const isResolving = resolving?.path === pathname;

  useEffect(
    () => () => {
      requestId.current += 1;
    },
    [pathname],
  );

  const submit = (event: FormEvent) => {
    event.preventDefault();
    const currentRequest = ++requestId.current;
    const result = classifyQuery(query);
    if ('error' in result) {
      setError(result.error);
      setResolving(null);
      return;
    }
    setError('');
    if ('path' in result) {
      setResolving(null);
      void navigate(result.path);
      return;
    }

    // Block hashes and transaction ids are both 64 hex characters. Only the
    // node can say which this is, so ask before choosing a route.
    setResolving({ id: currentRequest, path: pathname });
    void api
      .search(result.hash)
      .then(({ type }) => {
        if (requestId.current !== currentRequest) return;
        const prefix = type === 'block' ? 'block' : 'tx';
        void navigate(`/explorer/${prefix}/${result.hash}`);
      })
      .catch((error: unknown) => {
        if (requestId.current !== currentRequest) return;
        if (error instanceof ApiError && error.status === 404) {
          setError('No block or transaction on this chain has that hash.');
        } else if (error instanceof TypeError) {
          setError('Could not complete the search. Check your connection and try again.');
        } else {
          setError(errorMessage(error));
        }
      })
      .finally(() => {
        if (requestId.current === currentRequest) setResolving(null);
      });
  };

  return (
    <form onSubmit={submit} className="mb-4" noValidate>
      <div className="flex gap-2">
        <div className="xp-sunken bg-sunken focus-within:border-accent flex flex-1 items-center gap-2.5 rounded-xs px-3 transition-colors">
          <Search className="text-ink-muted size-4 shrink-0" aria-hidden />
          <input
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            aria-label="Search the chain"
            aria-invalid={Boolean(error)}
            placeholder="Block height, block hash, transaction ID, or t-address"
            className="text-ink placeholder:text-ink-muted w-full border-0 bg-transparent py-2 text-[13px] outline-none focus:ring-0"
          />
        </div>
        <Button type="submit" variant="ghost">
          {isResolving ? 'Searching…' : 'Search'}
        </Button>
      </div>
      {error && (
        <p role="alert" className="text-negative mt-2 text-[12px]">
          {error}
        </p>
      )}
    </form>
  );
}
