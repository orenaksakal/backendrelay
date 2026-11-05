import { useIsHydrated } from '@/lib/hooks/useIsHydrated';
import { useRouter } from '@/lib/navigation';

export function useReadyRouter() {
  const router = useRouter();
  if (!router.isReady) {
    throw new Error(
      'Router was not ready. Render this inside a client-only page.',
    );
  }
  return router;
}

export function asClientOnlyPage<Props extends JSX.IntrinsicAttributes>(
  Component: React.ComponentType<Props>,
) {
  return function ClientOnlyPage(props: Props) {
    const isHydrated = useIsHydrated();
    const router = useRouter();
    if (!isHydrated) {
      return null;
    }
    if (!router.isReady) {
      return null;
    }

    return <Component {...props} />;
  };
}

export const ClientOnly: React.FC<{
  children: React.ReactNode;
}> = ({ children }) => {
  const isHydrated = useIsHydrated();
  const router = useRouter();
  if (!isHydrated) {
    return null;
  }
  if (!router.isReady) {
    return null;
  }
  return <>{children}</>;
};
