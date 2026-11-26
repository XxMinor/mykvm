// The backend writes many of its messages in Chinese. In another UI language
// the localized summary leads and the original follows it, so a reader of
// English, Japanese or Korean is not handed Chinese alone.
export function formatUnknownError(error: unknown, fallback: string, chineseUi = true): string {
  const text = describeUnknownError(error, fallback)
  return !chineseUi && text !== fallback && /[\u4e00-\u9fff]/.test(text) ? `${fallback} (${text})` : text
}

function describeUnknownError(error: unknown, fallback: string): string {
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
