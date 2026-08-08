import { act, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { ModelViewerHandle } from "../../src/islands";
import { ModelViewerIsland } from "../../src/islands/ModelViewerIsland";

const viewerMocks = vi.hoisted(() => ({
  create: vi.fn(),
}));

vi.mock("../../src/islands/index", () => ({
  createModelViewer: viewerMocks.create,
}));

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("ModelViewerIsland first frame", () => {
  it("applies a resize observed during async setup and requests a redraw when ready", async () => {
    let notifyResize: ResizeObserverCallback | null = null;
    class ControlledResizeObserver implements ResizeObserver {
      constructor(callback: ResizeObserverCallback) {
        notifyResize = callback;
      }
      disconnect() {}
      observe() {}
      unobserve() {}
    }
    vi.stubGlobal("ResizeObserver", ControlledResizeObserver);

    let resolveViewer!: (handle: ModelViewerHandle) => void;
    viewerMocks.create.mockReturnValue(
      new Promise<ModelViewerHandle>((resolve) => {
        resolveViewer = resolve;
      }),
    );
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response(new Uint8Array([1, 2, 3]), { status: 200 }),
    );

    const handle: ModelViewerHandle = {
      backend: "webgpu",
      antialiasingSamples: 4,
      sourceDrawCount: 1,
      batchedDrawCount: 1,
      framingVersion: 1,
      loadPreviewMesh: vi.fn(),
      setCamera: vi.fn(),
      setCameraPose: vi.fn(),
      setLighting: vi.fn(),
      setWireframe: vi.fn(),
      resize: vi.fn(),
      requestRedraw: vi.fn(),
      hasModel: vi.fn(() => true),
      free: vi.fn(),
    };

    render(<ModelViewerIsland src="/model.preview-mesh" />);
    const canvas = screen.getByRole("region");
    const container = canvas.parentElement as HTMLDivElement;
    let width = 640;
    let height = 480;
    Object.defineProperties(container, {
      clientWidth: { configurable: true, get: () => width },
      clientHeight: { configurable: true, get: () => height },
    });

    // This is the lost-notification sequence from #186: fitCanvas updates the backing store while
    // createModelViewer is unresolved and there is no handle to resize yet.
    act(() => {
      notifyResize?.([], {} as ResizeObserver);
    });
    expect(canvas).toHaveAttribute("width", "640");
    expect(canvas).toHaveAttribute("height", "480");

    await act(async () => {
      resolveViewer(handle);
    });
    await screen.findByRole("button", { name: /Studio lighting/ });

    expect(handle.resize).toHaveBeenCalledWith(640, 480);
    expect(handle.loadPreviewMesh).toHaveBeenCalledWith(new Uint8Array([1, 2, 3]));
    expect(handle.requestRedraw).toHaveBeenCalledTimes(1);
    expect(vi.mocked(handle.resize).mock.invocationCallOrder[0]).toBeLessThan(
      vi.mocked(handle.loadPreviewMesh).mock.invocationCallOrder[0],
    );

    // Later drawer/layout changes still reconfigure the surface, but duplicate observations do not.
    width = 800;
    height = 600;
    act(() => notifyResize?.([], {} as ResizeObserver));
    expect(handle.resize).toHaveBeenLastCalledWith(800, 600);
    act(() => notifyResize?.([], {} as ResizeObserver));
    expect(handle.resize).toHaveBeenCalledTimes(2);
  });
});
