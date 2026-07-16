"""Pipecat observer that forwards speaking/TTS/metrics events to the
Pipecat Audio Metrics app over its bridge WebSocket.

Usage — add one line where you build your PipelineTask:

    from pipecat_observer import AudioMetricsBridgeObserver

    task = PipelineTask(
        pipeline,
        params=PipelineParams(...),
        observers=[AudioMetricsBridgeObserver("ws://localhost:8123")],
    )

If the bot runs on a different machine than the metrics app, point the URL at
the metrics machine's LAN address (the app listens on 0.0.0.0).

The observer is fire-and-forget: if the app is not running, events are
dropped silently and the bot is unaffected. Events are timestamped by the
app on arrival, so no clock sync is required (sub-ms error on localhost/LAN).

Requires: pip install websockets
"""

import asyncio
import json
from typing import Optional

import websockets

from pipecat.frames.frames import (
    BotStartedSpeakingFrame,
    BotStoppedSpeakingFrame,
    MetricsFrame,
    UserStartedSpeakingFrame,
    UserStoppedSpeakingFrame,
)
from pipecat.observers.base_observer import BaseObserver, FramePushed

try:  # optional frame types, present in most pipecat versions
    from pipecat.frames.frames import TTSStartedFrame, TTSStoppedFrame
except ImportError:  # pragma: no cover
    TTSStartedFrame = TTSStoppedFrame = None


FRAME_EVENTS = {
    UserStartedSpeakingFrame: "user_started_speaking",
    UserStoppedSpeakingFrame: "user_stopped_speaking",
    BotStartedSpeakingFrame: "bot_started_speaking",
    BotStoppedSpeakingFrame: "bot_stopped_speaking",
}
if TTSStartedFrame is not None:
    FRAME_EVENTS[TTSStartedFrame] = "bot_tts_started"
    FRAME_EVENTS[TTSStoppedFrame] = "bot_tts_stopped"


class AudioMetricsBridgeObserver(BaseObserver):
    def __init__(self, url: str = "ws://localhost:8123", source: str = "pipecat"):
        super().__init__()
        self._url = url
        self._source = source
        self._queue: asyncio.Queue = asyncio.Queue(maxsize=256)
        self._seen_ids: "OrderedSet" = _LruSet(2048)
        self._task: Optional[asyncio.Task] = None

    async def on_push_frame(self, data: FramePushed):
        if self._task is None:
            self._task = asyncio.create_task(self._sender())

        frame = data.frame
        # A frame is observed once per processor hop — dedupe by frame id.
        if frame.id in self._seen_ids:
            return

        name = FRAME_EVENTS.get(type(frame))
        if name is not None:
            self._seen_ids.add(frame.id)
            self._enqueue({"v": 1, "type": "event", "name": name,
                           "source": self._source})
            return

        if isinstance(frame, MetricsFrame):
            self._seen_ids.add(frame.id)
            for d in frame.data:
                try:
                    meta = d.model_dump(mode="json")
                except Exception:
                    meta = {"repr": repr(d)}
                self._enqueue({
                    "v": 1,
                    "type": "event",
                    "name": f"metrics_{type(d).__name__}",
                    "source": self._source,
                    "meta": meta,
                })

    def _enqueue(self, msg: dict):
        try:
            self._queue.put_nowait(json.dumps(msg))
        except asyncio.QueueFull:
            pass  # never block or break the pipeline

    async def _sender(self):
        while True:
            try:
                async with websockets.connect(self._url, open_timeout=2) as ws:
                    while True:
                        msg = await self._queue.get()
                        await ws.send(msg)
            except asyncio.CancelledError:
                return
            except Exception:
                # App not running / connection dropped: retry quietly.
                await asyncio.sleep(2)


class _LruSet:
    """Tiny bounded set for frame-id dedupe."""

    def __init__(self, capacity: int):
        self._capacity = capacity
        self._items = dict()

    def __contains__(self, item) -> bool:
        return item in self._items

    def add(self, item):
        self._items[item] = None
        if len(self._items) > self._capacity:
            self._items.pop(next(iter(self._items)))
