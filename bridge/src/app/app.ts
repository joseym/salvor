import { Component, isDevMode } from '@angular/core';
import { RouterLink, RouterOutlet } from '@angular/router';

import { ThemeService } from './core/theme';

@Component({
  selector: 'bridge-root',
  imports: [RouterOutlet, RouterLink],
  templateUrl: './app.html',
  styleUrl: './app.css',
})
export class App {
  protected readonly isDevMode = isDevMode();

  constructor(protected readonly theme: ThemeService) {}
}
