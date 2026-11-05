import { useEffect, useState } from 'react';

/**
 * Reactive `matchMedia` for layout choices CSS cannot make on its own, such as
 * the Playground's horizontal panes becoming a stack below `lg` (B.3).
 */
export function useMediaQuery(query: string): boolean {
  const [matches, setMatches] = useState(() =>
    typeof window === 'undefined' ? false : window.matchMedia(query).matches,
  );

  useEffect(() => {
    const list = window.matchMedia(query);
    const update = () => {
      setMatches(list.matches);
    };
    update();
    list.addEventListener('change', update);
    return () => {
      list.removeEventListener('change', update);
    };
  }, [query]);

  return matches;
}
