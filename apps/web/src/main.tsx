import { render } from 'preact';
import { App } from './App';
import { initAuth, clearToken } from '@/auth';
import './styles/global.css';

initAuth(() => {
  clearToken();
  window.location.reload();
});

const root = document.getElementById('app');
if (root === null) throw new Error('Root element #app not found');
render(<App />, root);
