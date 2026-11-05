import { useEffect } from 'react';
import { useRouter } from '@/lib/navigation';

const Custom404 = () => {
  const router = useRouter();

  useEffect(() => {
    if (!router.isReady) return;

    router.replace('/');
  }, [router.isReady]);

  return null;
};

export default Custom404;
