"use client";

import { useCallback, useEffect, useRef } from "react";
import { usePathname, useRouter } from "next/navigation";

import {
  DESKTOP_ATTACH_EVENT,
  SHELL_EVENTS,
  fileFromLocalFile,
  isDesktopShell,
  noteUiReady,
  onShellEvent,
  readLocalFile,
  shellLog,
  takeNotificationTarget,
  takeOpenRequest,
  type DesktopAttachDetail,
} from "@/lib/desktop-shell";

/** How often the queues are re-checked when the event channel is unavailable. */
const POLL_INTERVAL_MS = 5000;
/** Guard against a runaway queue draining forever in one tick. */
const MAX_PER_DRAIN = 8;
/** Where a handed-over file belongs when the user is somewhere else. */
const CHAT_ROUTE = "/chat";

/**
 * Drains the shell's queues into the running web app.
 *
 * Two kinds of work arrive here:
 *
 * * **open requests** — a `deeptutor://` link, a file association, or a file
 *   dropped on the Dock icon. Links become navigations; files are read through
 *   the shell and re-broadcast as `File` objects, which is the currency the
 *   composer already accepts.
 * * **notification targets** — when a backgrounded round finished, clicking the
 *   system notification brings the app forward, and this is what turns that
 *   into "open the session you were told about".
 *
 * The shell also emits an event for both, but `withGlobalTauri` only guarantees
 * the core module, so the queues are also polled. Delivery is idempotent: both
 * shell commands are destructive `take`s, so an event that arrives right after a
 * poll finds nothing to hand over twice.
 */
export default function DesktopBridge() {
  const router = useRouter();
  const pathname = usePathname();
  const pendingFiles = useRef<DesktopAttachDetail[]>([]);
  const draining = useRef(false);
  const pathnameRef = useRef(pathname);

  pathnameRef.current = pathname;

  const go = useCallback(
    (route: string) => {
      if (!route) return;
      router.push(route);
    },
    [router],
  );

  /** Hand queued files to the composer; unsent files stay queued. */
  const flushFiles = useCallback(() => {
    if (!pendingFiles.current.length) return;
    const remaining: DesktopAttachDetail[] = [];
    for (const detail of pendingFiles.current) {
      const delivered = { ...detail, acknowledged: false };
      window.dispatchEvent(
        new CustomEvent(DESKTOP_ATTACH_EVENT, { detail: delivered }),
      );
      // The composer sets `acknowledged` synchronously when it takes the files;
      // otherwise (wrong page, still mounting) the batch is retried.
      if (!delivered.acknowledged) {
        remaining.push(detail);
      }
    }
    pendingFiles.current = remaining;
  }, []);

  const drainOpenRequests = useCallback(async () => {
    for (let guard = 0; guard < MAX_PER_DRAIN; guard += 1) {
      const request = await takeOpenRequest();
      if (!request) return;
      if (request.kind === "route" && request.route) {
        go(request.route);
        continue;
      }
      if (request.kind !== "file" || !request.path) continue;
      try {
        const payload = await readLocalFile(request.path);
        pendingFiles.current.push({
          files: [fileFromLocalFile(payload)],
          source: request.source,
        });
        void shellLog(
          `handed ${payload.name} to the composer (${request.source})`,
        );
      } catch (error) {
        void shellLog(`could not read ${request.path}: ${String(error)}`);
      }
    }
  }, [go]);

  const drainNotificationTarget = useCallback(async () => {
    const target = await takeNotificationTarget();
    if (target?.route) go(target.route);
  }, [go]);

  const drainAll = useCallback(async () => {
    if (draining.current) return;
    draining.current = true;
    try {
      await drainOpenRequests();
      await drainNotificationTarget();
      flushFiles();
    } catch (error) {
      void shellLog(`desktop bridge drain failed: ${String(error)}`);
    } finally {
      draining.current = false;
    }
  }, [drainNotificationTarget, drainOpenRequests, flushFiles]);

  useEffect(() => {
    if (!isDesktopShell()) return;
    // Startup timing: the shell owns the launcher half of the launch, this page
    // owns the render half, and `performance.now()` is the only clock they share.
    void noteUiReady(Math.round(performance.now())).catch((error: unknown) => {
      // Never let a timing ping fail quietly: silence here once cost an hour of
      // wondering why a launch line never appeared.
      void shellLog(`startup ping failed: ${String(error)}`);
    });
    void shellLog("web ui connected (desktop shell)");
    void drainAll();

    const unsubscribe: Array<() => void> = [
      onShellEvent(SHELL_EVENTS.openRequest, () => void drainAll()),
      onShellEvent(SHELL_EVENTS.notificationTarget, () => void drainAll()),
      // Settings changed from the menu bar: re-broadcast for the open settings
      // page, which would otherwise show a stale toggle.
      onShellEvent(SHELL_EVENTS.shellSettings, (payload) => {
        window.dispatchEvent(
          new CustomEvent("deeptutor:shell-settings", { detail: payload }),
        );
      }),
      onShellEvent(SHELL_EVENTS.updateReport, (payload) => {
        window.dispatchEvent(
          new CustomEvent("deeptutor:update-report", { detail: payload }),
        );
      }),
      // Exports: the shell picks the destination, so it is the only side that
      // knows where the file landed.
      onShellEvent(SHELL_EVENTS.downloadFinished, (payload) => {
        window.dispatchEvent(
          new CustomEvent("deeptutor:download-finished", { detail: payload }),
        );
      }),
    ];

    const onFocus = () => void drainAll();
    window.addEventListener("focus", onFocus);
    document.addEventListener("visibilitychange", onFocus);
    const onComposerReady = () => flushFiles();
    window.addEventListener("deeptutor:desktop-attach-ready", onComposerReady);
    const timer = window.setInterval(() => void drainAll(), POLL_INTERVAL_MS);

    return () => {
      for (const stop of unsubscribe) stop();
      window.removeEventListener("focus", onFocus);
      document.removeEventListener("visibilitychange", onFocus);
      window.removeEventListener(
        "deeptutor:desktop-attach-ready",
        onComposerReady,
      );
      window.clearInterval(timer);
    };
  }, [drainAll, flushFiles]);

  // A file hand-off that arrives while the user is elsewhere is attached to the
  // conversation, so take them there before the composer is asked to take it.
  useEffect(() => {
    if (!isDesktopShell()) return;
    if (!pendingFiles.current.length) return;
    if (!pathnameRef.current?.startsWith(CHAT_ROUTE)) go(CHAT_ROUTE);
    flushFiles();
  }, [flushFiles, go, pathname]);

  return null;
}
