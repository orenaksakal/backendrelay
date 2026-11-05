import { createContext, useContext } from 'react';

export type Theme = 'system' | 'light' | 'dark';
export type ResolvedTheme = 'light' | 'dark';

/** Shared with public/theme-init.js, which applies the theme before paint. */
export const THEME_STORAGE_KEY = 'br.theme';

export interface ThemeContextValue {
  /** The preference the user picked: System, Light or Dark. */
  theme: Theme;
  /** The theme actually applied: System resolves to the OS preference. */
  resolvedTheme: ResolvedTheme;
  setTheme: (theme: Theme) => void;
}

export const ThemeContext = createContext<ThemeContextValue | null>(null);

export function isTheme(value: unknown): value is Theme {
  return value === 'system' || value === 'light' || value === 'dark';
}

export function readStoredTheme(): Theme | undefined {
  try {
    const stored = window.localStorage.getItem(THEME_STORAGE_KEY);
    return isTheme(stored) ? stored : undefined;
  } catch {
    // Storage can be unavailable (private mode, blocked cookies).
    return undefined;
  }
}

export function getSystemTheme(): ResolvedTheme {
  try {
    return window.matchMedia('(prefers-color-scheme: dark)').matches
      ? 'dark'
      : 'light';
  } catch {
    // No DOM (prerender) or no matchMedia support; assume light.
    return 'light';
  }
}

export function resolveTheme(
  theme: Theme,
  systemTheme: ResolvedTheme,
): ResolvedTheme {
  return theme === 'system' ? systemTheme : theme;
}

export function applyTheme(theme: ResolvedTheme): void {
  const root = document.documentElement;
  root.classList.toggle('dark', theme === 'dark');
  // Mirrors public/theme-init.js so a later toggle also updates UA colours.
  root.style.colorScheme = theme;
}

export function useTheme(): ThemeContextValue {
  const context = useContext(ThemeContext);
  if (!context) {
    throw new Error('useTheme must be used inside <ThemeProvider>');
  }
  return context;
}
