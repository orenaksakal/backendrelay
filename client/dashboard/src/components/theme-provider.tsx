import {
  useCallback,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from 'react';
import {
  THEME_STORAGE_KEY,
  ThemeContext,
  applyTheme,
  getSystemTheme,
  readStoredTheme,
  resolveTheme,
  type ResolvedTheme,
  type Theme,
  type ThemeContextValue,
} from '@/lib/theme';

export interface ThemeProviderProps {
  children: ReactNode;
  /** Preference used before anything is stored. */
  defaultTheme?: Theme;
}

/**
 * System / Light / Dark theme state, persisted in localStorage `br.theme`.
 *
 * `public/theme-init.js` applies the same preference before first paint; this
 * provider takes over afterwards and follows the OS preference while set to
 * System.
 */
export function ThemeProvider({
  children,
  defaultTheme = 'system',
}: ThemeProviderProps) {
  const [theme, setThemeState] = useState<Theme>(
    () => readStoredTheme() ?? defaultTheme,
  );
  const [systemTheme, setSystemTheme] = useState<ResolvedTheme>(() =>
    getSystemTheme(),
  );
  const resolvedTheme = resolveTheme(theme, systemTheme);

  useEffect(() => {
    applyTheme(resolvedTheme);
  }, [resolvedTheme]);

  useEffect(() => {
    const query = window.matchMedia('(prefers-color-scheme: dark)');
    const onChange = () => {
      setSystemTheme(query.matches ? 'dark' : 'light');
    };

    query.addEventListener('change', onChange);
    return () => {
      query.removeEventListener('change', onChange);
    };
  }, []);

  const setTheme = useCallback((next: Theme) => {
    setThemeState(next);
    try {
      window.localStorage.setItem(THEME_STORAGE_KEY, next);
    } catch {
      // Storage can be unavailable; the theme still applies to this session.
    }
  }, []);

  const value = useMemo<ThemeContextValue>(
    () => ({ theme, resolvedTheme, setTheme }),
    [theme, resolvedTheme, setTheme],
  );

  return (
    <ThemeContext.Provider value={value}>{children}</ThemeContext.Provider>
  );
}
