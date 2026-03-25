# Minimal Web Console

This folder contains a tiny browser UI for `agent-bus`.

What it does:
- Reads recent messages from `GET /messages`
- Reads recent notifications from `GET /notifications/{agent}`
- Sends a basic message with `POST /messages`
- Sends a knock with `POST /knock`

Assumptions:
- The HTTP service is already running, usually at `http://localhost:8400`
- The main repo will decide how this folder is served
- This UI is intentionally static and framework-free

Files:
- `index.html` - page shell
- `styles.css` - small visual layer
- `app.js` - HTTP API client and rendering logic
