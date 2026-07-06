import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { BrowserRouter, Route, Routes } from "react-router-dom";
import { Workspace } from "./components/Workspace";

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
          {/* URL owns view state via ?query params (lib/view-state.ts); one workspace route. */}
          <Route path="*" element={<Workspace />} />
        </Routes>
      </BrowserRouter>
    </QueryClientProvider>
  );
}
