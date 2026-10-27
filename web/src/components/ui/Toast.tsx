import * as RadixToast from '@radix-ui/react-toast';
import { AlertTriangle, Check, X } from 'lucide-react';
import { useCallback, useMemo, useState, type ReactNode } from 'react';
import {
  ToastContext,
  type ToastAction,
  type ToastApi,
  type ToastMessage,
  type ToastTone,
} from './toast-context';
import { Button } from './Button';
import { cn } from '@/lib/cn';

let nextId = 0;

export function ToastProvider({ children }: { children: ReactNode }) {
  const [messages, setMessages] = useState<ToastMessage[]>([]);

  const push = useCallback(
    (tone: ToastTone, title: string, detail?: string, action?: ToastAction) => {
      const id = nextId++;
      setMessages((current) => [...current, { id, tone, title, detail, action }]);
      return () => setMessages((current) => current.filter((message) => message.id !== id));
    },
    [],
  );

  const api = useMemo<ToastApi>(
    () => ({
      success: (title, detail) => {
        push('success', title, detail);
      },
      error: (title, detail, action) => push('error', title, detail, action),
    }),
    [push],
  );

  return (
    <ToastContext value={api}>
      <RadixToast.Provider swipeDirection="right" duration={6000}>
        {children}
        {messages.map((message) => (
          <RadixToast.Root
            key={message.id}
            type={message.tone === 'success' ? 'background' : 'foreground'}
            duration={message.action ? Infinity : message.tone === 'error' ? 10000 : 5000}
            onOpenChange={(open) => {
              if (!open) setMessages((current) => current.filter((m) => m.id !== message.id));
            }}
            className={cn(
              'xp-raised animate-slide-in flex shrink-0 items-start gap-2.5 rounded-xs p-3 text-[12px] shadow-(--shadow-float)',
              'data-[state=closed]:animate-out data-[state=closed]:fade-out',
              message.tone === 'success'
                ? 'bg-positive-soft text-positive'
                : 'bg-negative-soft text-negative',
            )}
            style={{ touchAction: 'pan-y' }}
          >
            {message.tone === 'success' ? (
              <Check className="mt-px size-3.5 shrink-0" aria-hidden />
            ) : (
              <AlertTriangle className="mt-px size-3.5 shrink-0" aria-hidden />
            )}
            <div className="min-w-0 flex-1 wrap-anywhere">
              <div className="flex items-start gap-2.5">
                <RadixToast.Title className="min-w-0 flex-1 font-semibold">
                  {message.title}
                </RadixToast.Title>
                <RadixToast.Close
                  aria-label="Dismiss notification"
                  className="hover:bg-ink/5 grid size-6 shrink-0 cursor-pointer place-items-center rounded-xs"
                >
                  <X className="size-3.5" aria-hidden />
                </RadixToast.Close>
              </div>
              {message.detail && (
                <RadixToast.Description className="mt-1 leading-relaxed">
                  {message.detail}
                </RadixToast.Description>
              )}
              {message.action && (
                <div className="mt-2 flex justify-end">
                  <RadixToast.Action altText={message.action.label} asChild>
                    <Button size="sm" onClick={message.action.onClick}>
                      {message.action.label}
                    </Button>
                  </RadixToast.Action>
                </div>
              )}
            </div>
          </RadixToast.Root>
        ))}
        <RadixToast.Viewport className="fixed top-5 right-5 z-100 flex max-h-[calc(100dvh-40px)] w-[min(388px,calc(100vw-40px))] flex-col gap-2 overflow-y-auto overscroll-contain p-1 outline-none" />
      </RadixToast.Provider>
    </ToastContext>
  );
}
