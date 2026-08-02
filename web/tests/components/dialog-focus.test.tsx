import { useState } from "react";
import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, test } from "vitest";
import { useDialogs } from "../../src/lib/dialogs";
import { useFocusTrap } from "../../src/lib/use-focus-trap";
import { renderApp } from "./render";

function ConfirmationHarness() {
  const { confirm } = useDialogs();
  const [result, setResult] = useState("none");
  return (
    <>
      <button
        type="button"
        onClick={() =>
          void confirm({
            title: "Delete permanently?",
            message: "This cannot be undone.",
            confirmLabel: "Delete",
            danger: true,
          }).then((accepted) => setResult(accepted ? "accepted" : "cancelled"))
        }
      >
        Open confirmation
      </button>
      <output aria-label="Result">{result}</output>
    </>
  );
}

function TransitionHarness() {
  const [open, setOpen] = useState(false);
  const ref = useFocusTrap<HTMLDivElement>(open);
  return (
    <>
      <button type="button" onClick={() => setOpen(true)}>Open from first</button>
      <button type="button" onClick={() => setOpen(true)}>Open from second</button>
      <div ref={ref} hidden={!open}>
        <button type="button" onClick={() => setOpen(false)}>Close trap</button>
        <button type="button">Last action</button>
      </div>
    </>
  );
}

test("danger dialogs trap keyboard focus, cancel on Escape, and restore the trigger", async () => {
  const user = userEvent.setup();
  renderApp(<ConfirmationHarness />);
  const trigger = screen.getByRole("button", { name: "Open confirmation" });

  trigger.focus();
  await user.click(trigger);
  const cancel = screen.getByRole("button", { name: "Cancel" });
  const destructive = screen.getByRole("button", { name: "Delete" });
  expect(cancel).toHaveFocus();

  await user.tab({ shift: true });
  expect(destructive).toHaveFocus();
  await user.tab();
  expect(cancel).toHaveFocus();

  await user.keyboard("{Escape}");
  expect(screen.queryByRole("alertdialog")).not.toBeInTheDocument();
  expect(screen.getByRole("status", { name: "Result" })).toHaveTextContent("cancelled");
  expect(trigger).toHaveFocus();
});

test("each enabled focus-trap session restores the trigger that opened that session", async () => {
  const user = userEvent.setup();
  renderApp(<TransitionHarness />);
  const first = screen.getByRole("button", { name: "Open from first" });
  const second = screen.getByRole("button", { name: "Open from second" });

  await user.click(first);
  expect(screen.getByRole("button", { name: "Close trap" })).toHaveFocus();
  await user.click(screen.getByRole("button", { name: "Close trap" }));
  expect(first).toHaveFocus();

  await user.click(second);
  expect(screen.getByRole("button", { name: "Close trap" })).toHaveFocus();
  await user.click(screen.getByRole("button", { name: "Close trap" }));
  expect(second).toHaveFocus();
});
