import React, { createContext, useContext, useEffect, useState } from 'react';
import { ThemeMode } from 'shared/types';

type ThemeProviderProps = {
  children: React.ReactNode;
  initialTheme?: ThemeMode;
};

type ThemeProviderState = {
  theme: ThemeMode;
  resolvedTheme: 'light' | 'dark';
  setTheme: (theme: ThemeMode) => void;
};

const initialState: ThemeProviderState = {
  theme: ThemeMode.SYSTEM,
  resolvedTheme: 'light',
  setTheme: () => null,
};

const ThemeProviderContext = createContext<ThemeProviderState>(initialState);

export function ThemeProvider({
  children,
  initialTheme = ThemeMode.SYSTEM,
  ...props
}: ThemeProviderProps) {
  const [theme, setThemeState] = useState<ThemeMode>(initialTheme);
  const [resolvedTheme, setResolvedTheme] = useState<'light' | 'dark'>(() => {
    if (typeof window === 'undefined') return 'light';
    if (initialTheme === ThemeMode.SYSTEM) {
      return window.matchMedia('(prefers-color-scheme: dark)').matches
        ? 'dark'
        : 'light';
    }
    return initialTheme === ThemeMode.DARK ? 'dark' : 'light';
  });

  // Update theme when initialTheme changes
  useEffect(() => {
    setThemeState(initialTheme);
  }, [initialTheme]);

  useEffect(() => {
    if (theme !== ThemeMode.SYSTEM) {
      setResolvedTheme(theme === ThemeMode.DARK ? 'dark' : 'light');
      return;
    }

    const media = window.matchMedia('(prefers-color-scheme: dark)');
    const updateResolvedTheme = () => {
      setResolvedTheme(media.matches ? 'dark' : 'light');
    };

    updateResolvedTheme();

    if (media.addEventListener) {
      media.addEventListener('change', updateResolvedTheme);
      return () => media.removeEventListener('change', updateResolvedTheme);
    }

    media.addListener(updateResolvedTheme);
    return () => media.removeListener(updateResolvedTheme);
  }, [theme]);

  useEffect(() => {
    const root = window.document.documentElement;
    root.classList.remove('light', 'dark');
    root.classList.add(resolvedTheme);
  }, [resolvedTheme]);

  const setTheme = (newTheme: ThemeMode) => {
    setThemeState(newTheme);
  };

  const value = {
    theme,
    resolvedTheme,
    setTheme,
  };

  return (
    <ThemeProviderContext.Provider {...props} value={value}>
      {children}
    </ThemeProviderContext.Provider>
  );
}

export const useTheme = () => {
  const context = useContext(ThemeProviderContext);

  if (context === undefined)
    throw new Error('useTheme must be used within a ThemeProvider');

  return context;
};
