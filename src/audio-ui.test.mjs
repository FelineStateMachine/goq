import assert from 'node:assert/strict';
import test from 'node:test';

import { audioButtonPresentation } from './audio-ui.mjs';

test('audio button uses a speaker glyph with a mute action while playing', () => {
  assert.deepEqual(audioButtonPresentation({
    available: true,
    muted: false,
    state: 'playing',
    detail: 'bounded Opus playback',
  }), {
    glyph: '🔊',
    ariaLabel: 'Mute audio. Audio playing. bounded Opus playback',
    title: 'Audio playing: bounded Opus playback',
  });
});

test('audio button uses a muted glyph with an unmute action while muted', () => {
  const presentation = audioButtonPresentation({
    available: true,
    muted: true,
    state: 'playing',
    detail: 'bounded Opus playback',
  });
  assert.equal(presentation.glyph, '🔇');
  assert.match(presentation.ariaLabel, /^Unmute audio\. Audio muted\./);
});

test('audio button distinguishes priming from unavailable', () => {
  assert.equal(audioButtonPresentation({
    available: true,
    muted: false,
    state: 'priming',
    detail: 'Waiting for bounded Opus prebuffer',
  }).glyph, 'audio ...');
  const unavailable = audioButtonPresentation({
    available: false,
    muted: false,
    state: 'unavailable',
    detail: 'not connected',
  });
  assert.equal(unavailable.glyph, '🔇');
  assert.match(unavailable.ariaLabel, /^Audio unavailable\./);
});
