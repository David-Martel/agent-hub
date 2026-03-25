const els = {
  baseUrl: document.getElementById("baseUrl"),
  agent: document.getElementById("agent"),
  sender: document.getElementById("sender"),
  recipient: document.getElementById("recipient"),
  topic: document.getElementById("topic"),
  body: document.getElementById("body"),
  requestAck: document.getElementById("requestAck"),
  refreshBtn: document.getElementById("refreshBtn"),
  sendBtn: document.getElementById("sendBtn"),
  knockBtn: document.getElementById("knockBtn"),
  status: document.getElementById("status"),
  messages: document.getElementById("messages"),
  notifications: document.getElementById("notifications"),
};

const storageKeys = {
  baseUrl: "agent-bus.web.baseUrl",
  agent: "agent-bus.web.agent",
  sender: "agent-bus.web.sender",
  recipient: "agent-bus.web.recipient",
  topic: "agent-bus.web.topic",
};

function setStatus(text, isError = false) {
  els.status.textContent = text;
  els.status.classList.toggle("error", isError);
}

function loadPrefs() {
  for (const [key, storageKey] of Object.entries(storageKeys)) {
    const value = localStorage.getItem(storageKey);
    if (value) {
      els[key].value = value;
    }
  }
  if (!els.agent.value) {
    els.agent.value = "codex";
  }
  if (!els.sender.value) {
    els.sender.value = els.agent.value || "codex";
  }
  if (!els.recipient.value) {
    els.recipient.value = "claude";
  }
  if (!els.topic.value) {
    els.topic.value = "status";
  }
}

function savePrefs() {
  for (const [key, storageKey] of Object.entries(storageKeys)) {
    localStorage.setItem(storageKey, els[key].value.trim());
  }
}

function escapeText(value) {
  return String(value ?? "");
}

function renderList(container, items, renderItem, emptyText) {
  if (!items.length) {
    container.innerHTML = `<p class="empty">${escapeText(emptyText)}</p>`;
    return;
  }
  container.innerHTML = items.map(renderItem).join("");
}

function messageItem(message) {
  const tags = Array.isArray(message.tags) ? message.tags.join(", ") : "";
  return `
    <article class="item">
      <div class="item-top">
        <span class="pill">${escapeText(message.from || "unknown")} -> ${escapeText(message.to || "all")}</span>
        <span>${escapeText(message.timestamp_utc || "")}</span>
      </div>
      <div class="item-top">
        <strong>${escapeText(message.topic || "")}</strong>
        <span>${escapeText(message.priority || "")}${tags ? ` · ${escapeText(tags)}` : ""}</span>
      </div>
      <p class="body">${escapeText(message.body || "")}</p>
    </article>
  `;
}

function notificationItem(notification) {
  const msg = notification.message || {};
  return `
    <article class="item">
      <div class="item-top">
        <span class="pill">${escapeText(notification.reason || "notification")}</span>
        <span>${escapeText(notification.created_at || "")}</span>
      </div>
      <div class="item-top">
        <strong>${escapeText(msg.from || "")} -> ${escapeText(notification.agent || "")}</strong>
        <span>${msg.request_ack ? "ack" : "info"}</span>
      </div>
      <p class="body">${escapeText(msg.body || "")}</p>
    </article>
  `;
}

async function jsonFetch(url, init) {
  const response = await fetch(url, {
    headers: { "content-type": "application/json" },
    ...init,
  });
  const text = await response.text();
  let body = null;
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    body = text;
  }
  if (!response.ok) {
    const message = typeof body === "object" && body && "error" in body ? body.error : response.statusText;
    throw new Error(message || `HTTP ${response.status}`);
  }
  return body;
}

async function loadMessages() {
  const baseUrl = els.baseUrl.value.trim().replace(/\/+$/, "");
  const url = new URL(`${baseUrl}/messages`);
  if (els.agent.value.trim()) {
    url.searchParams.set("agent", els.agent.value.trim());
  }
  url.searchParams.set("since", "120");
  url.searchParams.set("limit", "25");
  const data = await jsonFetch(url.toString());
  const items = Array.isArray(data) ? data : data.messages || [];
  renderList(els.messages, items, messageItem, "No messages yet.");
}

async function loadNotifications() {
  const agent = els.agent.value.trim();
  if (!agent) {
    renderList(els.notifications, [], notificationItem, "Set an agent to read notifications.");
    return;
  }
  const baseUrl = els.baseUrl.value.trim().replace(/\/+$/, "");
  const url = new URL(`${baseUrl}/notifications/${encodeURIComponent(agent)}`);
  url.searchParams.set("history", "25");
  const data = await jsonFetch(url.toString());
  const items = Array.isArray(data) ? data : data.notifications || [];
  renderList(els.notifications, items, notificationItem, "No notifications yet.");
}

async function refresh() {
  savePrefs();
  setStatus("Refreshing...");
  try {
    await Promise.all([loadMessages(), loadNotifications()]);
    setStatus("Ready.");
  } catch (error) {
    setStatus(error.message || String(error), true);
  }
}

async function sendMessage(topic) {
  savePrefs();
  const baseUrl = els.baseUrl.value.trim().replace(/\/+$/, "");
  const payload = {
    sender: els.sender.value.trim(),
    recipient: els.recipient.value.trim(),
    topic,
    body: els.body.value.trim() || (topic === "knock" ? "check the bus" : "ready"),
    request_ack: els.requestAck.checked,
    tags: [],
  };
  if (!payload.sender || !payload.recipient) {
    throw new Error("sender and recipient are required");
  }
  await jsonFetch(`${baseUrl}/messages`, {
    method: "POST",
    body: JSON.stringify(payload),
  });
}

async function knock() {
  savePrefs();
  const baseUrl = els.baseUrl.value.trim().replace(/\/+$/, "");
  const payload = {
    sender: els.sender.value.trim(),
    recipient: els.recipient.value.trim(),
    body: els.body.value.trim() || "check the bus",
    request_ack: true,
    tags: ["attention", "knock"],
  };
  if (!payload.sender || !payload.recipient) {
    throw new Error("sender and recipient are required");
  }
  await jsonFetch(`${baseUrl}/knock`, {
    method: "POST",
    body: JSON.stringify(payload),
  });
}

els.refreshBtn.addEventListener("click", () => void refresh());
els.sendBtn.addEventListener("click", async () => {
  setStatus("Sending message...");
  try {
    await sendMessage(els.topic.value.trim() || "status");
    setStatus("Message sent.");
    await refresh();
  } catch (error) {
    setStatus(error.message || String(error), true);
  }
});

els.knockBtn.addEventListener("click", async () => {
  setStatus("Sending knock...");
  try {
    await knock();
    setStatus("Knock sent.");
    await refresh();
  } catch (error) {
    setStatus(error.message || String(error), true);
  }
});

for (const element of [els.baseUrl, els.agent, els.sender, els.recipient, els.topic]) {
  element.addEventListener("change", savePrefs);
}

loadPrefs();
void refresh();
