export {
  SubcClient,
  SubcError,
  connectionFileExists,
  type BindIdentity,
  type RouteTarget,
  type CatalogEntry,
  type RequestOptions,
  type ConnectOptions,
} from "./client.js";
export {
  readConnectionFile,
  ConnectionFileError,
  type ConnectionInfo,
  type Endpoint,
} from "./connection-file.js";
export {
  FrameType,
  Priority,
  PROTOCOL_VERSION,
  HEADER_LEN,
  buildFrame,
  buildFlags,
  encodeFrame,
  decodeHeader,
  encodeHeader,
  DecodeError,
  type Frame,
  type EnvelopeHeader,
} from "./envelope.js";
export {
  authenticateClient,
  computeProof,
  AuthError,
  NONCE_LEN,
  PROOF_LEN,
  SERVER_PROOF_DOMAIN,
  CLIENT_AUTH_DOMAIN,
} from "./auth.js";
export { SubcSocket, SocketClosedError, SocketTimeoutError } from "./socket.js";
