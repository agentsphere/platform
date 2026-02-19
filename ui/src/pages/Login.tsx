import { useState } from 'preact/hooks';
import { useAuth } from '../lib/auth';
import { ApiError } from '../lib/api';

export function Login() {
  const { login } = useAuth();
  const [name, setName] = useState('');
  const [password, setPassword] = useState('');
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);

  const submit = async (e: Event) => {
    e.preventDefault();
    setError('');
    setLoading(true);
    try {
      await login(name, password);
      window.location.href = '/';
    } catch (err) {
      setError(err instanceof ApiError ? err.body.error : 'Login failed');
    } finally {
      setLoading(false);
    }
  };

  return (
    <div class="login-page">
      <div class="card login-card">
        <h1 class="login-title">Platform</h1>
        <form onSubmit={submit}>
          <div class="form-group">
            <label>Username</label>
            <input class="input" type="text" value={name}
              onInput={(e) => setName((e.target as HTMLInputElement).value)}
              autoFocus required />
          </div>
          <div class="form-group">
            <label>Password</label>
            <input class="input" type="password" value={password}
              onInput={(e) => setPassword((e.target as HTMLInputElement).value)}
              required />
          </div>
          {error && <div class="error-msg">{error}</div>}
          <button class="btn btn-primary" style="width:100%;margin-top:0.5rem"
            type="submit" disabled={loading}>
            {loading ? 'Signing in...' : 'Sign in'}
          </button>
        </form>
      </div>
    </div>
  );
}
