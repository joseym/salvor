# Bridge

This project was generated using [Angular CLI](https://github.com/angular/angular-cli) version 22.0.7.

## The `@salvor/client` dependency

`bridge/` consumes the TypeScript SDK at `sdks/typescript` as a local npm `file:`
dependency (`package.json`: `"@salvor/client": "file:../sdks/typescript"`). `npm install`
symlinks it into `node_modules/@salvor/client` the same way `npm link` would, so edits to
the SDK's source are picked up on the SDK's own next `npm run build` with no republish or
version bump. Nothing is published anywhere; there is no npm registry involved.

Because the SDK's `package.json` points `main`/`types` at its own `dist/` (gitignored,
built by `tsc`), that `dist/` must exist before `bridge/`'s build resolves the package:

```bash
cd sdks/typescript && npm install && npm run build
cd ../bridge && npm install
```

The API layer wrapping it lives in `src/app/core/api/`. See that directory's `index.ts`
for the exported surface (`RunsService`, `RunDetailService`, `RunEventsService`,
`ClientRunService`, and the connection-pill state machine).

## Development server

To start a local development server, run:

```bash
ng serve
```

Once the server is running, open your browser and navigate to `http://localhost:4200/`. The application will automatically reload whenever you modify any of the source files.

## Code scaffolding

Angular CLI includes powerful code scaffolding tools. To generate a new component, run:

```bash
ng generate component component-name
```

For a complete list of available schematics (such as `components`, `directives`, or `pipes`), run:

```bash
ng generate --help
```

## Building

To build the project run:

```bash
ng build
```

This will compile your project and store the build artifacts in the `dist/` directory. By default, the production build optimizes your application for performance and speed.

## Running unit tests

To execute unit tests with the [Vitest](https://vitest.dev/) test runner, use the following command:

```bash
ng test
```

## Running end-to-end tests

For end-to-end (e2e) testing, run:

```bash
ng e2e
```

Angular CLI does not come with an end-to-end testing framework by default. You can choose one that suits your needs.

## Additional Resources

For more information on using the Angular CLI, including detailed command references, visit the [Angular CLI Overview and Command Reference](https://angular.dev/tools/cli) page.
