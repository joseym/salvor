import { TestBed } from '@angular/core/testing';
import { provideRouter } from '@angular/router';
import { App } from './app';
import { routes } from './app.routes';
import { provideSalvorApi } from './core/api';

describe('App', () => {
  beforeEach(async () => {
    await TestBed.configureTestingModule({
      imports: [App],
      providers: [provideRouter(routes), provideSalvorApi({ baseUrl: '' })],
    }).compileComponents();
  });

  it('should create the app', () => {
    const fixture = TestBed.createComponent(App);
    const app = fixture.componentInstance;
    expect(app).toBeTruthy();
  });

  it('should render the shell brand, the five nav links and the theme toggle', async () => {
    const fixture = TestBed.createComponent(App);
    fixture.detectChanges();
    await fixture.whenStable();
    const compiled = fixture.nativeElement as HTMLElement;
    expect(compiled.querySelector('#app-nav')).toBeTruthy();
    expect(compiled.textContent).toContain('Salvor');
    expect(compiled.textContent).toContain('bridge v0.3');
    // All five views are nav destinations — Workflows joined when the graph engine shipped.
    expect(compiled.querySelectorAll('.nav-link').length).toBe(5);
    const labels = Array.from(compiled.querySelectorAll('.nav-link .nav-text')).map((n) => n.textContent?.trim());
    expect(labels).toEqual(['Runs', 'Inspector', 'Inbox', 'Workflows', 'Spend']);
    expect(compiled.querySelector('#theme-toggle')).toBeTruthy();
  });
});
