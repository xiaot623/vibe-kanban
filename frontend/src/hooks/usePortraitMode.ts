import { useState, useEffect } from 'react';

/**
 * Hook to detect portrait mode (when screen width < height).
 * This is the default condition for mobile mode in kanban.
 */
export function usePortraitMode(): boolean {
  const getIsPortrait = () =>
    typeof window !== 'undefined'
      ? window.innerWidth < window.innerHeight
      : false;

  const [isPortrait, setIsPortrait] = useState(getIsPortrait);

  useEffect(() => {
    if (typeof window === 'undefined') return;

    const handleResize = () => {
      setIsPortrait(getIsPortrait());
    };

    window.addEventListener('resize', handleResize);
    // Also listen for orientation changes on mobile devices
    window.addEventListener('orientationchange', handleResize);

    // Set initial value
    setIsPortrait(getIsPortrait());

    return () => {
      window.removeEventListener('resize', handleResize);
      window.removeEventListener('orientationchange', handleResize);
    };
  }, []);

  return isPortrait;
}
