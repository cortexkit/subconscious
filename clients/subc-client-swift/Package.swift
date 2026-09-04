// swift-tools-version:5.9
import PackageDescription

// Native Swift subc client. Byte-for-byte parity with @cortexkit/subc-client (TS)
// and subc-client-rs (Rust): the handshake proof construction, the 21-byte
// envelope layout, and the frame codec must produce identical bytes. This is the
// wire layer for native CortexKit apps (macOS first), so it is held to the same
// golden-vector parity bar as the other two clients.
//
// SubcFed implements the client/origin side of the fed-wire protocol and starts
// Noise handshakes for iOS 16+ and macOS 13+. It stays free of AppKit/UIKit and
// only makes outbound connections; it never listens for incoming connections.
let package = Package(
    name: "SubcClient",
    platforms: [
        .iOS(.v16),
        .macOS(.v13),
    ],
    products: [
        .library(name: "SubcClient", targets: ["SubcClient"]),
        .library(name: "SubcFed", targets: ["SubcFed"]),
        // Shared alfonso-surface models (Ask/Board/Observe/Projects) with the
        // decode-tolerance discipline. Exported as a product so native apps
        // (iOS, GPUI) consume the models without forking them.
        .library(name: "SubcChatAskSupport", targets: ["SubcChatAskSupport"]),
        .executable(name: "subc-swift-probe", targets: ["SubcSwiftProbe"]),
    ],
    targets: [
        .target(name: "SubcClient"),
        .target(name: "SubcFed"),
        .target(name: "SubcChatAskSupport"),
        .executableTarget(name: "SubcSwiftProbe", dependencies: ["SubcClient"]),
        .testTarget(
            name: "SubcFedTests",
            dependencies: ["SubcFed"]
        ),
        .testTarget(
            name: "SubcClientTests",
            dependencies: ["SubcClient", "SubcChatAskSupport"],
            resources: [
                .copy("Fixtures/wire_vectors.json"),
                .copy("Fixtures/board-wire-fixtures-v1.json"),
                .copy("Fixtures/board-wire-v3.json"),
            ]
        ),
    ]
)
