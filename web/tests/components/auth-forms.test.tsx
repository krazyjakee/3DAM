import { render, screen } from "@testing-library/react";
import { expect, test } from "vitest";
import { AccountLoginForm } from "../../src/components/auth/AccountLoginForm";
import { ClaimScreen } from "../../src/components/auth/ClaimScreen";
import { TokenLoginForm } from "../../src/components/auth/TokenLoginForm";

test("the account login form renders standalone", () => {
  render(<AccountLoginForm reason={null} allowReadOnly={false} />);

  expect(screen.getByRole("heading", { name: /Sign in to local/i })).toBeInTheDocument();
  expect(screen.getByLabelText("Username")).toBeInTheDocument();
  expect(screen.getByLabelText("Password")).toBeInTheDocument();
});

test("the initial account claim screen renders standalone", () => {
  render(<ClaimScreen />);

  expect(screen.getByRole("dialog", { name: /Claim local/i })).toBeInTheDocument();
  expect(screen.getByLabelText("Username")).toBeInTheDocument();
  expect(screen.getByLabelText("Confirm password")).toBeInTheDocument();
});

test("the token login form renders standalone", () => {
  render(<TokenLoginForm reason="Token rejected" allowReadOnly={false} />);

  expect(screen.getByRole("heading", { name: /Sign in to local/i })).toBeInTheDocument();
  expect(screen.getByRole("alert")).toHaveTextContent("Token rejected");
  expect(screen.getByLabelText("Token")).toBeInTheDocument();
});
