# bot

A Pipecat AI voice agent built with a cascade pipeline (STT → LLM → TTS).

**This is the test bot for the Pipecat Audio Metrics app** (repo root). It has
the `AudioMetricsBridgeObserver` wired into its pipeline
([server/pipecat_observer.py](server/pipecat_observer.py)), so RTVI-level
events (user/bot started/stopped speaking, TTS start/stop, TTFB metrics)
stream to the metrics app's bridge at `AUDIO_METRICS_WS_URL` (default
`ws://localhost:8123`). If the app isn't running, events are dropped silently
and the bot is unaffected.

**Benchmark workflow:** start the metrics app → `uv run bot.py` in `server/`
→ open http://localhost:7860 in a browser on this machine (bot audio must
play through this machine's output) → press Start in the metrics app → talk.

## Configuration

- **Bot Type**: Web
- **Transport(s)**: SmallWebRTC
- **Pipeline**: Cascade
  - **STT**: Deepgram
  - **LLM**: OpenAI
  - **TTS**: Cartesia

## Setup

### Server

1. **Navigate to server directory**:

   ```bash
   cd server
   ```

2. **Install dependencies**:

   ```bash
   uv sync
   ```

3. **Configure environment variables**:

   ```bash
   cp .env.example .env
   # Edit .env and add your API keys
   ```

4. **Run the bot**:

   ```bash
   uv run bot.py
   ```

   The runner serves every transport; the caller selects which one (a web/mobile
   client picks its transport when it connects; a telephony provider connects to
   `/ws`).

## Project Structure

```
bot/
├── server/              # Python bot server
│   ├── bot.py           # Main bot implementation
│   ├── pyproject.toml   # Python dependencies
│   ├── .env.example     # Environment variables template
│   ├── .env             # Your API keys (git-ignored)
│   └── ...
├── .gitignore           # Git ignore patterns
└── README.md            # This file
```
## Building with an AI coding agent

Extending this bot with Claude Code, Codex, or another AI coding assistant? Give it live, accurate Pipecat context instead of stale training data with the **Pipecat Context Hub** — a local index of Pipecat docs, examples, and API source your agent queries over MCP:

```bash
# Build the local index (first run takes a couple of minutes)
uvx pipecat-ai-context-hub@latest refresh

# Add it to your agent (use the line for the one you use)
claude mcp add pipecat-context-hub -- uvx pipecat-ai-context-hub serve   # Claude Code
codex mcp add pipecat-context-hub -- uvx pipecat-ai-context-hub serve    # Codex
```

MCP servers load at session start, so add it before opening your coding session. See the [Pipecat Context Hub docs](https://docs.pipecat.ai/api-reference/context-hub) for the full setup.

## Learn More

- [Pipecat Documentation](https://docs.pipecat.ai/)
- [Pipecat GitHub](https://github.com/pipecat-ai/pipecat)
- [Pipecat Examples](https://github.com/pipecat-ai/pipecat-examples)
- [Discord Community](https://discord.gg/pipecat)