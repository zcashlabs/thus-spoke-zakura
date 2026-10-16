import type { MouseEvent } from 'react';
import { useNavigate } from 'react-router-dom';

/**
 * Opens `to` when the row is clicked. Links inside the row navigate on their
 * own, and modifier clicks must not move the current tab, so both are skipped.
 */
export function useRowNavigation(to: string | null | undefined) {
  const navigate = useNavigate();
  if (!to) return undefined;

  return (event: MouseEvent) => {
    const onLink = event.target instanceof Element && event.target.closest('a');
    const withModifier = event.metaKey || event.ctrlKey || event.shiftKey || event.altKey;
    if (!onLink && !withModifier) void navigate(to);
  };
}
