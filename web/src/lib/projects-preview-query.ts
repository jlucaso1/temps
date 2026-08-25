import { getProjectsOptions } from '@/api/client/@tanstack/react-query.gen'

/** Page size for the app-wide projects preview. */
export const PROJECTS_PREVIEW_PER_PAGE = 4

/**
 * Shared options for the app-wide projects preview.
 *
 * `ProjectsContext` and `useActivationSignals` both read it, and they must
 * agree on every field. TanStack Query dedupes by query key and the key
 * includes the request params, so a differing page size silently becomes a
 * second `GET /projects` per page load -- which is exactly what used to happen
 * (`per_page: 1` in the activation probe versus `4` in the context).
 *
 * The stale window exists for the same reason. Both observers mount a few
 * hundred milliseconds apart during one page load, and at the default
 * `staleTime: 0` the later one finds the first one's result already stale and
 * refetches it -- one aligned key, still two requests.
 *
 * It costs no freshness. Both observers live for the lifetime of the app, so
 * refetch-on-mount was never what kept this query current; writes are, and
 * every project create flow invalidates `['getProjects']`, which ignores
 * `staleTime`.
 *
 * This lives in its own module rather than beside the context because
 * `react-refresh/only-export-components` requires a component file to export
 * only components.
 */
export function projectsPreviewQueryOptions() {
  return {
    ...getProjectsOptions({
      query: { page: 1, per_page: PROJECTS_PREVIEW_PER_PAGE },
    }),
    staleTime: 30 * 1000,
  }
}
