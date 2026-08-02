import { HttpResponse, http } from "msw";
import { screen } from "@testing-library/react";
import { expect, test } from "vitest";
import { Upload } from "../../src/components/Upload";
import { renderApp } from "./render";
import { server } from "./server";

test("the upload picker explains grant denials and never enables federated sources", async () => {
  server.use(
    http.get("http://localhost/api/version", () =>
      HttpResponse.json({
        api: "v1",
        server: "test",
        capabilities: [],
        auth: "accounts",
        accounts: true,
        upload: true,
      }),
    ),
    http.get("http://localhost/api/v1/whoami", () =>
      HttpResponse.json({ identity: "erin", anonymous: false, scopes: ["read", "write"] }),
    ),
    http.get("http://localhost/api/v1/sources", () =>
      HttpResponse.json([
        {
          id: "writable-source",
          kind: "local_fs",
          uri: "/assets/writable",
          name: "Writable library",
          options: {},
          writable: true,
        },
        {
          id: "read-share",
          kind: "local_fs",
          uri: "/assets/read-only",
          name: "Read share",
          options: {},
          writable: false,
          writable_reason: "read-only — a write share is required",
        },
        {
          id: "peer-source",
          kind: "federated",
          uri: "peer://library",
          name: "Peer library",
          options: {},
          writable: false,
          writable_reason: "read-only — a write share is required",
        },
      ]),
    ),
  );

  renderApp(<Upload />, { route: "/upload" });

  expect(
    await screen.findByRole("button", { name: /Writable library/i }),
  ).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: /Read share/i })).not.toBeInTheDocument();
  expect(screen.getByText("read-only — a write share is required")).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: /Peer library/i })).not.toBeInTheDocument();
  expect(screen.getByText("a peer's library is read-only")).toBeInTheDocument();
});
