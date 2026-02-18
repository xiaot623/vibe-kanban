import { useCurrentUser } from './useCurrentUser';

export function useAuth() {
  const { data: status, isLoading } = useCurrentUser();

  return {
    isSignedIn: status?.logged_in ?? false,
    isLoaded: !isLoading && status !== undefined,
    userId: status?.profile?.user_id ?? null,
  };
}
