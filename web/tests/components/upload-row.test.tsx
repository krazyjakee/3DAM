import { fireEvent, render, screen } from "@testing-library/react";
import { expect, test, vi } from "vitest";
import { UploadRow } from "../../src/components/upload/UploadRow";
import type { UploadItem } from "../../src/components/upload/useUploadQueue";

const base: UploadItem = {
  id: "row-1",
  file: new File(["asset"], "asset.glb"),
  state: "queued",
  progress: 0,
};

function row(item: UploadItem, onRemove = vi.fn()) {
  return render(
    <ul>
      <UploadRow item={item} onRemove={onRemove} />
    </ul>,
  );
}

test("an upload row renders every queue state and keeps removal scoped to removable rows", () => {
  const onRemove = vi.fn();
  const view = row(base, onRemove);

  fireEvent.click(screen.getByRole("button", { name: "Remove asset.glb" }));
  expect(onRemove).toHaveBeenCalledOnce();

  view.rerender(
    <ul>
      <UploadRow item={{ ...base, state: "uploading", progress: 0.42 }} onRemove={onRemove} />
    </ul>,
  );
  expect(screen.getByRole("progressbar")).toHaveAttribute("aria-valuenow", "42");
  expect(screen.queryByRole("button", { name: /Remove/ })).not.toBeInTheDocument();

  view.rerender(
    <ul>
      <UploadRow
        item={{
          ...base,
          state: "done",
          progress: 1,
          outcome: {
            path: "incoming/asset.glb",
            skipped: false,
            size: 5,
            uncatalogued_reason: "Unsupported format",
          },
        }}
        onRemove={onRemove}
      />
    </ul>,
  );
  expect(screen.getByTitle("Unsupported format")).toBeInTheDocument();
  expect(screen.getByRole("status")).toHaveTextContent("incoming/asset.glb uploaded");

  view.rerender(
    <ul>
      <UploadRow item={{ ...base, state: "skipped" }} onRemove={onRemove} />
    </ul>,
  );
  expect(screen.getByText(/skipped —/)).toBeInTheDocument();

  view.rerender(
    <ul>
      <UploadRow item={{ ...base, state: "error", error: "Network failed" }} onRemove={onRemove} />
    </ul>,
  );
  expect(screen.getByTitle("Network failed")).toBeInTheDocument();
  expect(screen.getByRole("status")).toHaveTextContent("asset.glb upload failed: Network failed");
});
