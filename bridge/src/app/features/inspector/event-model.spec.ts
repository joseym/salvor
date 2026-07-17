import { zoneOf } from './event-model';

describe('zoneOf — the scrub zone a timeline row falls into', () => {
  it('marks every seq at or past the prefix as beyond', () => {
    expect(zoneOf(5, 5)).toBe('beyond');
    expect(zoneOf(6, 5)).toBe('beyond');
    expect(zoneOf(0, 0)).toBe('beyond'); // nothing folded yet — seq 0 has not happened
  });

  it('marks seq n-1 — the event under the playhead — as boundary, never seq n', () => {
    // the bug this replaces: the old treatment accented seq === n (the first EXCLUDED event)
    expect(zoneOf(4, 5)).toBe('boundary');
    expect(zoneOf(5, 5)).not.toBe('boundary');
  });

  it('there is no boundary at n = 0 — nothing is folded, so nothing is under the playhead', () => {
    expect(zoneOf(0, 0)).not.toBe('boundary');
    expect(zoneOf(-1, 0)).not.toBe('boundary');
  });

  it('marks everything strictly before the boundary as folded', () => {
    expect(zoneOf(0, 5)).toBe('folded');
    expect(zoneOf(3, 5)).toBe('folded');
  });

  it('a single-event prefix (n=1) makes seq 0 the boundary, not folded or beyond', () => {
    expect(zoneOf(0, 1)).toBe('boundary');
  });

  it('classifies a whole 26-event log consistently, one boundary, folded before it, beyond after', () => {
    const n = 12;
    const zones = Array.from({ length: 26 }, (_, seq) => zoneOf(seq, n));
    expect(zones.filter((z) => z === 'boundary')).toHaveLength(1);
    expect(zones[n - 1]).toBe('boundary');
    expect(zones.slice(0, n - 1).every((z) => z === 'folded')).toBe(true);
    expect(zones.slice(n).every((z) => z === 'beyond')).toBe(true);
  });
});
