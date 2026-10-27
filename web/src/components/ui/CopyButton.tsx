import { useEffect, useRef, useState, type ReactNode } from 'react';
import { Check } from 'lucide-react';
import { useToast } from './toast-context';

/**
 * Copying gave no feedback in the previous UI, so there was no way to tell a
 * successful copy from a no-op. The confirmation is also announced politely
 * for screen readers, and a failed copy raises an error notification.
 */
export function CopyButton({
  value,
  label,
  icon,
}: {
  value: string;
  label: string;
  icon: ReactNode;
}) {
  const [copied, setCopied] = useState(false);
  const toast = useToast();
  const dismissFailure = useRef<(() => void) | null>(null);

  useEffect(() => {
    if (!copied) return;
    const timer = setTimeout(() => setCopied(false), 1600);
    return () => clearTimeout(timer);
  }, [copied]);

  return (
    <button
      type="button"
      aria-label={label}
      className="text-ink-muted hover:text-accent cursor-pointer rounded transition-colors"
      onClick={() => {
        let write: Promise<void>;
        try {
          write = navigator.clipboard.writeText(value);
        } catch (error) {
          write = Promise.reject(error instanceof Error ? error : new Error(String(error)));
        }
        void write.then(
          () => {
            dismissFailure.current?.();
            dismissFailure.current = null;
            setCopied(true);
          },
          () => {
            setCopied(false);
            dismissFailure.current?.();
            dismissFailure.current = toast.error(
              'Could not copy',
              `The browser blocked clipboard access. Copy it manually: ${value}`,
            );
          },
        );
      }}
    >
      {copied ? <Check className="text-accent size-4" /> : icon}
      <span className="sr-only" aria-live="polite">
        {copied ? 'Copied' : ''}
      </span>
    </button>
  );
}
