import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { BrowserRouter, Route, Routes } from "react-router-dom";
import { Workspace } from "./components/Workspace";
import { Settings } from "./components/Settings";
import { Duplicates } from "./components/Duplicates";

const queryClient = new QueryClient({
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
      <BrowserRouter>
        <Routes>
          {/* The admin / Settings surface (tech-spec 09 §B.4, 10). */}
          <Route path="/settings" element={<Settings />} />
          {/* Duplicate / dedupe review (tech-spec 05 §4). */}
          <Route path="/duplicates" element={<Duplicates />} />
          {/* URL owns view state via ?query params (lib/view-state.ts); one workspace route. */}
          <Route path="*" element={<Workspace />} />
        </Routes>
      </BrowserRouter>
    </QueryClientProvider>
  );
}
