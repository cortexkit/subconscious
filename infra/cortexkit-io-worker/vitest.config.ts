import { cloudflareTest } from "@cloudflare/vitest-pool-workers";
import { defineConfig } from "vitest/config";
import {
  TEST_ADMIN_TOKEN,
  TEST_APP_ID,
  TEST_ED25519_PKCS8_PEM,
  TEST_INSTALLATION_ID,
  TEST_RSA_PKCS8_PEM,
  TEST_WEBHOOK_SECRET,
} from "./test/keys";

export default defineConfig({
  plugins: [
    cloudflareTest({
      wrangler: { configPath: "./wrangler.toml" },
      miniflare: {
        bindings: {
          GITHUB_WEBHOOK_SECRET: TEST_WEBHOOK_SECRET,
          ADMIN_TOKEN: TEST_ADMIN_TOKEN,
          RELEASE_INDEX_SIGNING_KEY: TEST_ED25519_PKCS8_PEM,
          GITHUB_APP_ID: TEST_APP_ID,
          GITHUB_APP_INSTALLATION_ID: TEST_INSTALLATION_ID,
          GITHUB_APP_PRIVATE_KEY: TEST_RSA_PKCS8_PEM,
        },
      },
    }),
  ],
  test: {
    include: ["test/**/*.test.ts"],
  },
});
