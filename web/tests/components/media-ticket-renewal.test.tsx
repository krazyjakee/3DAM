// Streaming previews carry a five-minute media ticket, and HTMLMediaElement reports its expiry only
// as an opaque `error`. The leaves answer that by asking for a fresh ticket and resuming where they
// were — but that only works if they stay mounted while the new one is in flight. These tests pin
// both halves: the hook keeps the expiring URL on screen during a re-mint, and the audio player
// carries its position and playing state across the swap. Blanking instead (which `Preview` turns
// into "Loading preview…") takes the transport away mid-playback, leaving nothing to pause.

import { HttpResponse, http } from "msw";
import { act, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { AudioPlayer } from "../../src/components/AudioPlayer";
import { useMediaTicket } from "../../src/lib/media-blob";
import { renderApp } from "./render";
import { server } from "./server";

const PATH = "/api/v1/assets/aud-1/content";
const URL_BASE = `http://localhost${PATH}`;

/** Hands out t1, t2, … and lets a test hold the next mint open to inspect the in-flight state. */
function ticketEndpoint() {
  let issued = 0;
  let gate: Promise<void> | null = null;
  let open: (() => void) | null = null;
  server.use(
    http.post("http://localhost/api/v1/media-ticket", async () => {
      if (gate) await gate;
      issued += 1;
      return HttpResponse.json({ ticket: `t${issued}` });
    }),
  );
  return {
    hold() {
      gate = new Promise<void>((resolve) => {
        open = resolve;
      });
    },
    release() {
      open?.();
      gate = null;
      open = null;
    },
  };
}

function TicketProbe({ path, onRenew }: { path: string; onRenew: (fn: () => void) => void }) {
  const ticket = useMediaTicket(path, true);
  onRenew(ticket.renew);
  return <div data-testid="ticket">{`${ticket.status}:${ticket.url ?? "-"}`}</div>;
}

test("a renewal re-mints in the background, keeping the expiring URL on screen", async () => {
  const endpoint = ticketEndpoint();
  let renew = () => {};
  renderApp(<TicketProbe path={PATH} onRenew={(fn) => (renew = fn)} />);

  await waitFor(() =>
    expect(screen.getByTestId("ticket")).toHaveTextContent(`ready:${URL_BASE}?ticket=t1`),
  );

  // Hold the second mint open: this is the window in which the element used to be unmounted.
  endpoint.hold();
  await act(async () => {
    renew();
  });
  expect(screen.getByTestId("ticket")).toHaveTextContent(`ready:${URL_BASE}?ticket=t1`);

  endpoint.release();
  await waitFor(() =>
    expect(screen.getByTestId("ticket")).toHaveTextContent(`ready:${URL_BASE}?ticket=t2`),
  );
});

test("a different target still blanks — the old URL is the wrong media", async () => {
  ticketEndpoint();
  const other = "/api/v1/assets/aud-2/content";
  const otherUrl = `http://localhost${other}`;
  const { rerender } = renderApp(<TicketProbe path={PATH} onRenew={() => {}} />);

  await waitFor(() =>
    expect(screen.getByTestId("ticket")).toHaveTextContent(`ready:${URL_BASE}?ticket=t1`),
  );
  rerender(<TicketProbe path={other} onRenew={() => {}} />);
  expect(screen.getByTestId("ticket")).toHaveTextContent("loading:-");
  await waitFor(() =>
    expect(screen.getByTestId("ticket")).toHaveTextContent(`ready:${otherUrl}?ticket=t2`),
  );
});

let played: HTMLMediaElement[] = [];

beforeEach(() => {
  played = [];
  vi.spyOn(HTMLMediaElement.prototype, "play").mockImplementation(function (
    this: HTMLMediaElement,
  ) {
    played.push(this);
    this.dispatchEvent(new Event("play"));
    return Promise.resolve();
  });
  vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue({
    fillRect: () => {},
    fillStyle: "",
  } as unknown as CanvasRenderingContext2D);
  vi.spyOn(HTMLElement.prototype, "getClientRects").mockImplementation(function (
    this: HTMLElement,
  ) {
    return [this.getBoundingClientRect()] as unknown as DOMRectList;
  });
});

afterEach(() => vi.restoreAllMocks());

test("the player carries its position and playing state across a ticket swap", async () => {
  const view = renderApp(
    <AudioPlayer src={`${PATH}?ticket=t1`} assetId="aud-1" peaks={[0.5, 0.8]} />,
  );
  const audio = view.container.querySelector("audio");
  if (!audio) throw new Error("no <audio>");
  Object.defineProperty(audio, "duration", { configurable: true, value: 90 });

  audio.dispatchEvent(new Event("loadedmetadata"));
  audio.play();
  Object.defineProperty(audio, "currentTime", { configurable: true, writable: true, value: 42 });
  audio.dispatchEvent(new Event("timeupdate"));
  await screen.findByText("0:42 / 1:30");

  // The renewed ticket arrives as a new `src` on the same element; the load algorithm zeroes the
  // clock and pauses before the replacement reports metadata.
  view.rerender(<AudioPlayer src={`${PATH}?ticket=t2`} assetId="aud-1" peaks={[0.5, 0.8]} />);
  audio.currentTime = 0;
  audio.dispatchEvent(new Event("timeupdate"));
  audio.dispatchEvent(new Event("pause"));
  audio.dispatchEvent(new Event("loadedmetadata"));

  expect(audio.currentTime).toBe(42);
  expect(played).toEqual([audio, audio]);
});
