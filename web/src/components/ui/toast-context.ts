import { createContext, use } from 'react';

export type ToastTone = 'success' | 'error';

export interface ToastAction {
  label: string;
  onClick: () => void;
}

export interface ToastMessage {
  id: number;
  tone: ToastTone;
  title: string;
  detail?: string | undefined;
  action?: ToastAction | undefined;
}

export interface ToastApi {
  success: (title: string, detail?: string) => void;
  error: (title: string, detail?: string, action?: ToastAction) => () => void;
}

export const ToastContext = createContext<ToastApi | null>(null);

export function useToast(): ToastApi {
  const api = use(ToastContext);
  if (!api) throw new Error('useToast must be used inside <ToastProvider>');
  return api;
}
