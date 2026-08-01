import { useEffect, useState } from "react";
import { authenticatedFetch, csrfHeaders, resolveUrl } from "./server";

type MediaBlob = {
  url: string | null;
  status: "idle" | "loading" | "ready" | "error";
  /** Re-mint a stream ticket after an element reports an expired request; blobs need no renewal. */
  renew: () => void;
};

const NO_RENEW = () => {};

/** Load protected media with an Authorization header, then expose only a local blob URL to DOM
 * elements and WASM islands. Neither the bearer nor the authenticated response URL enters markup,
 * history, caches, error strings, or browser media diagnostics. */
export function useMediaBlob(path: string | null, active = true): MediaBlob {
  const [state, setState] = useState<Omit<MediaBlob, "renew">>({ url: null, status: "idle" });

  useEffect(() => {
    if (!path || !active) {
      setState({ url: null, status: "idle" });
      return;
    }
    const abort = new AbortController();
    let objectUrl: string | null = null;
    setState({ url: null, status: "loading" });
    void authenticatedFetch(path, { signal: abort.signal })
      .then((response) => {
        if (!response.ok) throw new Error(`media request failed (${response.status})`);
        return response.blob();
      })
      .then((blob) => {
        if (abort.signal.aborted) return;
        objectUrl = URL.createObjectURL(blob);
        setState({ url: objectUrl, status: "ready" });
      })
      .catch(() => {
        if (!abort.signal.aborted) setState({ url: null, status: "error" });
      });
    return () => {
      abort.abort();
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [path, active]);

  return { ...state, renew: NO_RENEW };
}

/** Mint an exact-target ticket for streaming elements that must retain HTTP Range behavior. The
 * returned URL contains a five-minute derived credential, never the parent bearer. */
export function useMediaTicket(path: string | null, active = true): MediaBlob {
  const [state, setState] = useState<Omit<MediaBlob, "renew">>({ url: null, status: "idle" });
  const [generation, setGeneration] = useState(0);

  useEffect(() => {
    if (!path || !active) {
      setState({ url: null, status: "idle" });
      return;
    }
    const abort = new AbortController();
    setState({ url: null, status: "loading" });
    const resource = new URL(resolveUrl(path), location.origin);
    const targetResource = new URL(path, location.origin);
    const target = `${targetResource.pathname}${targetResource.search}`;
    void authenticatedFetch("/api/v1/media-ticket", {
      method: "POST",
      signal: abort.signal,
      headers: {
        "content-type": "application/json",
        accept: "application/json",
        ...csrfHeaders(),
      },
      body: JSON.stringify({ target }),
    })
      .then(async (response) => {
        if (!response.ok) throw new Error(`media ticket failed (${response.status})`);
        return (await response.json()) as { ticket: string };
      })
      .then(({ ticket }) => {
        if (abort.signal.aborted) return;
        // Append without reserializing the existing query: the server binds the ticket to the
        // exact encoded target, including order and escaping.
        const separator = resource.search ? "&" : "?";
        setState({
          url: `${resource.toString()}${separator}ticket=${encodeURIComponent(ticket)}`,
          status: "ready",
        });
      })
      .catch(() => {
        if (!abort.signal.aborted) setState({ url: null, status: "error" });
      });
    return () => abort.abort();
  }, [path, active, generation]);

  return { ...state, renew: () => setGeneration((value) => value + 1) };
}
