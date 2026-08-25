import { useQuery } from '@tanstack/react-query'
import {
  listConnectionsOptions,
  listDomainsOptions,
  getProjectsOptions,
  listNotificationProvidersOptions,
  listServicesOptions,
  listBackupSchedulesOptions,
  listDnsProvidersOptions,
  listUsersOptions,
  getAiProviderStatusOptions,
  listApiKeysOptions,
} from '@/api/client/@tanstack/react-query.gen'
import { PROJECTS_PREVIEW_PER_PAGE } from '@/contexts/ProjectsContext'
import { useSettings } from './useSettings'
import { SIMULATE_EMPTY_INSTALL } from '@/lib/devSimulate'
import { getAiHarnessStatus } from '@/lib/ai-onboarding'

export interface ActivationSignals {
  /** Dedicated active admin key has authenticated an external AI harness */
  aiHarnessConfigured: boolean
  /** Active provider can power Temps' built-in chat and AI features */
  aiProviderConfigured: boolean
  /** Git provider connected and active */
  gitConnected: boolean
  /** At least one active wildcard domain in the database */
  wildcardDomainReady: boolean
  /** At least one project created */
  hasProject: boolean
  /** external_url is configured in settings */
  externalUrlSet: boolean
  /** At least one enabled notification provider */
  notificationsConfigured: boolean
  /** At least one managed database/service created */
  hasDatabase: boolean
  /** At least one backup schedule configured */
  backupsConfigured: boolean
  /** At least one DNS provider connected */
  dnsProviderConnected: boolean
  /** More than just the initial admin — i.e. a teammate was invited */
  teamInvited: boolean
  /** True once all signals are loaded (used to avoid flash of incomplete state) */
  isLoaded: boolean
  /** Number of completed items out of total */
  completedCount: number
  totalCount: number
}

const TOTAL = 10

export interface UseActivationSignalsOptions {
  /**
   * When false, none of the ten listing queries behind these signals run.
   * Callers that only render the checklist when it is visible (see
   * `useGettingStarted`) pass `false` once it has been dismissed -- that is
   * what keeps a dismissed checklist from costing ten HTTP requests on every
   * navigation.
   */
  enabled?: boolean
}

/**
 * The ten "has the operator set this up yet" signals behind the onboarding
 * checklist, each derived from a listing endpoint.
 *
 * Disabled (`{ enabled: false }`) this runs no queries at all: every signal
 * comes back `false` and `isLoaded` stays `false`, so a caller can tell "not
 * fetched" apart from "fetched, nothing configured". Callers that render the
 * checklist conditionally should disable it rather than ignore the result --
 * the queries cost ten HTTP requests per page load either way.
 */
export function useActivationSignals(
  options: UseActivationSignalsOptions = {}
): ActivationSignals {
  const enabled = options.enabled ?? true

  const { data: settings, isLoading: settingsLoading } = useSettings({
    enabled,
  })

  const { data: connections, isLoading: connectionsLoading } = useQuery({
    ...listConnectionsOptions({}),
    enabled,
    retry: false,
  })

  const { data: domainsData, isLoading: domainsLoading } = useQuery({
    ...listDomainsOptions({ query: { page_size: 50 } }),
    enabled,
    retry: false,
  })

  // Deliberately the SAME page size `ProjectsContext` asks for. TanStack Query
  // dedupes by query key and the key includes the params, so the old
  // `per_page: 1` here produced a second, near-identical `GET /projects` on
  // every page load purely because the numbers differed. All this needs is "is
  // there at least one project", which the first page of the list answers.
  const { data: projectsData, isLoading: projectsLoading } = useQuery({
    ...getProjectsOptions({
      query: { page: 1, per_page: PROJECTS_PREVIEW_PER_PAGE },
    }),
    enabled,
    retry: false,
  })

  const { data: providersData, isLoading: providersLoading } = useQuery({
    ...listNotificationProvidersOptions({}),
    enabled,
    retry: false,
  })

  const { data: servicesData, isLoading: servicesLoading } = useQuery({
    ...listServicesOptions({}),
    enabled,
    retry: false,
  })

  const { data: backupSchedulesData, isLoading: backupSchedulesLoading } =
    useQuery({
      ...listBackupSchedulesOptions({}),
      enabled,
      retry: false,
    })

  const { data: dnsProvidersData, isLoading: dnsProvidersLoading } = useQuery({
    ...listDnsProvidersOptions({}),
    enabled,
    retry: false,
  })

  const { data: usersData, isLoading: usersLoading } = useQuery({
    ...listUsersOptions({ query: { include_deleted: false } }),
    enabled,
    retry: false,
  })

  const { data: aiProviderStatus, isLoading: aiProviderStatusLoading } =
    useQuery({
      ...getAiProviderStatusOptions(),
      enabled,
      retry: false,
    })

  const { data: apiKeysData, isLoading: apiKeysLoading } = useQuery({
    ...listApiKeysOptions({ query: { page: 1, page_size: 100 } }),
    enabled,
    retry: false,
  })

  // TEMP: pretend nothing is set up yet, for building the first-run
  // experience. See lib/devSimulate.ts. Placed after all hooks so hook order
  // stays stable.
  if (SIMULATE_EMPTY_INSTALL) {
    return {
      aiHarnessConfigured: false,
      aiProviderConfigured: false,
      gitConnected: false,
      wildcardDomainReady: false,
      hasProject: false,
      externalUrlSet: false,
      notificationsConfigured: false,
      hasDatabase: false,
      backupsConfigured: false,
      dnsProviderConnected: false,
      teamInvited: false,
      isLoaded: true,
      completedCount: 0,
      totalCount: TOTAL,
    }
  }

  // With the queries disabled nothing is in flight, so every `isLoading` is
  // false -- reporting "loaded" from that would be a lie, and callers use
  // `isLoaded` to decide whether the checklist may render. A disabled hook has
  // no signals to show, so it is never loaded.
  const isLoaded =
    enabled &&
    !settingsLoading &&
    !connectionsLoading &&
    !domainsLoading &&
    !projectsLoading &&
    !providersLoading &&
    !servicesLoading &&
    !backupSchedulesLoading &&
    !dnsProvidersLoading &&
    !usersLoading &&
    !aiProviderStatusLoading &&
    !apiKeysLoading

  const aiProviderConfigured = aiProviderStatus?.configured === true
  const aiHarnessConfigured =
    getAiHarnessStatus(apiKeysData?.api_keys) === 'connected'

  const gitConnected =
    (connections?.connections?.filter((c) => c.is_active).length ?? 0) > 0

  const wildcardDomainReady =
    domainsData?.domains?.some((d) => d.is_wildcard && d.status === 'active') ??
    false

  const hasProject = (projectsData?.projects?.length ?? 0) > 0

  const externalUrlSet = !!settings?.external_url

  const notificationsConfigured =
    Array.isArray(providersData) && providersData.some((p) => p.enabled)

  const hasDatabase = (servicesData?.length ?? 0) > 0

  const backupsConfigured = (backupSchedulesData?.length ?? 0) > 0

  const dnsProviderConnected = (dnsProvidersData?.length ?? 0) > 0

  // The initial admin always exists, so "team invited" means more than one
  // active user.
  const teamInvited = (usersData?.length ?? 0) > 1

  const completed = [
    aiHarnessConfigured,
    gitConnected,
    wildcardDomainReady,
    hasProject,
    externalUrlSet,
    notificationsConfigured,
    hasDatabase,
    backupsConfigured,
    dnsProviderConnected,
    teamInvited,
  ].filter(Boolean).length

  return {
    aiHarnessConfigured,
    aiProviderConfigured,
    gitConnected,
    wildcardDomainReady,
    hasProject,
    externalUrlSet,
    notificationsConfigured,
    hasDatabase,
    backupsConfigured,
    dnsProviderConnected,
    teamInvited,
    isLoaded,
    completedCount: completed,
    totalCount: TOTAL,
  }
}
