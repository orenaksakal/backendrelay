// Applies the stored theme before the first paint. The shell loads this file as
// a blocking script so the page never flashes the wrong colours. Keep the key
// and the fallback in sync with src/lib/theme.ts.
(function () {
  var STORAGE_KEY = 'br.theme';
  var theme = 'system';

  try {
    var stored = window.localStorage.getItem(STORAGE_KEY);
    if (stored === 'light' || stored === 'dark' || stored === 'system') {
      theme = stored;
    }
  } catch (error) {
    // Storage can be unavailable (private mode, blocked cookies); use System.
  }

  var prefersDark =
    typeof window.matchMedia === 'function' &&
    window.matchMedia('(prefers-color-scheme: dark)').matches;
  var dark = theme === 'dark' || (theme === 'system' && prefersDark);

  document.documentElement.classList.toggle('dark', dark);
  document.documentElement.style.colorScheme = dark ? 'dark' : 'light';
})();
