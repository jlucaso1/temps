import { describe, expect, test } from 'bun:test'
import { getProjectsOptions } from '@/api/client/@tanstack/react-query.gen'
import {
  PROJECTS_PREVIEW_PER_PAGE,
  projectsPreviewQueryOptions,
} from '@/lib/projects-preview-query'
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
  test('the shared options key differs from the old per_page: 1 probe', () => {
    // TanStack Query dedupes by query key, and the key includes the request
    // params. When the two call sites drift apart (they used to: per_page 1 vs
    // 4) the console issues two near-identical GET /projects per page load.
    const shared = projectsPreviewQueryOptions().queryKey

    expect(shared).toEqual(
      getProjectsOptions({
        query: { page: 1, per_page: PROJECTS_PREVIEW_PER_PAGE },
      }).queryKey
    )
    expect(
      getProjectsOptions({ query: { page: 1, per_page: 1 } }).queryKey
    ).not.toEqual(shared)
  })

  test('the shared options carry a stale window, so one load is one request', () => {
    // Aligning the key is not enough on its own: the two observers mount a few
    // hundred ms apart, and at staleTime 0 the later one refetches immediately.
    expect(projectsPreviewQueryOptions().staleTime).toBeGreaterThan(0)
  })
})
