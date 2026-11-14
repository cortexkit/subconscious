export function registrationHelper(): Record<string, unknown> {
  // Literal paths let the compiler embed only the helper built for its target.
  try {
    switch (`${process.platform}-${process.arch}`) {
      case "darwin-arm64": return require("../../subc-client/native/subc-registration-darwin-arm64.node");
      case "darwin-x64": return require("../../subc-client/native/subc-registration-darwin-x64.node");
      case "linux-arm64": return require("../../subc-client/native/subc-registration-linux-arm64.node");
      case "linux-x64": return require("../../subc-client/native/subc-registration-linux-x64.node");
      case "win32-arm64": return require("../../subc-client/native/subc-registration-win32-arm64.node");
      case "win32-x64": return require("../../subc-client/native/subc-registration-win32-x64.node");
      default: throw new Error("registration native platform unsupported");
    }
  } catch (error) { throw error; }
}
