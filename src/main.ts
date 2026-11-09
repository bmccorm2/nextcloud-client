import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";

type Status = { kind: string; text: string };
type State = {
  server_url: string | null;
  login_name: string | null;
  host: string | null;
  local_root: string | null;
  autostart: boolean;
  status: Status;
};

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

const server = $<HTMLInputElement>("server");
const loginForm = $<HTMLFormElement>("login-form");
const loginWait = $("login-wait");
const loggedIn = $("logged-in");
const who = $("who");
const folder = $("folder");
const confirmBox = $("confirm-folder");
const newFolder = $("new-folder");
const autostart = $<HTMLInputElement>("autostart");
const errorBox = $("error");
const statusText = $("status");
const dot = $("dot");
const activity = $("activity");

let pendingFolder: string | null = null;
let current: State | null = null;

function showError(e: unknown) {
  errorBox.textContent = String(e);
  errorBox.hidden = false;
}

function clearError() {
  errorBox.hidden = true;
}

function renderStatus(s: Status) {
  statusText.textContent = s.text;
  dot.dataset.kind = s.kind;
}

function render(s: State) {
  current = s;
  const logged = !!s.login_name;
  loginForm.hidden = logged;
  loggedIn.hidden = !logged;
  loginWait.hidden = true;
  if (logged) who.textContent = `Logged in as ${s.login_name} on ${s.host ?? ""}`;
  if (s.server_url && !server.value) server.value = s.server_url;
  folder.textContent = s.local_root ?? "Not set";
  folder.title = s.local_root ?? "";
  autostart.checked = s.autostart;
  renderStatus(s.status);
}

async function call(cmd: string, args?: Record<string, unknown>) {
  clearError();
  try {
    render(await invoke<State>(cmd, args));
  } catch (e) {
    if (e !== "Login cancelled") showError(e);
    render(await invoke<State>("get_state"));
  }
}

loginForm.addEventListener("submit", async (ev) => {
  ev.preventDefault();
  loginForm.hidden = true;
  loginWait.hidden = false;
  await call("start_login", { serverUrl: server.value });
});

$("cancel").addEventListener("click", () => invoke("cancel_login"));
$("logout").addEventListener("click", () => call("logout"));

$("choose").addEventListener("click", async () => {
  const picked = await open({ directory: true, multiple: false });
  if (typeof picked !== "string") return;
  if (!current?.local_root) {
    await call("set_local_root", { path: picked });
  } else if (picked !== current.local_root) {
    pendingFolder = picked;
    newFolder.textContent = picked;
    confirmBox.hidden = false;
  }
});

$("confirm-yes").addEventListener("click", async () => {
  confirmBox.hidden = true;
  if (pendingFolder) await call("set_local_root", { path: pendingFolder });
  pendingFolder = null;
});

$("confirm-no").addEventListener("click", () => {
  confirmBox.hidden = true;
  pendingFolder = null;
});

autostart.addEventListener("change", () => call("set_autostart", { enabled: autostart.checked }));

listen<Status>("status", (ev) => renderStatus(ev.payload));
listen<string>("activity", (ev) => {
  activity.textContent = ev.payload;
  activity.hidden = !ev.payload;
});
invoke<State>("get_state").then(render).catch(showError);
