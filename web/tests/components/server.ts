import { setupServer } from "msw/node";

/** Tests install only the request handlers their user journey needs. */
export const server = setupServer();
