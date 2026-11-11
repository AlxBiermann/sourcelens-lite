import { useState } from 'react';
import { nextThemeChoice, setThemeChoice, themeChoice, type ThemeChoice } from '../theme';

const LABEL: Record<ThemeChoice, string> = { system: 'System', light: 'Light', dark: 'Dark' };

export function ThemeToggle() {
  const [choice, setChoice] = useState<ThemeChoice>(themeChoice);
  const next = nextThemeChoice(choice);

  return (
    <button
      type="button"
      className="sl-theme-toggle"
      aria-label={`Theme: ${LABEL[choice]}. Switch to ${LABEL[next]}.`}
      onClick={() => {
        setThemeChoice(next);
        setChoice(next);
      }}
    >
      <span className={`sl-theme-icon sl-theme-icon-${choice}`} aria-hidden="true" />
      <span className="sl-theme-label">{LABEL[choice]} theme</span>
    </button>
  );
}
