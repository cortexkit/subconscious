export interface Env {
  RELEASE_INDEX: KVNamespace;
  GITHUB_WEBHOOK_SECRET: string;
  ADMIN_TOKEN: string;
  RELEASE_INDEX_SIGNING_KEY: string;
  GITHUB_APP_ID: string;
  GITHUB_APP_INSTALLATION_ID: string;
  GITHUB_APP_PRIVATE_KEY: string;
}

export type FetchFn = (input: RequestInfo | URL, init?: RequestInit) => Promise<Response>;
