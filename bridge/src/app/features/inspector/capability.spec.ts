import { NO_FORK_CAPABILITIES, forkOffered } from './capability';

describe('forkOffered — the fork capability gate', () => {
  it('is OFF for this build: the server advertises no fork API today', () => {
    expect(forkOffered(NO_FORK_CAPABILITIES)).toBe(false);
    expect(forkOffered({ fork: false })).toBe(false);
  });

  it('turns ON only when fork is genuinely advertised — the v0.4 path', () => {
    expect(forkOffered({ fork: true })).toBe(true);
  });
});
