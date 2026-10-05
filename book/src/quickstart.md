# Quick Start

This guide walks you through the essential steps to get RecurAgent running.

## 1. Initialize Your Workspace

Navigate to your project directory and initialize RecurAgent:

```bash
cd your-project
ra init
```

This creates a `.ra/` directory with default configuration, bootstrap files (AGENTS.md, SOUL.md, USER.md), and directories for memory, sessions, and skills.

## 2. Set Your API Key

Export at least one LLM provider key:

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
```

Other providers follow the same pattern — e.g. `DEEPSEEK_API_KEY`, `MOONSHOT_API_KEY`, or `ZAI_API_KEY` for Z.AI (GLM); the full optional list lives in [Configuration → Environment Variables](./configuration.md#llm-providers).

Add this to your `~/.bashrc` or `~/.zshrc` for persistence. You can also use `ra auth login --provider openai` for OAuth-based login.

## 3. Check Setup

Verify everything is configured correctly:

```bash
ra status
```

This shows your config file location, active provider and model, API key status, and bootstrap file availability.

## 4. Start Chatting

Launch an interactive multi-turn conversation:

```bash
ra chat
```

Or send a single message and exit:

```bash
ra chat --message "Add a hello function to lib.rs"
```

## 5. Run the Gateway

To serve multiple messaging channels as a persistent daemon:

```bash
ra gateway
```

This requires a `gateway` section in your config with at least one channel configured. See the [Configuration](configuration.md) chapter for details.

## 6. Launch the Web UI

If you built with the `api` feature, start the web dashboard:

```bash
ra serve
```

Then open `http://localhost:50080` in your browser — it lands on the ra-web app (`/app/`). The admin dashboard stays available at `/admin/`.
