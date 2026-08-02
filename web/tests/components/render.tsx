import type { ReactElement, ReactNode } from "react";
import { render } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router";
import { DialogProvider } from "../../src/lib/dialogs";

export function testQueryClient(): QueryClient {
  return new QueryClient({
    defaultOptions: {
      queries: { retry: false, staleTime: Infinity },
      mutations: { retry: false },
    },
  });
}

export function renderApp(
  ui: ReactElement,
  options: { route?: string; client?: QueryClient } = {},
) {
  const client = options.client ?? testQueryClient();
  const route = options.route ?? "/";
  const Wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>
      <MemoryRouter initialEntries={[route]}>
        <DialogProvider>{children}</DialogProvider>
      </MemoryRouter>
    </QueryClientProvider>
  );
  return { ...render(ui, { wrapper: Wrapper }), client };
}
