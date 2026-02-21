import { createContext } from 'preact';
import { useState, useContext, useCallback } from 'preact/hooks';

export type ToastType = 'success' | 'error' | 'info';

interface ToastItem {
  id: number;
  message: string;
  type: ToastType;
}

interface ToastContext {
  addToast: (message: string, type?: ToastType) => void;
}

const ToastCtx = createContext<ToastContext>({ addToast: () => {} });

let nextId = 0;

export function ToastProvider({ children }: { children: any }) {
  const [toasts, setToasts] = useState<ToastItem[]>([]);

  const addToast = useCallback((message: string, type: ToastType = 'info') => {
    const id = ++nextId;
    setToasts(prev => [...prev, { id, message, type }]);
    setTimeout(() => {
      setToasts(prev => prev.filter(t => t.id !== id));
    }, 5000);
  }, []);

  const dismiss = (id: number) => {
    setToasts(prev => prev.filter(t => t.id !== id));
  };

  return (
    <ToastCtx.Provider value={{ addToast }}>
      {children}
      <div class="toast-container">
        {toasts.map(t => (
          <div key={t.id} class={`toast toast-${t.type}`} onClick={() => dismiss(t.id)}>
            {t.message}
          </div>
        ))}
      </div>
    </ToastCtx.Provider>
  );
}

export function useToast(): ToastContext {
  return useContext(ToastCtx);
}
