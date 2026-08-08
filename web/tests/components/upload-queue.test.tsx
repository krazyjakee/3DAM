import type { ReactNode } from "react";
import { act, renderHook, waitFor } from "@testing-library/react";
import { QueryClientProvider } from "@tanstack/react-query";
import { beforeEach, describe, expect, test, vi } from "vitest";
import { api } from "../../src/api/client";
import type { UploadOutcome } from "../../src/api/types";
import { toast } from "../../src/lib/toast";
import {
  CONCURRENCY,
  useUploadQueue,
} from "../../src/components/upload/useUploadQueue";
import { testQueryClient } from "./render";

const destination = { source: "source-1", folder: "incoming", collision: "suffix" } as const;

function file(name: string): File {
  return new File([name], name, { type: "application/octet-stream" });
}

function outcome(name: string): UploadOutcome {
  return { path: name, skipped: false, size: name.length, asset: null };
}

function renderQueue() {
  const client = testQueryClient();
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
  return renderHook(() => useUploadQueue(), { wrapper });
}

describe("upload queue", () => {
  beforeEach(() => {
    vi.spyOn(toast, "success").mockImplementation(() => 0);
  });

  test(`runs at most ${CONCURRENCY} uploads concurrently`, async () => {
    let active = 0;
    let peak = 0;
    const releases: Array<() => void> = [];
    const upload = vi.spyOn(api, "upload").mockImplementation((request) => {
      active += 1;
      peak = Math.max(peak, active);
      return new Promise((resolve) => {
        releases.push(() => {
          active -= 1;
          resolve(outcome(request.name));
        });
      });
    });
    const { result } = renderQueue();

    act(() => result.current.addFiles(Array.from({ length: 8 }, (_, i) => file(`${i}.glb`))));
    let run!: Promise<void>;
    act(() => {
      run = result.current.run(destination);
    });

    await waitFor(() => expect(upload).toHaveBeenCalledTimes(3));
    expect(peak).toBe(CONCURRENCY);

    await act(async () => {
      releases.splice(0).forEach((release) => release());
    });
    await waitFor(() => expect(upload).toHaveBeenCalledTimes(6));
    expect(peak).toBe(CONCURRENCY);

    await act(async () => {
      releases.splice(0).forEach((release) => release());
    });
    await waitFor(() => expect(upload).toHaveBeenCalledTimes(8));
    expect(peak).toBe(CONCURRENCY);

    await act(async () => {
      releases.splice(0).forEach((release) => release());
      await run;
    });

    expect(result.current.running).toBe(false);
    expect(result.current.items.every((item) => item.state === "done")).toBe(true);
  });

  test("removing a queued row during a run cancels it before a worker sends it", async () => {
    const releases: Array<() => void> = [];
    const upload = vi.spyOn(api, "upload").mockImplementation((request) =>
      new Promise((resolve) => {
        releases.push(() => resolve(outcome(request.name)));
      }),
    );
    const { result } = renderQueue();

    act(() =>
      result.current.addFiles([
        file("one.glb"),
        file("two.glb"),
        file("three.glb"),
        file("cancel.glb"),
      ]),
    );
    const cancelledId = result.current.items[3].id;
    let run!: Promise<void>;
    act(() => {
      run = result.current.run(destination);
    });
    await waitFor(() => expect(upload).toHaveBeenCalledTimes(CONCURRENCY));

    act(() => result.current.remove(cancelledId));
    expect(result.current.items.map((item) => item.file.name)).not.toContain("cancel.glb");

    await act(async () => {
      releases.splice(0).forEach((release) => release());
      await run;
    });

    expect(upload.mock.calls.map(([request]) => request.name)).toEqual([
      "one.glb",
      "two.glb",
      "three.glb",
    ]);
  });

  test("workers consume the ref-backed queue when state gains a file mid-run", async () => {
    const releases: Array<() => void> = [];
    const upload = vi.spyOn(api, "upload").mockImplementation((request) =>
      new Promise((resolve) => {
        releases.push(() => resolve(outcome(request.name)));
      }),
    );
    const { result } = renderQueue();

    act(() =>
      result.current.addFiles([file("one.glb"), file("two.glb"), file("three.glb")]),
    );
    let run!: Promise<void>;
    act(() => {
      run = result.current.run(destination);
    });
    await waitFor(() => expect(upload).toHaveBeenCalledTimes(CONCURRENCY));

    act(() => result.current.addFiles([file("late.glb")]));
    expect(result.current.items.at(-1)).toMatchObject({
      state: "queued",
      file: expect.objectContaining({ name: "late.glb" }),
    });

    await act(async () => {
      releases.shift()?.();
    });
    await waitFor(() => expect(upload).toHaveBeenCalledTimes(CONCURRENCY + 1));
    expect(upload.mock.calls.at(-1)?.[0].name).toBe("late.glb");

    await act(async () => {
      releases.splice(0).forEach((release) => release());
      await run;
    });
    expect(result.current.items.at(-1)?.state).toBe("done");
  });
});
