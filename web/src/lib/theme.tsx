// Light/dark theme controller (DESIGN_GUIDELINES §3.5 — honour the OS preference, ship a dark
// default). The user picks one of three preferences — `system` (follow the OS), `light`, or `dark`
// — persisted in localStorage. We resolve that to a concrete `light`/`dark` and stamp it as
// `data-theme` on <html>; `index.css` overrides the design tokens for the light case only (dark is
// the `@theme` default). A tiny inline script in `index.html` applies the same resolution *before*
// first paint so there's no dark-then-light flash. `matchMedia` is watched so a `system` user
// tracks the OS flipping live.
import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useState,
  type ReactNode,
} from "react";

export type ThemePref = "system" | "light" | "dark";
export type ResolvedTheme = "light" | "dark";

const STORAGE_KEY = "3dam-theme";

/** The stored preference, defaulting to `system` (and tolerant of a missing/garbage value). */
export function storedPref(): ThemePref {
  const v = typeof localStorage !== "undefined" ? localStorage.getItem(STORAGE_KEY) : null;
  return v === "light" || v === "dark" || v === "system" ? v : "system";
}

/** Resolve a preference to a concrete theme, consulting the OS only for `system`. */
export function resolvePref(pref: ThemePref): ResolvedTheme {
  if (pref !== "system") return pref;
  return window.matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
}

/** Stamp the resolved theme onto <html> — `data-theme` drives the CSS token override; the `dark`
 *  class and `color-scheme` keep native form controls/scrollbars in step. Mirrors the pre-paint
 *  script in index.html (keep the two in sync). */
export function applyResolved(resolved: ResolvedTheme): void {
  const root = document.documentElement;
  root.dataset.theme = resolved;
  root.classList.toggle("dark", resolved === "dark");
  root.style.colorScheme = resolved;
  document.querySelector('meta[name="color-scheme"]')?.setAttribute("content", resolved);
}

interface ThemeCtx {
  pref: ThemePref;
  resolved: ResolvedTheme;
  setPref: (pref: ThemePref) => void;
}

const Ctx = createContext<ThemeCtx | null>(null);

/** App-root provider: owns the preference, re-applies it on change, and keeps a `system` user in
 *  sync with the OS. Mounted once (in `App`) so the OS listener lives regardless of route. */
export function ThemeProvider({ children }: { children: ReactNode }) {
  const [pref, setPrefState] = useState<ThemePref>(storedPref);
  const [resolved, setResolved] = useState<ResolvedTheme>(() => resolvePref(storedPref()));

  useEffect(() => {
    const r = resolvePref(pref);
    setResolved(r);
    applyResolved(r);
  }, [pref]);

  // Follow the OS while (and only while) the preference is `system`.
  useEffect(() => {
    if (pref !== "system") return;
    const mq = window.matchMedia("(prefers-color-scheme: light)");
    const onChange = () => {
      const r = resolvePref("system");
      setResolved(r);
      applyResolved(r);
    };
    mq.addEventListener("change", onChange);
    return () => mq.removeEventListener("change", onChange);
  }, [pref]);

  const setPref = useCallback((p: ThemePref) => {
    localStorage.setItem(STORAGE_KEY, p);
    setPrefState(p);
  }, []);

  return <Ctx.Provider value={{ pref, resolved, setPref }}>{children}</Ctx.Provider>;
}

export function useTheme(): ThemeCtx {
  const ctx = useContext(Ctx);
  if (!ctx) throw new Error("useTheme must be used within <ThemeProvider>");
  return ctx;
}
