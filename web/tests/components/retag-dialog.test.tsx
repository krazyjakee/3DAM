import { fireEvent, screen, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, test } from "vitest";
import { RetagDialog } from "../../src/components/RetagDialog";
import { renderApp } from "./render";
import { server } from "./server";

test("batch retag previews, applies, and reports excluded targets", async () => {
  const requests: Array<{ dry_run?: boolean; add?: string[] }> = [];
  server.use(
    http.post("http://localhost/api/v1/tags/list", () =>
      HttpResponse.json([{ name: "approved", count: 4, manual: true }]),
    ),
    http.post("http://localhost/api/v1/tags/edit", async ({ request }) => {
      const body = (await request.json()) as { dry_run?: boolean; add?: string[] };
      requests.push(body);
      return HttpResponse.json({
        matched: 1,
        changed: 1,
        additions: 1,
        removals: 0,
        warnings: [
          {
            subject: "peer",
            code: "target_read_only",
            message: "Asset is readable but requires a write share",
          },
        ],
      });
    }),
  );

  renderApp(
    <RetagDialog scope={{ assets: ["local"] }} excludedPeers={1} onClose={() => {}} />,
  );
  expect(screen.getByText(/1 federated target is read-only and excluded/i)).toBeInTheDocument();
  fireEvent.change(screen.getByLabelText("Add manual tags"), {
    target: { value: "Approved, approved" },
  });
  fireEvent.click(screen.getByRole("button", { name: "Preview" }));
  expect(await screen.findByText(/Preview: 1 of 1 assets change/i)).toBeInTheDocument();
  expect(screen.getByText(/requires a write share/i)).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Apply" }));
  expect(await screen.findByText(/Applied: 1 of 1 assets change/i)).toBeInTheDocument();
  await waitFor(() => expect(requests).toHaveLength(2));
  expect(requests[0]).toMatchObject({ dry_run: true, add: ["approved"] });
  expect(requests[1]).toMatchObject({ dry_run: false, add: ["approved"] });
});
