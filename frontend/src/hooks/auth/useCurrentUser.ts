import { useQuery } from '@tanstack/react-query';
import { oauthApi } from '@/lib/api';

export function useCurrentUser() {
  const query = useQuery({
    queryKey: ['auth', 'user'],
    queryFn: () => oauthApi.status(),
    retry: 2,
    staleTime: 5 * 60 * 1000, // 5 minutes
    refetchOnWindowFocus: false,
    refetchOnReconnect: false,
  });

  return query;
}
