import test from 'node:test'
import assert from 'node:assert/strict'
import { formatUnknownError } from './errors.ts'

test('preserves Tauri string rejections and JavaScript errors', () => {
  const detail = 'MoveFileExW(protected input helper) failed with Windows error 32'
  assert.equal(formatUnknownError(detail, 'Operation failed'), detail)
  assert.equal(formatUnknownError(new Error(detail), 'Operation failed'), detail)
})

test('keeps structured backend errors readable', () => {
  assert.equal(
    formatUnknownError({ message: 'Access denied', code: 5 }, 'Operation failed'),
    '{"message":"Access denied","code":5}',
  )
})

test('uses the localized fallback for missing or unreadable errors', () => {
  const circular: { cause?: unknown } = {}
  circular.cause = circular
  for (const error of [null, undefined, '', '  ', new Error(''), circular]) {
    assert.equal(formatUnknownError(error, '操作失败'), '操作失败')
  }
})
