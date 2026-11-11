export type ThemeChoice = 'system' | 'light' | 'dark';

const STORAGE_KEY = 'sourcelens.theme';
const media = window.matchMedia('(prefers-color-scheme: dark)');

export function themeChoice(): ThemeChoice {
  try {
    const stored = window.localStorage.getItem(STORAGE_KEY);
    if (stored === 'light' || stored === 'dark') return stored;
  } catch {
    // Storage can be unavailable (private mode); fall back to the system theme.
  }
  return 'system';
}

function apply(choice: ThemeChoice) {
  const resolved = choice === 'system' ? (media.matches ? 'dark' : 'light') : choice;
  document.documentElement.dataset.theme = resolved;
}

export function setThemeChoice(choice: ThemeChoice) {
  try {
    if (choice === 'system') window.localStorage.removeItem(STORAGE_KEY);
    else window.localStorage.setItem(STORAGE_KEY, choice);
  } catch {
    // Not persisted; the choice still applies to this page.
  }
  apply(choice);
}

export function nextThemeChoice(choice: ThemeChoice): ThemeChoice {
  return choice === 'system' ? 'light' : choice === 'light' ? 'dark' : 'system';
}

export function initTheme() {
  apply(themeChoice());
  media.addEventListener('change', () => {
    if (themeChoice() === 'system') apply('system');
  });
}
