import { Monitor, Moon, Sun } from 'lucide-react';
import { useRef, type KeyboardEvent } from 'react';
import { useTheme } from '@/lib/useTheme';
import type { ThemePreference } from '@/lib/theme';
import { cn } from '@/lib/cn';

const OPTIONS: ReadonlyArray<{ value: ThemePreference; label: string; Icon: typeof Sun }> = [
  { value: 'light', label: 'Light', Icon: Sun },
  { value: 'system', label: 'System', Icon: Monitor },
  { value: 'dark', label: 'Dark', Icon: Moon },
];

/**
 * Three explicit states rather than a two-way switch: "system" has to be
 * reachable, otherwise choosing light once permanently opts the user out of
 * following their OS.
 */
export function ThemeToggle() {
  const [preference, setPreference] = useTheme();
  const options = useRef<Array<HTMLButtonElement | null>>([]);

  const select = (index: number) => {
    const next = (index + OPTIONS.length) % OPTIONS.length;
    setPreference(OPTIONS[next]!.value);
    options.current[next]?.focus();
  };

  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (event.altKey || event.ctrlKey || event.metaKey) return;
    const current = options.current.indexOf(event.target as HTMLButtonElement);
    if (current === -1) return;
    const target = {
      ArrowRight: current + 1,
      ArrowDown: current + 1,
      ArrowLeft: current - 1,
      ArrowUp: current - 1,
      Home: 0,
      End: OPTIONS.length - 1,
    }[event.key];
    if (target === undefined) return;
    event.preventDefault();
    select(target);
  };

  return (
    <div
      role="radiogroup"
      aria-label="Colour theme"
      onKeyDown={onKeyDown}
      className="xp-sunken bg-sunken flex gap-0.5 rounded-xs p-0.5"
    >
      {OPTIONS.map(({ value, label, Icon }, index) => {
        const active = preference === value;
        return (
          <button
            key={value}
            ref={(element) => {
              options.current[index] = element;
            }}
            type="button"
            role="radio"
            aria-checked={active}
            aria-label={label}
            title={label}
            tabIndex={active ? 0 : -1}
            onClick={() => setPreference(value)}
            className={cn(
              'grid flex-1 cursor-pointer place-items-center rounded-xs py-1.5 transition-colors',
              active ? 'xp-raised bg-raised text-accent-strong' : 'text-ink-muted hover:text-ink',
            )}
          >
            <Icon className="size-3.5" />
          </button>
        );
      })}
    </div>
  );
}
