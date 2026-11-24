# Browser test lab

The server includes an optional workbench at `/lab`. It uses the same enrollment verifier, native Rivet client, Conversation actor, Rig AgentRun, and Memory actor as the robot. The server mediates browser actor calls using a device-scoped application session. The physical robot still uses the official Rust client and its scoped engine gateway token directly.

## Local workspace

For the prepared local configuration, run `bun run dev:lab` from the repository root and open `http://127.0.0.1:18080/lab`. The local Compose file uses isolated ports and a separate named data volume. `bun run dev:lab:stop` stops the container without deleting its data.

The ignored `.env.local` holds the provider configuration and `BUDDY_LAB_TOKEN`; copy the latter into the workbench login. Load `secrets/local/device.identity.json` when connecting a simulated robot. These files are private local setup artifacts, not repository defaults. Edit `AI_MODEL` to choose another OpenRouter model, then rerun `bun run dev:lab` to recreate the server with updated configuration. Do not share the local environment file.

OIDC registration is optional for operator chat testing. Configure your real OIDC issuer/client before testing real account ownership. Without it, the lab can still enroll a device and run conversations; it clearly reports registration is unavailable.

## Start

1. Follow the README's initialization and provisioning instructions. Keep the generated device identity JSON available on your computer.
2. Configure `.env` with your OpenRouter model/key, or an explicit local OpenAI-compatible endpoint and tool-capable model. Add `BUDDY_LAB_TOKEN` with a new, random secret of at least 32 characters. Do not reuse the signing key. This UI is disabled when that variable is empty.
3. Configure OIDC to test ownership registration, with the callback `${BUDDY_PUBLIC_URL}/auth/callback`. Supported ID-token algorithms are RS256 and ES256; the discovery document must provide `jwks_uri`.
4. Run `docker compose up --build -d`, then visit `${BUDDY_PUBLIC_URL}/lab`. For loopback-only testing set `BUDDY_PUBLIC_URL=http://127.0.0.1:8080`, `BUDDY_PUBLIC_RIVET_URL=http://127.0.0.1:6420`, and `BUDDY_ALLOW_INSECURE_LOCAL=true`. Existing services must not occupy those host ports. Production URLs require HTTPS.
5. Enter the operator token, load the provisioned identity JSON, and connect. Browser WebCrypto signs the server challenge; the private key remains in browser memory. Reconnecting the same identity resolves the same bot.
6. Generate the QR, open registration, sign in, compare the code, and confirm on the simulated robot face.
7. Chat, ask it to remember a fact, inspect tool activity, refresh the page, load earlier history, and forget a saved fact. Stop cancels a running turn. Durable replay catches up after brief disconnections.

The workbench may chat before owner registration to allow provider testing without OIDC. It requires an operator credential plus a provisioned device identity. The physical robot's normal UI continues to require owner registration. The lab simulates the stage after Wi-Fi connects; it cannot validate NetworkManager or a physical display.

History is stored durably; only a bounded recent window and selected memories go into each model request. History search is a model tool. This lab has no arbitrary actor-command console, shell, file access, or admin revocation control. It renders model output as text, never executable HTML. Login cookies are HttpOnly/SameSite; mutations require Origin and CSRF checks. Responses are not cached. Disable the lab when it is not needed.

## Repeatable browser acceptance

Install dependencies with `bun install` and a browser with `bunx playwright install chromium`. Against an isolated server using test credentials:

```sh
BUDDY_TEST_SERVER_URL=http://127.0.0.1:8080 \
BUDDY_TEST_IDENTITY_FILE=secrets/device.identity.json \
BUDDY_LAB_TOKEN="$BUDDY_LAB_TOKEN" \
bun run test:lab
```

Set `BUDDY_TEST_CLAIM=1` only with an automated test OIDC provider; a real provider may require interactive authentication. The test expects a tool-capable model to remember blue, so it can incur provider charges. It checks login, rejected Origin/CSRF/admin actions, enrollment/reconnect identity, chat/tool/memory, history after reload, and mobile overflow. Screenshots default to `/tmp/robot-buddy-lab.png` and `/tmp/robot-buddy-lab-mobile.png`. Never run with a production bot: it sends a message and saves a fact.

The test key under `apps/server/tests/fixtures` is public test material used only by unit tests. Never configure it as a production identity provider.

## Spoken replies

The prepared local configuration uses `z-ai/glm-5.3-flash:nitro` with low reasoning and `elevenlabs/eleven-v4-turbo` for speech. The default speaker is Will. Restart the server after a voice-model change so an untouched factory actor adopts it. Enable **Speak new replies** after connecting your robot, then type a message. The checkbox click unlocks browser audio. **Replay last answer** speaks a completed answer explicitly; **Stop voice** cancels the active turn and streaming/playback. Reloading the page remains silent.

The voice panel displays first audio from the speech request, buffer duration and PCM format. This does not measure microphone-to-answer latency. Admitted dialogue sentences play as they become available; thinking and tool activity remain silent. The first-audio panel records whether playback began before turn completion. There is no microphone transcription yet. Set `BUDDY_TEST_VOICE=1` when running `test:lab` to test actual playback, interruption and replay (paid model requests).

## Persona, voice and local time

The Current robot prompt panel shows the saved editable persona and its revision. **Edit prompt** opens it in an editor; **Save prompt** stores it directly (`set_system_prompt` with the current revision, no model involved) and it applies from the next message. Refresh settings reads actor state again; completed agent tools also refresh it. Inspect persona opens the last 16 prompt revisions. The voice panel shows the saved speaker, delivery direction and expandable synthesis controls. Named persona and version selectors prepare a chat request, so switching exercises the agent tools instead of bypassing them.

Try:

- “Create and save an Action Buddy persona inspired by Arnold Schwarzenegger action-hero roles. Customize the delivery and voice settings, then switch to it.”
- “Make Action Buddy a little calmer and save a new version.”
- “Switch back to Buddy version 1.”
- “Use Sarah’s voice and keep the rest of your settings.”
- “Set your timezone to America/New_York. What time is it there?”

The date/time inspector is a snapshot taken when settings refreshes. Model requests receive a fresh clock independently. Location remains unknown until configured; the browser does not silently adopt its own location or timezone. Stored synthesis numbers are not sent to Eleven v4 Turbo. PCM format and capacity/authentication limits remain enforced by the server. Buddy version 1 on a new install is the J.P. companion. On an actor that still had the original factory profile, the first restart rewrites that untouched version 1. A profile you already edited stays as you saved it.

## Earlier-speech checks

`bun tests/early-player.mjs` runs an isolated browser/PCM fixture and verifies playback before response completion and cancellation. `bun tests/early-live.mjs` uses the same environment variables as browser acceptance, sends a long story request through OpenRouter, records whether first audio precedes turn completion, then tests Stop and silent reload. Run it only with a separate test identity; it incurs inference and speech charges. Actual early-audio timing depends on both providers. Rust actor integration additionally checks silent planning/reasoning, early chunk availability, pinned settings and cancellation. The Linux pipe-player test proves incremental consumption without requiring ALSA hardware.
