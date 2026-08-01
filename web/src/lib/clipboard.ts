// One reliable clipboard path for every copy affordance (issue #123). The Clipboard API is only
// exposed in secure contexts and may still reject when the browser or user denies permission. In
// every failure case the full value is kept in a persistent, selectable toast so copying manually
// remains possible; neither success nor failure feedback moves focus.

import { toast } from "./toast";

function manualCopy(value: string, label: string, message: string): false {
  toast.error(message, {
    manualCopy: {
      label: `${label} available for manual copying`,
      value,
    },
  });
  return false;
}

/** Copy text with accessible success feedback and a selectable fallback on every browser failure. */
export async function copyText(value: string, label: string): Promise<boolean> {
  const subject = label.toLowerCase();

  if (!window.isSecureContext) {
    return manualCopy(
      value,
      label,
      `Automatic copying is blocked on this connection. Select the ${subject} below and copy it manually.`,
    );
  }

  if (typeof navigator.clipboard?.writeText !== "function") {
    return manualCopy(
      value,
      label,
      `This browser doesn’t support automatic copying. Select the ${subject} below and copy it manually.`,
    );
  }

  try {
    await navigator.clipboard.writeText(value);
    toast.success(`${label} copied`);
    return true;
  } catch {
    return manualCopy(
      value,
      label,
      `Clipboard access failed or permission was denied. Select the ${subject} below and copy it manually.`,
    );
  }
}
