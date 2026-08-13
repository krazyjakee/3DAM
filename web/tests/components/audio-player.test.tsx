// The Inspector renders twice at once — the `lg` rail and the responsive drawer are both mounted —
// so a selected audio asset always has two <audio> elements in the DOM. A hidden one still makes
// sound, so any path that starts playback must address only the player on screen; otherwise a
// double-click plays the file twice, a beat apart, and pausing the visible player leaves the other
// one running. These tests mount both copies the way the workspace does and lock that in.

import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { act } from "@testing-library/react";
import { AudioPlayer } from "../../src/components/AudioPlayer";
import { requestAutoplay } from "../../src/lib/audio-intent";
import { emitShortcut } from "../../src/lib/shortcuts";
import { renderApp } from "./render";

const SRC = "/api/v1/assets/aud-1/content";

let played: HTMLMediaElement[] = [];
let paused: HTMLMediaElement[] = [];

beforeEach(() => {
  played = [];
  paused = [];
  // jsdom implements neither playback nor Canvas2D; the waveform swallows its own paint failure.
  vi.spyOn(HTMLMediaElement.prototype, "play").mockImplementation(function (
    this: HTMLMediaElement,
  ) {
    played.push(this);
    this.dispatchEvent(new Event("play"));
    return Promise.resolve();
  });
  vi.spyOn(HTMLMediaElement.prototype, "pause").mockImplementation(function (
    this: HTMLMediaElement,
  ) {
    paused.push(this);
    this.dispatchEvent(new Event("pause"));
  });
  // `paused` is read-only in jsdom, and the toggle branches on it: report the mocked state.
  Object.defineProperty(HTMLMediaElement.prototype, "paused", {
    configurable: true,
    get: function (this: HTMLMediaElement) {
      return !played.includes(this) || paused.includes(this);
    },
  });
  // The waveform canvas is incidental here, but jsdom ships no 2D context and its paint throws.
  vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue({
    fillRect: () => {},
    fillStyle: "",
  } as unknown as CanvasRenderingContext2D);
  // jsdom has no layout, so getClientRects() is empty for every element. Production reads it to tell
  // the mounted-but-hidden region from the one on screen — approximate the browser: no rects inside
  // a display:none subtree, one rect otherwise.
  const hiddenByAncestor = (el: HTMLElement | null): boolean =>
    el !== null && (el.style.display === "none" || hiddenByAncestor(el.parentElement));
  vi.spyOn(HTMLElement.prototype, "getClientRects").mockImplementation(function (
    this: HTMLElement,
  ) {
    if (hiddenByAncestor(this)) return [] as unknown as DOMRectList;
    return [this.getBoundingClientRect()] as unknown as DOMRectList;
  });
});

afterEach(() => {
  vi.restoreAllMocks();
});

/** The rail and the drawer, as the workspace mounts them: same asset, same src, one of them hidden
 *  (`lg:hidden` / `hidden lg:flex` in production, an inline display:none here). */
function renderBothInspectorCopies() {
  const view = renderApp(
    <>
      <div data-testid="rail">
        <AudioPlayer src={SRC} assetId="aud-1" peaks={[0.5, 0.8]} />
      </div>
      <div data-testid="drawer" style={{ display: "none" }}>
        <AudioPlayer src={SRC} assetId="aud-1" peaks={[0.5, 0.8]} />
      </div>
    </>,
  );
  const audioOf = (testId: string) => {
    const el = view.getByTestId(testId).querySelector("audio");
    if (!el) throw new Error(`no <audio> in ${testId}`);
    return el;
  };
  return { ...view, rail: audioOf("rail"), drawer: audioOf("drawer") };
}

test("a double-click autoplay request starts only the player the user can see", () => {
  const { rail } = renderBothInspectorCopies();

  act(() => requestAutoplay("aud-1"));

  expect(played).toEqual([rail]);
});

test("a player that is hidden by a layout change hands playback back", () => {
  const view = renderBothInspectorCopies();

  act(() => emitShortcut("play-pause"));
  expect(played).toEqual([view.rail]);

  // Crossing the `lg` breakpoint hides the rail and reveals the drawer without unmounting either —
  // whoever was playing must stop, since the controls now address the other copy.
  act(() => {
    view.getByTestId("rail").style.display = "none";
    view.getByTestId("drawer").style.display = "";
    window.dispatchEvent(new Event("resize"));
  });

  expect(paused).toEqual([view.rail]);
});

test("the play/pause shortcut drives the same single visible player", () => {
  const { rail, drawer } = renderBothInspectorCopies();

  act(() => emitShortcut("play-pause"));
  expect(played).toEqual([rail]);

  act(() => emitShortcut("play-pause"));
  expect(paused).toEqual([rail]);
  expect(played).not.toContain(drawer);
});
