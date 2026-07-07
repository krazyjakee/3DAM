import type React from "react";
import { useEffect } from "react";
import { useFocusTrap } from "@/lib/use-focus-trap";

/** A narrow-screen overlay drawer (responsive + touch pass).
 *
 *  On `lg` and up it is inert (`lg:hidden`): the workspace shows Navigation / Inspector as
 *  persistent rails there, so the drawer only exists to host the same content once the three-region
 *  layout collapses to a single scrollable column. It stays mounted (translated off-screen when
 *  closed) so the open/close slide animates, and closes on backdrop tap or Escape. */
export function Drawer({
  open,
  onClose,
  side,
  label,
  children,
}: {
  open: boolean;
  onClose: () => void;
  side: "left" | "right";
  label: string;
  children: React.ReactNode;
}) {
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, onClose]);

  // Trap focus inside the panel while open; restore it to the trigger on close (issue #26).
  const panelRef = useFocusTrap<HTMLDivElement>(open);

  const closedShift = side === "left" ? "-translate-x-full" : "translate-x-full";

  return (
    <div
      className={`fixed inset-0 z-40 lg:hidden ${open ? "" : "pointer-events-none"}`}
      aria-hidden={!open}
    >
      <div
        className={`absolute inset-0 bg-black/60 transition-opacity duration-200 ${
          open ? "opacity-100" : "opacity-0"
        }`}
        onClick={onClose}
      />
      <div
        ref={panelRef}
        role="dialog"
        aria-modal="true"
        aria-label={label}
        className={`absolute inset-y-0 flex w-[86%] max-w-[320px] flex-col bg-surface shadow-2xl transition-transform duration-200 ${
          side === "left" ? "left-0" : "right-0"
        } ${open ? "translate-x-0" : closedShift}`}
      >
        {children}
      </div>
    </div>
  );
}
