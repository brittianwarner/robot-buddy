// Wall-clock limits in the JS harnesses, the same rule as `support::timing_bound` in
// crates/buddy/tests/support/mod.rs: a limit is always measured and printed, and fails only
// under BUDDY_STRICT_TIMING=1 (an idle, dedicated runner). Shared runners and loaded
// developer machines enforce the ordering and functional checks instead.

export const STRICT_TIMING = process.env.BUDDY_STRICT_TIMING === '1';

/**
 * Prints `TIMING <name>_ms=<took> limit_ms=<limit> ok|OVER` and returns whether the check
 * passes: always when timing is not strict, otherwise only within the limit. A missing
 * measurement (`null`) fails only under strict timing too; the functional check that
 * produced it decides otherwise.
 */
export function timingBound({ name, took, limit, log = console.error }) {
  const within = typeof took === 'number' && took <= limit;
  log(`TIMING ${name}_ms=${took == null ? '-' : Math.round(took)} limit_ms=${limit} ${within ? 'ok' : 'OVER'}`);
  return within || !STRICT_TIMING;
}
