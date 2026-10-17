import { NavLink } from 'react-router-dom';
import { Box, Database, WalletCards } from 'lucide-react';
import type { ReactNode } from 'react';
import { cn } from '@/lib/cn';
import { SakuraMark } from '@/components/ui/SakuraMark';
import { ThemeToggle } from '@/components/ui/ThemeToggle';
import { useStatus } from '@/hooks/queries';
import { environmentReadiness } from '@/lib/status';

const NAV = [
  { to: '/wallet', label: 'Wallet', icon: WalletCards },
  { to: '/explorer', label: 'Explorer', icon: Box },
  { to: '/network', label: 'Network', icon: Database },
] as const;

export function Shell({ children }: { children: ReactNode }) {
  const { data: status } = useStatus();
  const readiness = status ? environmentReadiness(status) : null;

  return (
    <div className="min-h-screen md:grid md:grid-cols-[220px_minmax(0,1fr)]">
      <aside className="border-line bg-panel fixed bottom-0 z-10 flex h-16 w-full flex-row border-t px-2 md:top-0 md:h-screen md:w-[220px] md:flex-col md:border-t-0 md:border-r md:px-3 md:py-5">
        <div className="mb-12 hidden items-center gap-3 md:flex">
          <div className="xp-raised text-accent bg-accent-soft grid size-8 place-items-center rounded-xs">
            <SakuraMark className="size-5" />
          </div>
          <div>
            <b className="block text-sm">Thus Spoke</b>
            <span className="text-ink-muted block text-xs tracking-[0.1em] uppercase">Zakura</span>
          </div>
        </div>

        <nav className="grid w-full grid-cols-3 gap-1.5 md:grid-cols-1">
          {NAV.map(({ to, label, icon: Icon }) => (
            <NavLink
              key={to}
              to={to}
              className={({ isActive }) =>
                cn(
                  'text-ink-muted hover:bg-accent-soft flex cursor-pointer items-center gap-2.5 rounded-xs px-2.5 py-2 text-left text-[13px] transition-colors',
                  'max-md:flex-col max-md:justify-center max-md:gap-0.5 max-md:text-[11px]',
                  isActive &&
                    'bg-accent-soft text-accent-strong border-accent border-l-2 font-semibold',
                )
              }
            >
              <Icon className="size-4" />
              {label}
            </NavLink>
          ))}
        </nav>

        {/* Desktop: above the status pill. Mobile: in the header row,
            since the sidebar collapses to a bottom tab bar. */}
        <div className="mt-auto hidden md:block">
          <ThemeToggle />
        </div>

        <div className="xp-sunken bg-sunken mt-2 hidden items-center gap-2.5 rounded-xs p-2.5 md:flex">
          <span
            className={cn('size-2 rounded-xs', readiness?.ready ? 'bg-positive' : 'bg-ink-subtle')}
            aria-hidden
          />
          <div className="text-[12px]">
            <b className="block">{status?.network ?? 'Connecting'}</b>
            <span className="text-ink-muted block">{readiness?.label ?? 'Connecting'}</span>
            <span className="text-ink-muted block">Height {status?.node?.blocks ?? '—'}</span>
          </div>
        </div>
      </aside>

      <main className="mx-auto w-full max-w-[1400px] px-4 py-6 pb-24 md:col-start-2 md:px-8 md:py-8 md:pb-14">
        {children}
      </main>
    </div>
  );
}
