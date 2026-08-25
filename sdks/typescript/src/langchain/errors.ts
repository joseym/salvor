/** The errors this middleware raises on its own account. */

import { SalvorError } from "../errors.js";

/**
 * Something the middleware itself refuses, as opposed to something the control
 * plane refused (which stays a `SalvorApiError`). Every message names the thread
 * or the tool it is about and what would fix it, because these all surface
 * inside somebody else's agent loop, far from this file.
 */
export class SalvorMiddlewareError extends SalvorError {}
