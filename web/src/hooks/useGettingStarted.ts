import { useState } from 'react'
import { useActivationSignals } from './useActivationSignals'

const DISMISSED_KEY = 'temps_getting_started_dismissed'

export interface GettingStartedItem {
  key: string
  label: string
  description: string
  done: boolean
  href: string
  cta: string
}

export interface UseGettingStartedOptions {
  /**
   * Force the underlying activation queries to run even when the checklist has
   * been dismissed. The dedicated setup page passes this because it renders the
   * checklist regardless of the dismissal flag; everywhere else the default
   * applies and a dismissed checklist costs zero requests.
   */
  alwaysFetch?: boolean
}

/**
 * Whether the ten activation-signal queries should run.
 *
 * Extracted so the gate itself is covered by a test: getting this wrong is
 * invisible in the UI (a dismissed checklist renders nothing either way) and
 * silently costs ten HTTP requests on every single navigation.
 */
export function shouldFetchActivationSignals(
  dismissed: boolean,
  alwaysFetch: boolean
): boolean {
  return alwaysFetch || !dismissed
}

/**
 * Drives the onboarding checklist: the ordered setup items, how many are done,
 * and whether the checklist should be on screen at all.
 *
 * Dismissal is persisted in `localStorage` and gates the underlying activation
 * queries, so a dismissed checklist costs nothing per navigation. Pass
 * `alwaysFetch` when rendering the checklist regardless of that flag.
 */
export function useGettingStarted(options: UseGettingStartedOptions = {}) {
  const [dismissed, setDismissed] = useState(
    () => localStorage.getItem(DISMISSED_KEY) === 'true'
  )

  // A dismissed checklist renders nothing, so there is no reason to pay for the
  // ten listing requests behind it. Before this gate they fired on every single
  // navigation -- the largest single contributor to the page-load request
  // storm, and completely invisible to the user who had already dismissed it.
  const signals = useActivationSignals({
    enabled: shouldFetchActivationSignals(
      dismissed,
      options.alwaysFetch === true
    ),
  })

  const items: GettingStartedItem[] = [
    {
      key: 'ai',
      label: 'Connect your AI harness',
      description:
        'Create a dedicated admin key, install the Temps skill, and verify access from your harness.',
      done: signals.aiHarnessConfigured,
      href: '/setup/ai',
      cta: 'Connect harness',
    },
    {
      key: 'git',
      label: 'Connect a Git provider',
      description:
        'Link GitHub, GitLab, or Bitbucket to enable git-push deploys.',
      done: signals.gitConnected,
      // Navigate to the full page rather than a modal — the in-modal
      // GitProviderFlow overflows on smaller viewports (horizontal scroll).
      href: '/git-providers/add',
      cta: 'Connect Git',
    },
    // NOTE: "Deploy your first project" is intentionally omitted — this
    // checklist only renders once at least one project exists (the empty-
    // state onboarding owns the no-projects moment), so that step would
    // always be complete and add nothing.
    //
    // Ordered by importance: foundational platform setup that affects every
    // deployment (HTTPS routing, failure alerts, the DNS automation that backs
    // them) comes before per-app extras (databases and their backups), with
    // team collaboration last.
    {
      key: 'domain',
      label: 'Add a wildcard domain',
      description:
        'Point a wildcard DNS record at this server and get HTTPS for all apps.',
      done: signals.wildcardDomainReady,
      href: '/domains/add',
      cta: 'Add domain',
    },
    {
      key: 'notifications',
      label: 'Configure notifications',
      description:
        'Get alerted on Slack, email, or webhook when deployments fail.',
      done: signals.notificationsConfigured,
      href: '/settings/notifications',
      cta: 'Set up',
    },
    {
      key: 'dns',
      label: 'Add a DNS provider',
      description:
        'Connect Cloudflare or Route53 for automatic wildcard SSL certificates.',
      done: signals.dnsProviderConnected,
      href: '/dns-providers/add',
      cta: 'Connect',
    },
    {
      key: 'database',
      label: 'Add a database',
      description:
        'Provision a managed Postgres, Redis, or MongoDB to attach to a project.',
      done: signals.hasDatabase,
      href: '/storage/create',
      cta: 'Add database',
    },
    {
      key: 'backups',
      label: 'Set up backups',
      description:
        'Schedule automatic backups so your databases are protected.',
      done: signals.backupsConfigured,
      href: '/backups',
      cta: 'Set up',
    },
    {
      key: 'team',
      label: 'Invite your team',
      description: 'Add teammates so they can manage projects and deployments.',
      done: signals.teamInvited,
      href: '/settings/users',
      cta: 'Invite',
    },
  ]

  const completedCount = items.filter((i) => i.done).length
  const totalCount = items.length
  const allDone = completedCount === totalCount
  // Only allow dismissing once at least one item is done — prevents new
  // users from instantly hiding the checklist before engaging with it.
  const canDismiss = completedCount >= 1
  const visible = signals.isLoaded && !allDone && !dismissed

  function dismiss() {
    localStorage.setItem(DISMISSED_KEY, 'true')
    setDismissed(true)
  }

  return {
    items,
    completedCount,
    totalCount,
    allDone,
    canDismiss,
    dismissed,
    dismiss,
    visible,
    isLoaded: signals.isLoaded,
  }
}
