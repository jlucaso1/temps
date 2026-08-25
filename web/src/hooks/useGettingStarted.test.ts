import { describe, expect, test } from 'bun:test'
import { getProjectsOptions } from '@/api/client/@tanstack/react-query.gen'
import { PROJECTS_PREVIEW_PER_PAGE } from '@/contexts/ProjectsContext'
import { shouldFetchActivationSignals } from './useGettingStarted'

describe('shouldFetchActivationSignals', () => {
  test('fetches for a user who has not dismissed the checklist', () => {
    expect(shouldFetchActivationSignals(false, false)).toBe(true)
  })

  test('a dismissed checklist costs no requests', () => {
    // The checklist renders nothing once dismissed, so the ten listing queries
    // behind it were pure waste on every navigation.
    expect(shouldFetchActivationSignals(true, false)).toBe(false)
  })

  test('the setup page opts back in even when dismissed', () => {
    expect(shouldFetchActivationSignals(true, true)).toBe(true)
    expect(shouldFetchActivationSignals(false, true)).toBe(true)
  })
})

describe('projects query deduplication', () => {
  test('the activation probe and the app-wide preview share a query key', () => {
    // TanStack Query dedupes by query key, and the key includes the request
    // params. When these two drift apart (they used to: per_page 1 vs 4) the
    // console issues two near-identical GET /projects on every page load.
    const preview = getProjectsOptions({
      query: { page: 1, per_page: PROJECTS_PREVIEW_PER_PAGE },
    }).queryKey
    const probe = getProjectsOptions({
      query: { page: 1, per_page: PROJECTS_PREVIEW_PER_PAGE },
    }).queryKey

    expect(probe).toEqual(preview)
    expect(
      getProjectsOptions({ query: { page: 1, per_page: 1 } }).queryKey
    ).not.toEqual(preview)
  })
})
