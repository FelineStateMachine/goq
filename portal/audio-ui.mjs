export function audioButtonPresentation({ available, muted, state, detail }) {
  const normalizedState = typeof state === 'string' && state.length > 0 ? state : 'unavailable';
  const normalizedDetail = typeof detail === 'string' && detail.length > 0
    ? detail
    : normalizedState;

  let glyph;
  if (!available || muted || normalizedState === 'error' || normalizedState === 'unavailable') {
    glyph = '🔇';
  } else if (normalizedState === 'playing') {
    glyph = '🔊';
  } else {
    glyph = 'audio ...';
  }

  const action = available
    ? muted ? 'Unmute audio' : 'Mute audio'
    : 'Audio unavailable';
  const audibleState = muted && available ? 'muted' : normalizedState;
  return {
    glyph,
    ariaLabel: `${action}. Audio ${audibleState}. ${normalizedDetail}`,
    title: `Audio ${audibleState}: ${normalizedDetail}`,
  };
}
