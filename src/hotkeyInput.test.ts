import test from 'node:test';
import assert from 'node:assert/strict';
import { hotkeyFromKeyboardEvent, metaKeyLabelForPlatform } from './hotkeyInput.ts';

const event = (key: string, modifiers = {}) => ({ key, ctrlKey: false, altKey: false, shiftKey: false, metaKey: false, ...modifiers });

test('recorders require the configured modifiers and never bind a bare arrow', () => {
  for (const key of ['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown']) {
    assert.equal(hotkeyFromKeyboardEvent(event(key)), null);
    assert.equal(hotkeyFromKeyboardEvent(event(key, {altKey: true})), `alt+${key.slice(5).toLowerCase()}`);
  }
  assert.equal(hotkeyFromKeyboardEvent(event('k', {altKey: true, shiftKey: true})), 'alt+shift+k');
  assert.equal(hotkeyFromKeyboardEvent(event('Alt', {altKey: true})), null);
});

test('only bare delete keys clear a binding, while modifier and platform labels stay explicit', () => {
  for (const key of ['Backspace', 'Delete']) {
    assert.equal(hotkeyFromKeyboardEvent(event(key)), 'disabled');
    assert.equal(hotkeyFromKeyboardEvent(event(key, {ctrlKey: true})), null);
  }
  assert.equal(hotkeyFromKeyboardEvent(event('k', {metaKey: true}), metaKeyLabelForPlatform('Windows')), 'win+k');
  assert.equal(hotkeyFromKeyboardEvent(event('k', {metaKey: true}), metaKeyLabelForPlatform('macOS')), 'command+k');
  assert.equal(hotkeyFromKeyboardEvent(event('F24')), 'f24');
});
