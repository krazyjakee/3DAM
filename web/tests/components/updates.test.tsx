import { afterEach, describe, expect, it, vi } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Updates } from "../../src/components/Updates";
import { UpdateNotice } from "../../src/components/UpdateNotice";
import type { DesktopUpdateStatus } from "../../src/api/types";
import { renderApp } from "./render";

const base: DesktopUpdateStatus = {
  current_version: "0.1.5", supported: true, unavailable_reason: null,
  automatic_checks: true, phase: "idle", release: null, checked_at: null,
  downloaded_bytes: 0, total_bytes: null, error: null,
};

function native(status: DesktopUpdateStatus = base) {
  window.__3DAM_EMBEDDED_SERVER__ = true;
  const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
    if (command === "desktop_update_check") return { ...status, phase: "checking" };
    if (command === "desktop_update_install") return { ...status, phase: "downloading" };
    if (command === "desktop_update_preferences") return { ...status, automatic_checks: args?.automaticChecks };
    if (command === "desktop_update_restart") return undefined;
    return status;
  });
  window.__TAURI__ = { core: { invoke: <T,>(command: string, args?: Record<string, unknown>) => invoke(command, args) as Promise<T> } };
  return invoke;
}

afterEach(() => {
  delete window.__TAURI__;
  delete window.__3DAM_EMBEDDED_SERVER__;
});

describe("desktop updates", () => {
  it("explains browser and hosted usage without invoking native commands", () => {
    const invoke = native();
    window.__3DAM_EMBEDDED_SERVER__ = false;
    renderApp(<Updates />);
    expect(screen.getByText(/Open 3DAM with its local library/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Check for updates" })).not.toBeInTheDocument();
    expect(invoke).not.toHaveBeenCalled();
  });

  it("checks manually and displays a busy state", async () => {
    const user = userEvent.setup();
    const invoke = native();
    renderApp(<Updates />);
    await user.click(await screen.findByRole("button", { name: "Check for updates" }));
    expect(invoke).toHaveBeenCalledWith("desktop_update_check", undefined);
    expect(await screen.findByText("Checking for updates…")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Check for updates" })).toBeDisabled();
  });

  it("renders release notes as text and requires confirmation before installing", async () => {
    const user = userEvent.setup();
    const invoke = native({ ...base, phase: "available", release: { version: "0.2.0", notes: "<script>unsafe()</script>", date: null } });
    renderApp(<Updates />);
    const button = await screen.findByRole("button", { name: "Download and install 0.2.0" });
    expect(screen.getByText("<script>unsafe()</script>")).toBeInTheDocument();
    await user.click(button);
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    expect(invoke).not.toHaveBeenCalledWith("desktop_update_install", undefined);
    await user.click(button);
    await user.click(screen.getByRole("button", { name: "Download and install" }));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("desktop_update_install", undefined));
    expect(await screen.findByRole("progressbar", { name: "Update download" })).not.toHaveAttribute("value");
  });

  it("shows measured download progress and prevents duplicate actions", async () => {
    native({ ...base, phase: "downloading", downloaded_bytes: 50, total_bytes: 100 });
    renderApp(<Updates />);
    expect(await screen.findByRole("progressbar")).toHaveAttribute("value", "50");
    expect(screen.getByRole("button", { name: "Check for updates" })).toBeDisabled();
  });

  it("persists the automatic check preference through native state", async () => {
    const user = userEvent.setup();
    const invoke = native();
    renderApp(<Updates />);
    const checkbox = await screen.findByRole("checkbox", { name: /Automatically check for updates/ });
    await user.click(checkbox);
    expect(invoke).toHaveBeenCalledWith("desktop_update_preferences", { automaticChecks: false });
    await waitFor(() => expect(checkbox).not.toBeChecked());
  });

  it("shows failed checks and lets the user retry", async () => {
    native({ ...base, phase: "error", error: "Could not check for updates: offline" });
    renderApp(<Updates />);
    expect(await screen.findByRole("alert")).toHaveTextContent("offline");
    expect(screen.getByRole("button", { name: "Check for updates" })).toBeEnabled();
  });

  it("offers manual downloads for package-managed installations", async () => {
    native({ ...base, supported: false, unavailable_reason: "Use your package manager." });
    renderApp(<Updates />);
    expect(await screen.findByText("Manual update required")).toBeInTheDocument();
    expect(screen.getByText("Use your package manager.")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /View releases and downloads/ })).toHaveAttribute("href", "https://github.com/krazyjakee/3DAM/releases");
    expect(screen.queryByRole("checkbox")).not.toBeInTheDocument();
  });

  it("restarts only after installation and shows restart errors", async () => {
    const user = userEvent.setup();
    const invoke = native({ ...base, phase: "ready" });
    invoke.mockImplementation(async (command) => {
      if (command === "desktop_update_restart") throw new Error("Restart failed");
      return { ...base, phase: "ready" };
    });
    renderApp(<Updates />);
    await user.click(await screen.findByRole("button", { name: "Restart 3DAM" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Restart failed");
    expect(screen.queryByRole("button", { name: "Check for updates" })).not.toBeInTheDocument();
  });

  it("announces available updates with a link to review", async () => {
    native({ ...base, phase: "available", release: { version: "0.2.0", notes: null, date: null } });
    renderApp(<UpdateNotice />);
    expect(await screen.findByText("3DAM 0.2.0 is available.")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Review update" })).toHaveAttribute("href", "/updates");
  });

  it("handles failed IPC status calls with a retry", async () => {
    const user = userEvent.setup();
    const invoke = native();
    invoke.mockRejectedValueOnce(new Error("IPC unavailable"));
    renderApp(<Updates />);
    expect(await screen.findByRole("alert")).toHaveTextContent("IPC unavailable");
    await user.click(screen.getByRole("button", { name: "Retry" }));
    expect(await screen.findByText("Ready to check for updates")).toBeInTheDocument();
  });
});
