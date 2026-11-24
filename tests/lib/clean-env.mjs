// Bun loads `.env`, `.env.local` and friends into process.env automatically. A developer's
// lab configuration (a real provider key, AI_REASONING_EFFORT, TTS settings) must never
// reach the isolated test stacks, so harnesses start from this copy instead.
const PRIVATE_PREFIXES = ["AI_", "TTS_", "BRAVE_", "OIDC_", "BUDDY_", "RIVET_", "DEVICE_ISSUER_"];
const KEPT = new Set([
  "BUDDY_STRICT_TIMING",
  "BUDDY_UPDATE_GOLDEN",
  "RIVET_ENGINE_BINARY_PATH",
]);

/** process.env without server, provider and lab settings; test knobs are kept. */
export function cleanEnv() {
  const env = { ...process.env };
  for (const key of Object.keys(env)) {
    if (KEPT.has(key) || key.startsWith("BUDDY_TEST_")) continue;
    if (PRIVATE_PREFIXES.some((prefix) => key.startsWith(prefix))) delete env[key];
  }
  return env;
}
