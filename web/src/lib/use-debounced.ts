import { useEffect, useState } from "react";

/** Returns `value` after it has been stable for `delayMs`. Used to debounce the derived search query
 *  so the input stays instant but the `/query` refetch only fires once typing settles (issue #33). */
export function useDebounced<T>(value: T, delayMs: number): T {
  const [debounced, setDebounced] = useState(value);
  useEffect(() => {
    const t = setTimeout(() => setDebounced(value), delayMs);
    return () => clearTimeout(t);
  }, [value, delayMs]);
  return debounced;
}
