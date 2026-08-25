import { ProjectResponse } from '@/api/client'
import { getProjectsOptions } from '@/api/client/@tanstack/react-query.gen'
import { useQuery } from '@tanstack/react-query'
import { createContext, useContext } from 'react'

interface ProjectsContextType {
  projects: ProjectResponse[]
  isLoading: boolean
}

const ProjectsContext = createContext<ProjectsContextType>({
  projects: [],
  isLoading: false,
})

/**
 * Page size for the app-wide projects preview.
 *
 * Exported because `useActivationSignals` reuses it: TanStack Query dedupes by
 * query key and the key includes the params, so any other page size there
 * silently becomes a second `GET /projects` on every page load. Keep the two
 * call sites reading this constant.
 */
export const PROJECTS_PREVIEW_PER_PAGE = 4

export function ProjectsProvider({ children }: { children: React.ReactNode }) {
  const { data, isLoading } = useQuery({
    ...getProjectsOptions({
      query: {
        page: 1,
        per_page: PROJECTS_PREVIEW_PER_PAGE,
      },
    }),
  })

  return (
    <ProjectsContext.Provider
      value={{ projects: data?.projects || [], isLoading }}
    >
      {children}
    </ProjectsContext.Provider>
  )
}

export function useProjects() {
  return useContext(ProjectsContext)
}
