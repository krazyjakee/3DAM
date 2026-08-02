import { HttpResponse, http } from "msw";
import { screen } from "@testing-library/react";
import { describe, expect, test } from "vitest";
import { AuthGate } from "../../src/components/AuthGate";
import { setServer } from "../../src/lib/server";
import { renderApp } from "./render";
import { server } from "./server";

const version = (auth: "off" | "anonymous" | "token") => ({
  api: "v1",
  server: "test",
  capabilities: [],
  auth,
  accounts: false,
});

describe("authentication gate", () => {
  test("a token server never mounts the workspace before sign-in", async () => {
    let protectedRequests = 0;
    server.use(
      http.get("http://localhost/api/version", () => HttpResponse.json(version("token"))),
      http.get("http://localhost/api/v1/stats", () => {
        protectedRequests += 1;
        return HttpResponse.json({});
      }),
    );

    renderApp(
      <AuthGate>
        <main>Private workspace</main>
      </AuthGate>,
    );

    expect(await screen.findByRole("dialog", { name: /sign in to local/i })).toBeInTheDocument();
    expect(screen.queryByText("Private workspace")).not.toBeInTheDocument();
    expect(protectedRequests).toBe(0);
  });

  test("a validated bearer mounts the workspace and stays at the request boundary", async () => {
    setServer("", "valid-test-token");
    let authorization: string | null = null;
    server.use(
      http.get("http://localhost/api/version", () => HttpResponse.json(version("token"))),
      http.get("http://localhost/api/v1/stats", ({ request }) => {
        authorization = request.headers.get("authorization");
        return HttpResponse.json({ assets: 0 });
      }),
    );

    renderApp(
      <AuthGate>
        <main>Private workspace</main>
      </AuthGate>,
    );

    expect(await screen.findByText("Private workspace")).toBeInTheDocument();
    expect(authorization).toBe("Bearer valid-test-token");
    expect(screen.queryByRole("dialog", { name: /sign in/i })).not.toBeInTheDocument();
  });

  test("a rejected stored credential returns the user to the login surface", async () => {
    setServer("", "expired-test-token");
    server.use(
      http.get("http://localhost/api/version", () => HttpResponse.json(version("token"))),
      http.get("http://localhost/api/v1/stats", () =>
        HttpResponse.json(
          { code: "unauthorized", message: "expired" },
          { status: 401 },
        ),
      ),
    );

    renderApp(
      <AuthGate>
        <main>Private workspace</main>
      </AuthGate>,
    );

    expect(await screen.findByRole("dialog", { name: /sign in to local/i })).toBeInTheDocument();
    expect(screen.getByText(/token was rejected/i)).toBeInTheDocument();
    expect(screen.queryByText("Private workspace")).not.toBeInTheDocument();
  });
});
