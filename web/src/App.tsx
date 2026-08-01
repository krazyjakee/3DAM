import { lazy, Suspense } from "react";
import { MutationCache, QueryCache, QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { BrowserRouter, Route, Routes } from "react-router";
import { JobNotifications } from "./components/StatusBar";
import { Toaster } from "./components/Toaster";
import { AuthGate } from "./components/AuthGate";
import { ApiError, type VersionInfo } from "./api/client";
import { qk } from "./api/queries";
import { DialogProvider } from "./lib/dialogs";
import { ThemeProvider } from "./lib/theme";
import { AUTH_COPY, isUnauthorized, notifyUnauthorized } from "./lib/auth";
import { getServer } from "./lib/server";
import { errorMessage, toast } from "./lib/toast";

// Keep route implementations out of the application shell. In particular, the browse workspace
// pulls in virtualisation and every preview surface, while settings and upload bring their own API
// trees. A direct visit still loads exactly one route chunk; navigation asks Vite for it on demand.
const Workspace = lazy(() =>
  import("./components/Workspace").then(({ Workspace }) => ({ default: Workspace })),
);
const Settings = lazy(() =>
  import("./components/Settings").then(({ Settings }) => ({ default: Settings })),
);
const Duplicates = lazy(() =>
  import("./components/Duplicates").then(({ Duplicates }) => ({ default: Duplicates })),
);
const Blocklist = lazy(() =>
  import("./components/Blocklist").then(({ Blocklist }) => ({ default: Blocklist })),
);
const Upload = lazy(() =>
  import("./components/Upload").then(({ Upload }) => ({ default: Upload })),
);
const JobHistory = lazy(() =>
  import("./components/JobHistory").then(({ JobHistory }) => ({ default: JobHistory })),
);

function RouteFallback() {
  return (
    <main className="flex min-h-0 flex-1 items-center justify-center text-sm text-fg-dim">
      Loading view…
    </main>
  );
}

// A write refused with 403 while browsing an anonymous-mode server signed out isn't an error to
// shout about — it's the read-only posture. Point at the fix instead of dumping the raw message.
function isSignedOutWrite(error: unknown): boolean {
  if (!(error instanceof ApiError) || error.status !== 403 || getServer().token) return false;
  const version = queryClient.getQueryData<VersionInfo>(qk.version);
  return version?.auth === "anonymous";
}

// A scope-denied write with a token present (a signed-in read-only token, or any under-scoped
// credential) is a permissions posture, not a crash — reframe the raw "forbidden: …" into one
// clear line rather than leaking the server's error string.
function isScopeDeniedWrite(error: unknown): boolean {
  return error instanceof ApiError && error.status === 403 && !!getServer().token;
}

// Every mutation reports through one place (issue #23): a failure always raises an error toast, so
// nothing fails silently, and a mutation can opt into a success toast via `meta.success`. Individual
// call sites stay free of boilerplate; they only add the message strings where it's worth announcing.
// 401s are the exception on both caches: they flip the AuthGate (lib/auth.ts) — one login screen,
// not a toast per failed call.
const queryClient: QueryClient = new QueryClient({
  queryCache: new QueryCache({
    onError: (error) => {
      if (isUnauthorized(error)) notifyUnauthorized();
    },
  }),
  mutationCache: new MutationCache({
    onError: (error, _vars, _ctx, mutation) => {
      if (isUnauthorized(error)) {
        notifyUnauthorized();
        return;
      }
      if (isSignedOutWrite(error)) {
        toast.info("Read-only — sign in to make changes");
        return;
      }
      if (isScopeDeniedWrite(error)) {
        toast.info(AUTH_COPY.writeDenied);
        return;
      }
      const prefix = mutation.meta?.errorPrefix as string | undefined;
      toast.error(prefix ? `${prefix}: ${errorMessage(error)}` : errorMessage(error));
    },
    onSuccess: (_data, _vars, _ctx, mutation) => {
      const msg = mutation.meta?.success as string | undefined;
      if (msg) toast.success(msg);
    },
  }),
  defaultOptions: {
    queries: {
      staleTime: 10_000,
      refetchOnWindowFocus: false,
      retry: 1,
    },
  },
});

export function App() {
  return (
    <QueryClientProvider client={queryClient}>
      <ThemeProvider>
      <DialogProvider>
      {/* Front-door auth: in token mode nothing below renders until a credential validates. */}
      <AuthGate>
        <BrowserRouter>
          <JobNotifications />
          <Suspense fallback={<RouteFallback />}>
            <Routes>
              {/* The admin / Settings surface (tech-spec 09 §B.4, 10). */}
              <Route path="/settings" element={<Settings />} />
              {/* Duplicate / dedupe review (tech-spec 05 §4). */}
              <Route path="/duplicates" element={<Duplicates />} />
              {/* Rescan blocklist management (issue #21). */}
              <Route path="/blocklist" element={<Blocklist />} />
              {/* Writing files into a source — the one write-into-source path (issue #80). */}
              <Route path="/upload" element={<Upload />} />
              {/* Durable background operation outcomes and reports (issue #117). */}
              <Route path="/jobs" element={<JobHistory />} />
              {/* URL owns view state via ?query params (lib/view-state.ts); one workspace route. */}
              <Route path="*" element={<Workspace />} />
            </Routes>
          </Suspense>
        </BrowserRouter>
      </AuthGate>
      {/* Global toast viewport — feedback for every mutation, on top of every route. */}
      <Toaster />
      </DialogProvider>
      </ThemeProvider>
    </QueryClientProvider>
  );
}
