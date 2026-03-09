import { render } from 'preact';
import Router from 'preact-router';
import { useState, useEffect } from 'preact/hooks';
import { AuthProvider, useAuth } from './lib/auth';
import { Layout } from './components/Layout';
import { ErrorBoundary } from './components/ErrorBoundary';
import { ToastProvider } from './components/Toast';
import { Login } from './pages/Login';
import { Setup } from './pages/Setup';
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
import { Health } from './pages/admin/Health';
import { Tokens } from './pages/admin/Tokens';
import { ProviderKeys } from './pages/ProviderKeys';
import { AccountSettings } from './pages/AccountSettings';
import { Workspaces } from './pages/Workspaces';
import { WorkspaceDetail } from './pages/WorkspaceDetail';
import { CreateApp } from './pages/CreateApp';
import { OnboardingProvider } from './lib/onboarding';
import { OnboardingOverlay } from './components/OnboardingOverlay';

function AppRouter() {
  const { user, loading } = useAuth();
  const [needsSetup, setNeedsSetup] = useState<boolean | null>(null);

  useEffect(() => {
    fetch('/api/setup/status')
      .then(r => r.json())
      .then(d => setNeedsSetup(d.needs_setup))
      .catch(() => setNeedsSetup(false));
  }, []);

  if (loading || needsSetup === null) return <div class="loading">Loading...</div>;
  if (needsSetup) return <Setup />;
  if (!user) return <Login />;

  return (
    <OnboardingProvider>
      <Layout>
        <OnboardingOverlay />
        <Router>
          <Dashboard path="/" />
          <CreateApp path="/create-app" />
          <Workspaces path="/workspaces" />
          <WorkspaceDetail path="/workspaces/:id" />
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
          <Health path="/admin/health" />
          <AccountSettings path="/settings/account" />
          <Tokens path="/settings/tokens" />
          <ProviderKeys path="/settings/provider-keys" />
        </Router>
      </Layout>
    </OnboardingProvider>
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
