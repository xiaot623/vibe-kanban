import { useEffect } from 'react';
import { listen } from '@tauri-apps/api/event';

type NotifyPayload = {
  title: string;
  message: string;
};

export function useNotifications() {
  useEffect(() => {
    // Check if we are in Tauri environment
    // @ts-ignore
    const isTauri = !!window.__TAURI_INTERNALS__;

    const showNotification = (title: string, message: string) => {
      if (Notification.permission === 'granted') {
        new Notification(title, { body: message });
      } else if (Notification.permission !== 'denied') {
        Notification.requestPermission().then((permission) => {
          if (permission === 'granted') {
            new Notification(title, { body: message });
          }
        });
      }
    };

    if (isTauri) {
      // Tauri implementation
      let unlisten: (() => void) | undefined;

      const setupListener = async () => {
        try {
          unlisten = await listen<NotifyPayload>('notify', (event) => {
            const { title, message } = event.payload;
            showNotification(title, message);
          });
        } catch (error) {
          console.error('Failed to setup Tauri notification listener', error);
        }
      };

      setupListener();

      return () => {
        if (unlisten) {
          unlisten();
        }
      };
    } else {
      // Browser implementation (SSE)
      const eventSource = new EventSource('/api/events');

      eventSource.addEventListener('notification', (event) => {
        try {
          const data = JSON.parse(event.data);
          showNotification(data.title, data.message);
        } catch (e) {
          console.error('Failed to parse notification event', e);
        }
      });

      return () => {
        eventSource.close();
      };
    }
  }, []);
}
