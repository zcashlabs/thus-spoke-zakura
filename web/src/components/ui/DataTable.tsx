import type { ReactNode } from 'react';
import { useNavigate } from 'react-router-dom';
import { cn } from '@/lib/cn';

/**
 * One table shell for every tabular surface, so the activity log and the block
 * list share column rhythm, header weight and row affordances instead of each
 * inventing their own.
 */
export interface Column {
  key: string;
  header: ReactNode;
  align?: 'left' | 'right';
  /** Hidden below the `sm` breakpoint. */
  collapse?: boolean;
  width?: string;
}

export function DataTable({ columns, children }: { columns: Column[]; children: ReactNode }) {
  return (
    <div className="overflow-x-auto">
      <table className="w-full min-w-[520px] text-[13px]">
        <thead>
          <tr className="border-line-strong bg-raised sticky top-0 z-10 border-b-2">
            {columns.map((column) => (
              <th
                key={column.key}
                scope="col"
                style={column.width ? { width: column.width } : undefined}
                className={cn(
                  'text-ink px-5 py-3 text-[11px] font-bold tracking-[0.12em] uppercase',
                  column.align === 'right' ? 'text-right' : 'text-left',
                  column.collapse && 'max-sm:hidden',
                )}
              >
                {column.header}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>{children}</tbody>
      </table>
    </div>
  );
}

export function Row({
  children,
  index = 0,
  to,
  className,
}: {
  children: ReactNode;
  index?: number;
  /** Makes the whole row a navigation target, not just the link inside it. */
  to?: string;
  className?: string;
}) {
  const navigate = useNavigate();

  return (
    <tr
      style={{ animationDelay: `${Math.min(index, 8) * 30}ms` }}
      // Mouse convenience only. Putting role/tabIndex here would override the
      // row's implicit `row` role and strip the table relationships; keyboard
      // and screen reader users navigate via the link inside the first cell,
      // which every clickable row is required to carry.
      onClick={
        to
          ? (event) => {
              if (
                event.defaultPrevented ||
                event.button !== 0 ||
                event.metaKey ||
                event.ctrlKey ||
                event.shiftKey ||
                event.altKey ||
                (event.target as Element).closest('a, button, input, select, textarea')
              )
                return;
              void navigate(to);
            }
          : undefined
      }
      className={cn(
        'border-line hover:bg-accent-soft animate-fade-in border-b transition-colors last:border-b-0',
        '[&>td]:px-5 [&>td]:py-3.5 [&>td]:align-middle',
        to && 'cursor-pointer',
        className,
      )}
    >
      {children}
    </tr>
  );
}

/** Right-aligned numerics use tabular figures so columns line up per digit. */
export function NumCell({ children, className }: { children: ReactNode; className?: string }) {
  return <td className={cn('text-right tabular-nums', className)}>{children}</td>;
}
