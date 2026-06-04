import { render } from 'preact';
import { App } from './App';
import { init401Interceptor } from '@/auth';
import './styles/global.css';

const cleanup = init401Interceptor(() => {
  // On 401, reload to force re-evaluation of the auth state and redirect to login.
  window.location.reload();
});

render(<App />, document.getElementById('app')!);

// Clean up interceptor on page hide.
document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'hidden') {
    cleanup();
  }
});
