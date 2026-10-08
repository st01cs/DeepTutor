"use client";

import i18n from "i18next";

import { isDesktopShell, notifyRoundComplete } from "@/lib/desktop-shell";

/** How much of the answer travels in the notification body. */
export const NOTIFICATION_BODY_LIMIT = 180;
/** Longest session title carried in the notification title. */
export const NOTIFICATION_TITLE_LIMIT = 60;

export interface DesktopRoundCompleteInput {
  /** Server session id — the notification's target. Without it there is nowhere to return to. */
  sessionId: string | null;
  /** Session title, when the backend has generated one. */
  sessionTitle?: string | null;
  /** The finished assistant message. */
  content?: string | null;
  /**
   * Whether the app is in the background. Only then is a notification useful:
   * a banner for a round the user just watched finish is noise.
   */
  backgrounded: boolean;
}

/** Collapse whitespace and cut to `limit`, marking the cut with an ellipsis. */
export function clipText(value: string, limit: number): string {
  const collapsed = value.replace(/\s+/g, " ").trim();
  if (collapsed.length <= limit) return collapsed;
  return `${collapsed.slice(0, limit - 1).trimEnd()}…`;
}

/**
 * Post the "round finished" system notification, if the desktop shell owns this
 * window and the user is looking somewhere else.
 *
 * Fire-and-forget on purpose: a notification that fails (permission denied, no
 * shell) must never disturb the turn that just completed. Web and CLI mode are
 * untouched — `isDesktopShell()` is false there, so this returns immediately.
 */
export function notifyDesktopRoundComplete(
  input: DesktopRoundCompleteInput,
): void {
  if (!input.backgrounded) return;
  if (!input.sessionId) return;
  if (!isDesktopShell()) return;
  const title = input.sessionTitle
    ? clipText(input.sessionTitle, NOTIFICATION_TITLE_LIMIT)
    : String(i18n.t("DeepTutor"));
  const body = input.content
    ? clipText(input.content, NOTIFICATION_BODY_LIMIT)
    : String(i18n.t("Round finished"));
  void notifyRoundComplete({
    title,
    body,
    route: `/chat/${input.sessionId}`,
    session_id: input.sessionId,
    kind: "round_complete",
  }).catch(() => {
    /* the turn is already done; a missing banner is not worth surfacing */
  });
}
