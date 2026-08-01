import { SalvorApiError } from '@salvor-run/client';

/**
 * WHAT A REFUSED START SAYS. `POST /v1/graph-runs` resolves everything the document references
 * BEFORE it spawns a run, so a reference that cannot resolve comes back as a refusal with no run id
 * rather than a run that fails halfway. Two of those refusals are facts about how the server is
 * wired rather than defects in the graph, and the canvas says so instead of leaving the operator to
 * edit a document that was never the problem:
 *
 * - `unknown_tool`: a stock `salvor serve` wires the tool registry EMPTY (`API.md`, "Resolution and
 *   the tool story"), so on a default server EVERY `tool` node refuses this way until a host
 *   registers the tool it names. `salvor serve --demo-tools` is the built-in non-empty registry.
 * - `unknown_graph`: stored graphs live in the server's memory and do not survive a restart. Content
 *   addressing makes that recoverable rather than lossy (the identical document mints the identical
 *   hash), so the honest instruction is "publish it again", not "it is gone".
 *
 * Every other refusal is reported verbatim and nothing is added: an explanation nobody has written
 * for it would be a guess dressed as a fact.
 */
export function startRefusal(err: unknown): string {
  const message = err instanceof Error ? err.message : String(err);
  const code = err instanceof SalvorApiError ? err.code : '';
  if (code === 'unknown_tool') {
    return (
      `start refused: ${message}. A stock salvor serve wires an empty tool registry, so every ` +
      `tool node refuses until a host registers that tool (salvor serve --demo-tools is the ` +
      `built-in set).`
    );
  }
  if (code === 'unknown_graph') {
    return (
      `start refused: ${message}. Stored graphs live in the server's memory, so a restart drops ` +
      `them; publishing the identical document again mints the identical hash.`
    );
  }
  return `start refused: ${message}`;
}
