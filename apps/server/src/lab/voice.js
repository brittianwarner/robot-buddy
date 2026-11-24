"use strict";
// Same PCM contract as the Linux player: one turn, bounded buffering, explicit cancellation.
window.BuddyVoice = (() => {
  let context = null,
    controller = null,
    generation = 0,
    nextTime = 0,
    busy = false,
    enabled = false;
  const sources = new Set(),
    watched = new Set(),
    played = new Set();
  let currentTurn = null;
  const el = (id) => document.getElementById(id);
  function status(text) {
    el("voice-status").textContent = text;
    document.querySelector(".face").dataset.speaking = String(busy);
    el("face-status").textContent = busy ? text : "I'm here.";
    controls();
  }
  async function prepare() {
    context ||= new AudioContext();
    await context.resume();
  }
  function stop() {
    currentTurn = null;
    watched.clear();
    generation++;
    controller?.abort();
    controller = null;
    for (const source of sources) {
      try {
        source.stop();
      } catch {}
    }
    sources.clear();
    busy = false;
    nextTime = 0;
    el("audio-buffer").textContent = "0 ms";
    status("Voice stopped");
  }
  async function speak(turnId, stream = false) {
    stop();
    el("first-audio").textContent = "—";
    delete el("first-audio").dataset.beforeCompletion;
    currentTurn = turnId;
    const mine = generation;
    const started = performance.now();
    busy = true;
    status("Preparing voice…");
    const abort = new AbortController();
    controller = abort;
    try {
      await prepare();
      if (mine !== generation) return;
      const response = await fetch("/lab/api/speech", {
        method: "POST",
        credentials: "same-origin",
        headers: { "content-type": "application/json", "x-buddy-csrf": csrf },
        body: JSON.stringify({ turn_id: turnId, stream }),
        signal: abort.signal,
      });
      if (!response.ok)
        throw new Error("Speech unavailable. Check the speech model and key.");
      if (
        response.headers.get("x-audio-turn-id") !== turnId ||
        response.headers.get("x-audio-encoding") !== "pcm_s16le"
      )
        throw new Error("Unexpected audio contract.");
      const type = response.headers.get("content-type") || "",
        rate = Number(type.match(/rate=(\d+)/)?.[1]),
        channels = Number(type.match(/channels=(\d+)/)?.[1]);
      if (
        !type.startsWith("audio/pcm") ||
        rate < 8000 ||
        rate > 96000 ||
        ![1, 2].includes(channels)
      )
        throw new Error("Unsupported audio format.");
      el("audio-format").textContent =
        `PCM 16-bit · ${rate / 1000} kHz · ${channels === 1 ? "mono" : "stereo"}`;
      const reader = response.body.getReader();
      let remainder = new Uint8Array(0),
        total = 0,
        first = true;
      while (true) {
        const { done, value } = await reader.read();
        if (done) break;
        if (mine !== generation) return;
        total += value.byteLength;
        if (total > 16 * 1024 * 1024)
          throw new Error("Audio exceeded its size budget.");
        const data = new Uint8Array(remainder.length + value.length);
        data.set(remainder);
        data.set(value, remainder.length);
        const frameBytes = channels * 2,
          usable = data.length - (data.length % frameBytes);
        remainder = data.slice(usable);
        for (let offset = 0; offset < usable;) {
          while (nextTime - context.currentTime > 0.5) {
            if (mine !== generation) return;
            await new Promise((r) => setTimeout(r, 20));
          }
          if (mine !== generation) return;
          const frames = Math.min(2048, (usable - offset) / frameBytes),
            buffer = context.createBuffer(channels, frames, rate),
            view = new DataView(
              data.buffer,
              data.byteOffset + offset,
              frames * frameBytes,
            );
          for (let ch = 0; ch < channels; ch++) {
            const samples = buffer.getChannelData(ch);
            for (let i = 0; i < frames; i++)
              samples[i] = view.getInt16((i * channels + ch) * 2, true) / 32768;
          }
          const source = context.createBufferSource();
          source.buffer = buffer;
          source.connect(context.destination);
          sources.add(source);
          source.onended = () => {
            sources.delete(source);
            source.disconnect();
          };
          nextTime = Math.max(nextTime, context.currentTime + 0.04);
          source.start(nextTime);
          nextTime += frames / rate;
          offset += frames * frameBytes;
          if (first) {
            first = false;
            el("first-audio").dataset.beforeCompletion = String(
              active === turnId,
            );
            if (active === turnId)
              activity("Speech began before the reply finished generating");
            el("first-audio").textContent =
              ((performance.now() - started) / 1000).toFixed(2) + " s";
            status("Speaking…");
            activity("Voice playback started · " + turnId.slice(0, 8));
          }
          el("audio-buffer").textContent =
            Math.max(0, (nextTime - context.currentTime) * 1000).toFixed(0) +
            " ms";
        }
      }
      if (remainder.length || first)
        throw new Error("Incomplete or empty audio stream.");
      while (mine === generation && sources.size)
        await new Promise((r) => setTimeout(r, 30));
      if (mine === generation) {
        busy = false;
        controller = null;
        el("audio-buffer").textContent = "0 ms";
        status("Voice ready");
        activity("Speech finished");
      }
    } catch (e) {
      if (mine === generation) {
        stop();
        if (e.name !== "AbortError") {
          status("Voice error");
          notice(e.message);
        }
      }
    }
  }
  function complete(id) {
    if (enabled && watched.has(id) && !played.has(id)) {
      played.add(id);
      if (played.size > 200) played.delete(played.values().next().value);
      watched.delete(id);
      speak(id, true);
    }
  }
  el("voice-enabled").onchange = async (e) => {
    enabled = e.target.checked;
    if (enabled) {
      try {
        await prepare();
        status("Voice enabled for new replies");
      } catch {
        enabled = false;
        e.target.checked = false;
        notice("Audio permission was not granted.");
      }
    } else stop();
  };
  el("voice-stop").onclick = async () => {
    stop();
    if (active) {
      try {
        await call("conversation", { type: "cancel", turn_id: active });
        await poll();
      } catch (e) {
        notice(e.message);
      }
    }
  };
  el("voice-replay").onclick = async () => {
    const turn = [...turns.values()]
      .filter((t) => t.status === "completed" && t.answer)
      .sort((a, b) => b.id.localeCompare(a.id))[0];
    if (!turn) return notice("Send a message first.");
    await prepare();
    speak(turn.id);
  };
  return {
    prepare,
    stop,
    complete,
    cancel: (id) => {
      if (currentTurn === id) stop();
    },
    watch: (id) => {
      watched.clear();
      watched.add(id);
    },
    // A lost connection ends automatic playback: a reply that completes while
    // disconnected is not spoken when catch-up events arrive. Replay still works.
    disconnected: () => watched.clear(),
    get busy() {
      return busy;
    },
    get enabled() {
      return enabled;
    },
    configure: (model) => {
      el("speech-model").textContent = model || "Speech not configured";
      el("voice-enabled").disabled = !model;
      el("voice-replay").disabled = !model;
    },
  };
})();
