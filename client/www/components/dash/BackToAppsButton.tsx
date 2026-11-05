import { ArrowUturnLeftIcon } from '@heroicons/react/24/outline';
import Link from '@/lib/navigation';
import { createPortal } from 'react-dom';

export const BackToAppsButton = () => {
  const element = document.getElementById('left-top-bar')!;
  if (!element) {
    return null;
  }
  return createPortal(
    <Link
      className="text-ui-muted hover:bg-ui-subtle ml-4 flex items-center gap-2 rounded-xs p-1 px-2 text-sm transition-colors"
      href="/dash"
    >
      <ArrowUturnLeftIcon width={12} />
      Back to Apps
    </Link>,
    element,
  );
};
