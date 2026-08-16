export const diagnosticLevels = ['INFO', 'WARN', 'ERROR'] as const
export type DiagnosticLevel = typeof diagnosticLevels[number]

export type DiagnosticEntry = {
  date: string
  time: string
  level: DiagnosticLevel
  target: string
  message: string
}

export type DiagnosticReport = {
  entries: DiagnosticEntry[]
  emailAvailable: boolean
}

export const reportWindow = (entries: readonly DiagnosticEntry[]): DiagnosticEntry[] => {
  let lastProblem = -1
  entries.forEach((entry, index) => {
    if (entry.level === 'WARN' || entry.level === 'ERROR') lastProblem = index
  })
  return lastProblem < 0 ? [] : entries.slice(0, lastProblem + 1)
}

export const formatDiagnosticReport = (entries: readonly DiagnosticEntry[]): string => entries
  .map((entry) => `[${entry.date}][${entry.time}][${entry.level}][${entry.target}] ${entry.message}`)
  .join('\n')
