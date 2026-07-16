import { type Signal } from '@angular/core';

import { type ConnectionState, createConnectionStateMachine } from './connection-state';

describe('createConnectionStateMachine', () => {
  it('starts as Idle(Snapshot)', () => {
    const { state } = createConnectionStateMachine();
    expect(state().kind).toBe('idle');
    expect(state().label).toBe('Snapshot');
  });

  it('transitions through the driver only, never a generic setter', () => {
    const { state, driver } = createConnectionStateMachine();

    driver.toConnected();
    expect(state()).toEqual(expect.objectContaining({ kind: 'connected', label: 'Live' }));

    driver.toEnded(false);
    expect(state()).toEqual(
      expect.objectContaining({ kind: 'ended', label: 'Ended', detached: false }),
    );

    driver.toPolling();
    expect(state()).toEqual(expect.objectContaining({ kind: 'polling', label: 'Polling' }));

    driver.toSnapshot('2026-01-01T00:00:00.000Z');
    expect(state()).toEqual({
      kind: 'idle',
      label: 'Snapshot',
      asOf: '2026-01-01T00:00:00.000Z',
    });
  });

  it('exposes state as a Signal with no set/update at the type level (the "no public setter" invariant)', () => {
    const { state } = createConnectionStateMachine();
    // Compile-time proof: `Signal<ConnectionState>` has no `.set` — this line only
    // typechecks because `state` is NOT typed as WritableSignal. If a future edit widens
    // the export to a WritableSignal, `tsc` fails here before any runtime check does.
    const readonlyState: Signal<ConnectionState> = state;
    expect('set' in readonlyState).toBe(false);
    expect('update' in readonlyState).toBe(false);
  });
});
