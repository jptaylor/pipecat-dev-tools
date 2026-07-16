/**
 * Forwards RTVI client events (from @pipecat-ai/client-js) to the Pipecat
 * Audio Metrics app's bridge WebSocket.
 *
 * Usage:
 *
 *   import { attachAudioMetricsBridge } from "./rtvi_client_bridge.js";
 *
 *   const client = new RTVIClient({ ... });      // or PipecatClient
 *   const detach = attachAudioMetricsBridge(client, {
 *     url: "ws://localhost:8123",
 *   });
 *   // later: detach();
 *
 * Fire-and-forget: if the metrics app is not running, events are dropped and
 * the client is unaffected. The app timestamps events on arrival.
 */

const EVENT_MAP = {
  userStartedSpeaking: "user_started_speaking",
  userStoppedSpeaking: "user_stopped_speaking",
  botStartedSpeaking: "bot_started_speaking",
  botStoppedSpeaking: "bot_stopped_speaking",
  botTtsStarted: "bot_tts_started",
  botTtsStopped: "bot_tts_stopped",
  botLlmStarted: "bot_llm_started",
  botLlmStopped: "bot_llm_stopped",
  metrics: "metrics",
};

export function attachAudioMetricsBridge(client, { url = "ws://localhost:8123", source = "rtvi-client" } = {}) {
  let ws = null;
  let closed = false;
  let retryTimer = null;

  function connect() {
    if (closed) return;
    try {
      ws = new WebSocket(url);
      ws.onopen = () => ws.send(JSON.stringify({ v: 1, type: "hello" }));
      ws.onclose = () => scheduleRetry();
      ws.onerror = () => {};
    } catch {
      scheduleRetry();
    }
  }

  function scheduleRetry() {
    ws = null;
    if (closed || retryTimer) return;
    retryTimer = setTimeout(() => {
      retryTimer = null;
      connect();
    }, 2000);
  }

  function send(name, meta) {
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    try {
      ws.send(JSON.stringify({ v: 1, type: "event", name, source, meta: meta ?? null }));
    } catch {}
  }

  const handlers = [];
  for (const [event, name] of Object.entries(EVENT_MAP)) {
    const handler = (payload) => {
      let meta = null;
      if (name === "metrics" && payload) {
        try {
          meta = JSON.parse(JSON.stringify(payload));
        } catch {}
      }
      send(name, meta);
    };
    try {
      client.on(event, handler);
      handlers.push([event, handler]);
    } catch {
      // Unknown event name on this client version — skip.
    }
  }

  connect();

  return function detach() {
    closed = true;
    if (retryTimer) clearTimeout(retryTimer);
    for (const [event, handler] of handlers) {
      try {
        client.off?.(event, handler);
      } catch {}
    }
    try {
      ws?.close();
    } catch {}
  };
}
