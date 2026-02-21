import { render } from 'preact';
import Router from 'preact-router';
import { AuthProvider, useAuth } from './lib/auth';
import { Layout } from './components/Layout';
import { ErrorBoundary } from './components/ErrorBoundary';
import { ToastProvider } from './components/Toast';
import { Login } from './pages/Login';
import { Dashboard } from './pages/Dashboard';
import { Projects } from './pages/Projects';
import { ProjectDetail } from './pages/ProjectDetail';
import { IssueDetail } from './pages/IssueDetail';
import { MRDetail } from './pages/MRDetail';
import { PipelineDetail } from './pages/PipelineDetail';
import { SessionDetail } from './pages/SessionDetail';
import { Logs } from './pages/observe/Logs';
import { Traces, TraceDetail } from './pages/observe/Traces';
import { Metrics } from './pages/observe/Metrics';
import { Alerts } from './pages/observe/Alerts';
import { Users } from './pages/admin/Users';
import { Roles } from './pages/admin/Roles';
import { Delegations } from './pages/admin/Delegations';
import { Tokens } from './pages/admin/Tokens';

function AppRouter() {
  const { user, loading } = useAuth();

  if (loading) return <div class="loading">Loading...</div>;
  if (!user) return <Login />;

  return (
    <Layout>
      <Router>
        <Dashboard path="/" />
        <Projects path="/projects" />
        <ProjectDetail path="/projects/:id/:tab?" />
        <IssueDetail path="/projects/:id/issues/:number" />
        <MRDetail path="/projects/:id/merge-requests/:number" />
        <PipelineDetail path="/projects/:id/pipelines/:pipelineId" />
        <SessionDetail path="/projects/:id/sessions/:sessionId" />
        <Logs path="/observe/logs" />
        <Traces path="/observe/traces" />
        <TraceDetail path="/observe/traces/:traceId" />
        <Metrics path="/observe/metrics" />
        <Alerts path="/observe/alerts" />
        <Users path="/admin/users" />
        <Roles path="/admin/roles" />
        <Delegations path="/admin/delegations" />
        <Tokens path="/settings/tokens" />
      </Router>
    </Layout>
  );
}

function App() {
  return (
    <ErrorBoundary>
      <AuthProvider>
        <ToastProvider>
          <AppRouter />
        </ToastProvider>
      </AuthProvider>
    </ErrorBoundary>
  );
}

render(<App />, document.getElementById('app')!);
