import { Component } from 'preact';

interface Props {
  children: any;
}

interface State {
  hasError: boolean;
  error: Error | null;
}

export class ErrorBoundary extends Component<Props, State> {
  constructor(props: Props) {
    super(props);
    this.state = { hasError: false, error: null };
  }

  static getDerivedStateFromError(error: Error): State {
    return { hasError: true, error };
  }

  componentDidCatch(error: Error) {
    console.error('ErrorBoundary caught:', error);
  }

  render() {
    if (this.state.hasError) {
      return (
        <div class="error-boundary">
          <div class="error-boundary-content">
            <h2>Something went wrong</h2>
            <p class="text-muted">An unexpected error occurred. Please try again.</p>
            {this.state.error && (
              <pre class="error-boundary-detail">{this.state.error.message}</pre>
            )}
            <button class="btn btn-primary mt-md"
              onClick={() => {
                this.setState({ hasError: false, error: null });
                window.location.reload();
              }}>
              Reload
            </button>
          </div>
        </div>
      );
    }
    return this.props.children;
  }
}
