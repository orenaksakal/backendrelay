import {
  Component,
  Suspense,
  lazy,
  useState,
  type ComponentType,
  type ComponentProps,
  type PropsWithChildren,
} from 'react';

class LoadBoundary extends Component<
  PropsWithChildren<{ retry: () => void }>,
  { failed: boolean }
> {
  state = { failed: false };
  static getDerivedStateFromError() {
    return { failed: true };
  }
  render() {
    if (this.state.failed)
      return (
        <section className="shell-load-error" role="alert">
          <h2>This page could not load</h2>
          <p>
            Check your connection and try again. Reload the dashboard if a new
            version was deployed.
          </p>
          <div className="wb-actions">
            <button onClick={this.props.retry}>Retry loading page</button>
            <button onClick={() => window.location.reload()}>
              Reload dashboard
            </button>
          </div>
        </section>
      );
    return this.props.children;
  }
}

/** Reset both the boundary and lazy import after a failed chunk load. */
export function lazyDashboard<T extends ComponentType<any>>(
  load: () => Promise<{ default: T }>,
) {
  const Initial = lazy(load);
  return function LazyDashboard(props: ComponentProps<T>) {
    const [{ attempt, Content }, retry] = useState<{
      attempt: number;
      Content: ComponentType<any>;
    }>(() => ({ attempt: 0, Content: Initial }));
    return (
      <LoadBoundary
        key={attempt}
        retry={() =>
          retry((state) => ({
            attempt: state.attempt + 1,
            Content: lazy(load),
          }))
        }
      >
        <Suspense
          fallback={
            <div className="wb-empty" role="status" aria-live="polite">
              Loading page…
            </div>
          }
        >
          <Content {...props} />
        </Suspense>
      </LoadBoundary>
    );
  };
}
