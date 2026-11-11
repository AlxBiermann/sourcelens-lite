import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { BrowserRouter } from 'react-router';
import '@fontsource-variable/archivo/wdth.css';
import '@fontsource/commit-mono/400.css';
import '@fontsource/commit-mono/700.css';
import './styles/tokens.css';
import './styles/base.css';
import './styles/components.css';
import { App } from './App';
import { ToastProvider } from './components/Toast';
import { SessionProvider } from './hooks/useSession';
import { initTheme } from './theme';

initTheme();

const root = document.getElementById('root');
if (!root) throw new Error('Missing #root element');

createRoot(root).render(
  <StrictMode>
    <BrowserRouter>
      <SessionProvider>
        <ToastProvider>
          <App />
        </ToastProvider>
      </SessionProvider>
    </BrowserRouter>
  </StrictMode>,
);
