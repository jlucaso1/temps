import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import {
  getPlatformSettings,
  updatePlatformSettings as updateSettingsApi,
  type PlatformSettings,
} from '@/api/platformSettings'

// Re-export types for backward compatibility
export type { PlatformSettings } from '@/api/platformSettings'
export type {
  DnsProviderSettings as DnsProvider,
  LetsEncryptSettings as LetsEncrypt,
  ScreenshotSettings as Screenshots,
} from '@/api/client/types.gen'

export interface UseSettingsOptions {
  /**
   * Set to false to keep the query from running. Used by consumers that only
   * need settings when a feature is actually on screen (e.g. the onboarding
   * checklist once it has been dismissed), so they stop contributing a request
   * to every page load.
   */
  enabled?: boolean
}

export function useSettings(options: UseSettingsOptions = {}) {
  return useQuery({
    queryKey: ['platform-settings'],
    queryFn: getPlatformSettings,
    enabled: options.enabled ?? true,
    staleTime: 5 * 60 * 1000, // 5 minutes
    retry: 1,
  })
}

export function useUpdateSettings() {
  const queryClient = useQueryClient()

  return useMutation({
    mutationFn: updateSettingsApi,
    onSuccess: (data) => {
      queryClient.setQueryData(['platform-settings'], data)
      queryClient.invalidateQueries({ queryKey: ['platform-settings'] })
    },
    onError: (error) => {
      toast.error('Failed to update settings', {
        description: error instanceof Error ? error.message : 'Unknown error',
      })
    },
  })
}

// Export for backwards compatibility
export type Settings = PlatformSettings
