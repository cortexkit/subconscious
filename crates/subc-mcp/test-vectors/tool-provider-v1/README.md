`model-visible-size.json` is copied byte for byte from `cortexkit-role-tool-provider 0.15.0`, `test-vectors/tool-provider-v1/model-visible-size.json`.

The gateway's routes carry native MCP `CallToolResult`. They reuse these samples' content rule (text by UTF-8 bytes, images as zero, other parts by RFC 8785 length, `structuredContent` excluded, exactly 65,536 bytes allowed), but the gateway has no tool-result/v1 or legacy-body decoding. The legacy samples exercise a test-only helper.
