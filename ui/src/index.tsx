import { render } from 'preact';
import Router from 'preact-router';
import { AuthProvider, useAuth } from './lib/auth';
import { Layout } from './components/Layout';
import { Login } from './pages/Login';
import { Dashboard } from './pages/Dashboard';
import { Projects } from './pages/Projects';
import { ProjectDetail } from './pages/ProjectDetail';
import { IssueDetail } from './pages/IssueDetail';
import { MRDetail } from './pages/MRDetail';
import { PipelineDetail } from './pages/PipelineDetail';
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
    <AuthProvider>
      <AppRouter />
    </AuthProvider>
  );
}

render(<App />, document.getElementById('app')!);
