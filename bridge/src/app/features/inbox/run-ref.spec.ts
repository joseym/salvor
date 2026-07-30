import { TestBed } from '@angular/core/testing';

import { RunRef } from './run-ref';

describe('RunRef', () => {
  it('defaults to id: truncates to 8 chars, .runid', () => {
    const fixture = TestBed.createComponent(RunRef);
    fixture.componentRef.setInput('id', 'run-abc123456789');
    fixture.detectChanges();
    const el = fixture.nativeElement as HTMLElement;
    const value = el.querySelector('.runid') as HTMLElement;
    expect(value.textContent).toBe('run-abc1');
    expect(value.getAttribute('title')).toBe('run-abc123456789');
  });

  it('kind="hash" keeps the sha256: prefix, then 8 hex; the full value stays in the title', () => {
    const fixture = TestBed.createComponent(RunRef);
    fixture.componentRef.setInput('id', 'sha256:71d29b223d5184c1d95d7b798d00cd6');
    fixture.componentRef.setInput('kind', 'hash');
    fixture.detectChanges();
    const el = fixture.nativeElement as HTMLElement;
    const value = el.querySelector('.runid') as HTMLElement;
    expect(value.textContent).toBe('sha256:71d29b22');
    expect(el.querySelector('.agent-label')).toBeNull();
  });

  it('kind="label" is never truncated and wears .agent-label, not .runid', () => {
    const fixture = TestBed.createComponent(RunRef);
    fixture.componentRef.setInput('id', 'a client-driven run label with plenty of characters');
    fixture.componentRef.setInput('kind', 'label');
    fixture.detectChanges();
    const el = fixture.nativeElement as HTMLElement;
    expect(el.querySelector('.runid')).toBeNull();
    const value = el.querySelector('.agent-label') as HTMLElement;
    expect(value.textContent).toBe('a client-driven run label with plenty of characters');
  });

  it('copy() stops propagation and writes the FULL untruncated value to the clipboard', async () => {
    const fixture = TestBed.createComponent(RunRef);
    fixture.componentRef.setInput('id', 'sha256:71d29b223d5184c1d95d7b798d00cd6');
    fixture.componentRef.setInput('kind', 'hash');
    fixture.detectChanges();
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal('navigator', { clipboard: { writeText } });

    const el = fixture.nativeElement as HTMLElement;
    const button = el.querySelector('.copy') as HTMLButtonElement;
    const event = new MouseEvent('click', { bubbles: true, cancelable: true });
    const stopSpy = vi.spyOn(event, 'stopPropagation');
    button.dispatchEvent(event);
    await Promise.resolve();

    expect(stopSpy).toHaveBeenCalled();
    expect(writeText).toHaveBeenCalledWith('sha256:71d29b223d5184c1d95d7b798d00cd6');
    vi.unstubAllGlobals();
  });
});
