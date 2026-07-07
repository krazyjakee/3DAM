import { MutationCache, QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { BrowserRouter, Route, Routes } from "react-router-dom";
import { Workspace } from "./components/Workspace";
import { Settings } from "./components/Settings";
import { Duplicates } from "./components/Duplicates";
import { Blocklist } from "./components/Blocklist";
import { Toaster } from "./components/Toaster";
import { DialogProvider } from "./lib/dialogs";
import { ThemeProvider } from "./lib/theme";
import { errorMessage, toast } from "./lib/toast";

// Every mutation reports through one place (issue #23): a failure always raises an error toast, so
// nothing fails silently, and a mutation can opt into a success toast via `meta.success`. Individual
// call sites stay free of boilerplate; they only add the message strings where it's worth announcing.
const queryClient = new QueryClient({
  mutationCache: new MutationCache({
    onError: (error, _vars, _ctx, mutation) => {
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
      <BrowserRouter>
        <Routes>
          {/* The admin / Settings surface (tech-spec 09 §B.4, 10). */}
          <Route path="/settings" element={<Settings />} />
          {/* Duplicate / dedupe review (tech-spec 05 §4). */}
          <Route path="/duplicates" element={<Duplicates />} />
          {/* Rescan blocklist management (issue #21). */}
          <Route path="/blocklist" element={<Blocklist />} />
          {/* URL owns view state via ?query params (lib/view-state.ts); one workspace route. */}
          <Route path="*" element={<Workspace />} />
        </Routes>
      </BrowserRouter>
      {/* Global toast viewport — feedback for every mutation, on top of every route. */}
      <Toaster />
      </DialogProvider>
      </ThemeProvider>
    </QueryClientProvider>
  );
}
