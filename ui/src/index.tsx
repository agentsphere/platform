import { render } from 'preact';

function App() {
  return (
    <div style={{ padding: '2rem', textAlign: 'center' }}>
      <h1>Platform</h1>
      <p>Unified AI-first platform</p>
    </div>
  );
}

render(<App />, document.getElementById('app')!);
