import assert from "node:assert/strict";
import test from "node:test";
import {
  DEFAULT_VIEWER_POSE,
  MAX_PITCH,
  MIN_ZOOM,
  keyAction,
  keyboard,
  orbit,
  pan,
  pinchPan,
  zoom,
} from "../src/lib/viewer-gestures.ts";

test("orbit clamps pitch and preserves pan/zoom", () => {
  const pose = { ...DEFAULT_VIEWER_POSE, zoom: 2, panX: 0.25 };
  const next = orbit(pose, 10, 10_000);
  assert.equal(next.yaw, pose.yaw + 0.1);
  assert.equal(next.pitch, MAX_PITCH);
  assert.equal(next.zoom, 2);
  assert.equal(next.panX, 0.25);
});

test("pinch zoom and midpoint pan compose without canvas-size-dependent drift", () => {
  const next = pinchPan(
    DEFAULT_VIEWER_POSE,
    [
      { x: 20, y: 30 },
      { x: 80, y: 30 },
    ],
    [
      { x: 10, y: 40 },
      { x: 110, y: 40 },
    ],
    200,
    100,
  );
  assert.equal(next.zoom, 0.6);
  assert.equal(next.panX, -0.05);
  assert.equal(next.panY, 0.1);
});

test("zoom and pan are bounded for hostile or runaway input", () => {
  assert.equal(zoom(DEFAULT_VIEWER_POSE, -1_000_000).zoom, MIN_ZOOM);
  assert.equal(pan(DEFAULT_VIEWER_POSE, -10_000, 10_000, 1, 1).panX, 2);
  assert.equal(pan(DEFAULT_VIEWER_POSE, -10_000, 10_000, 1, 1).panY, 2);
});

test("keyboard map preserves page keys unless the focused viewer recognizes them", () => {
  assert.equal(keyAction("ArrowLeft", false), "orbit-left");
  assert.equal(keyAction("ArrowLeft", true), "pan-left");
  assert.equal(keyAction("Home", false), "reset");
  assert.equal(keyAction("Tab", false), null);
  assert.deepEqual(keyboard({ ...DEFAULT_VIEWER_POSE, zoom: 3 }, "reset"), DEFAULT_VIEWER_POSE);
});
