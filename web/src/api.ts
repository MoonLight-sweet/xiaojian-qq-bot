// Admin API client. The bearer token lives in sessionStorage only: it
// survives a page reload inside the tab, never touches disk, and is
// dropped when the tab closes. A 401 clears it and bounces the shell
// back to the login form via the `auth-expired` event.

const TOKEN_KEY = "gemini-admin-token";

export function getToken(): string | null {
  return sessionStorage.getItem(TOKEN_KEY);
}

export function setToken(token: string | null): void {
  if (token) {
    sessionStorage.setItem(TOKEN_KEY, token);
  } else {
    sessionStorage.removeItem(TOKEN_KEY);
  }
}

export class ApiError extends Error {
  readonly status: number;
  readonly code: string;

  constructor(status: number, code: string) {
    super(code);
    this.status = status;
    this.code = code;
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(`/api/v1${path}`, {
    ...init,
    headers: {
      "Content-Type": "application/json",
      Authorization: `Bearer ${getToken() ?? ""}`,
      ...(init?.headers ?? {}),
    },
  });
  if (response.status === 401) {
    setToken(null);
    window.dispatchEvent(new Event("auth-expired"));
  }
  const body: unknown = await response.json().catch(() => ({}));
  if (!response.ok) {
    const code =
      typeof body === "object" && body !== null && "error" in body
        ? String((body as { error: unknown }).error)
        : "request_failed";
    throw new ApiError(response.status, code);
  }
  return body as T;
}

export function apiGet<T>(path: string): Promise<T> {
  return request<T>(path);
}

export function apiSend<T>(
  method: "PUT" | "PATCH" | "POST" | "DELETE",
  path: string,
  body?: unknown,
): Promise<T> {
  return request<T>(path, {
    method,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
}

/// Fetch a binary payload (sticker images) as an object URL.
export async function apiBlobUrl(path: string): Promise<string> {
  const response = await fetch(`/api/v1${path}`, {
    headers: { Authorization: `Bearer ${getToken() ?? ""}` },
  });
  if (!response.ok) {
    throw new ApiError(response.status, "image_failed");
  }
  return URL.createObjectURL(await response.blob());
}

// ---------------------------------------------------------------- types

export interface QuotaBucket {
  remaining_fraction: number;
  reset_time: string | null;
}

export interface Quota {
  gemini: {
    five_hour: QuotaBucket | null;
    weekly: QuotaBucket | null;
  };
  fetched_at: string | null;
  error: string | null;
}

export interface Pacing {
  tier: string;
  raw_tier: string;
  degradation_enabled: boolean;
  weekly_pct: number | null;
  five_hour_pct: number | null;
  snapshot_age_s: number | null;
  stale: boolean;
  effective_caps: Array<{ group_id: string; cap: number }>;
}

export interface AuxStatus {
  mode?: string;
  enabled?: boolean;
  ready?: boolean;
  model?: string;
  error?: string | null;
  [key: string]: unknown;
}

export interface Status {
  bot_id: string;
  health: {
    production_ready: boolean;
    blockers: string[];
    aux: AuxStatus | null;
    [key: string]: unknown;
  };
  groups: number;
  lanes: Array<{ group_id: string; queued: number; running: boolean; halted: boolean }>;
  pending_messages: number;
  outbox: Array<{ state: string; count: number }>;
  calls_total: number;
  pacing: Pacing;
}

export interface Group {
  group_id: string;
  enabled: boolean;
  paused: boolean;
  resident_insert_enabled: boolean;
  agy_conversation_id: string | null;
  has_conversation: boolean;
  agy_turn_count: number;
  ambient_pace: string;
  conversation_turn_limit: number | null;
  name: string | null;
  member_count: number | null;
  departed_at: number | null;
  [key: string]: unknown;
}

export interface Memory {
  memory_ref: string;
  kind: string;
  content: string;
  created_at?: number;
  updated_at?: number;
  archived_at?: number | null;
  origin?: string;
  [key: string]: unknown;
}

export interface HotMeme {
  meme_ref: string;
  content: string;
  source: string;
  confidence: number;
  created_at: number;
  updated_at: number;
}

export interface Sticker {
  hash: string;
  short: string;
  kind: string;
  description: string | null;
  emotion: string | null;
  intent: string | null;
  usage_hint: string | null;
  seen_count: number;
  last_seen_at: number;
  enabled: boolean;
  kept: number;
  [key: string]: unknown;
}

export interface Persona {
  persona: string;
  is_default: boolean;
  max_chars: number;
}

export interface AuxSettings {
  mode: string;
  enabled: boolean;
  base_url: string;
  model: string;
  session_header: string;
  api_key_set: boolean;
}

export interface RuntimeSwitches {
  session_rotation_enabled: boolean;
  quota_degradation_enabled: boolean;
}

export interface Settings {
  aux: AuxSettings;
  runtime: RuntimeSwitches;
}
