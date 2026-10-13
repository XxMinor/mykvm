export function formatUnknownError(error: unknown, fallback: string): string {
  if (error instanceof Error) {
    return error.message.trim() ? error.message : fallback
  }
  if (typeof error === 'string') {
    return error.trim() ? error : fallback
  }

  try {
    const text = JSON.stringify(error)
    return text && text !== 'null' ? text : fallback
  } catch {
    return fallback
  }
}
