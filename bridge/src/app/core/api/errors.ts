import { SalvorError } from '@salvor/client';

/** Render any thrown value as a display string, without ever leaking `any`. */
export function errorMessage(err: unknown): string {
  if (err instanceof SalvorError) return err.message;
  if (err instanceof Error) return err.message;
  return String(err);
}
