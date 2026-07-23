// swift-tools-version:5.9
import PackageDescription

// Native Swift subc client. Byte-for-byte parity with @cortexkit/subc-client (TS)
// and subc-client-rs (Rust): the handshake proof construction, the 21-byte
// envelope layout, and the frame codec must produce identical bytes. This is the
// wire layer for native CortexKit apps (macOS first), so it is held to the same
// golden-vector parity bar as the other two clients.
let package = Package(
    name: "SubcClient",
    platforms: [.macOS(.v13)],
    products: [
        .library(name: "SubcClient", targets: ["SubcClient"]),
        .library(name: "SubcFed", targets: ["SubcFed"]),
        .executable(name: "subc-swift-probe", targets: ["SubcSwiftProbe"]),
        .executable(name: "subc-chat", targets: ["SubcChat"]),
    ],
    targets: [
        .target(name: "SubcClient"),
        .target(name: "SubcFed"),
        .target(name: "SubcChatAskSupport"),
        .executableTarget(name: "SubcSwiftProbe", dependencies: ["SubcClient"]),
        .executableTarget(name: "SubcChat", dependencies: ["SubcClient", "SubcChatAskSupport"]),
        .testTarget(name: "SubcFedTests", dependencies: ["SubcFed"]),
        .testTarget(
            name: "SubcClientTests",
            dependencies: ["SubcClient", "SubcChatAskSupport"],
            resources: [
                .copy("Fixtures/wire_vectors.json"),
                .copy("Fixtures/board-wire-fixtures-v1.json"),
            ]
        ),
    ]
)
