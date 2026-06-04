import { render } from 'preact';
import { App } from './App';
import { initAuth, clearToken } from '@/auth';
import './styles/global.css';

initAuth(() => {
  clearToken();
  window.location.reload();
});

render(<App />, document.getElementById('app')!);
