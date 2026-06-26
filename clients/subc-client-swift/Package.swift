// swift-tools-version:5.9
import PackageDescription

// Native Swift subc client. Byte-for-byte parity with @cortexkit/subc-client (TS)
// and subc-client-rs (Rust): the handshake proof construction, the 17-byte
// envelope layout, and the frame codec must produce identical bytes. This is the
// wire layer for native CortexKit apps (macOS first), so it is held to the same
// golden-vector parity bar as the other two clients.
let package = Package(
    name: "SubcClient",
    platforms: [.macOS(.v13)],
    products: [
        .library(name: "SubcClient", targets: ["SubcClient"]),
        .executable(name: "subc-swift-probe", targets: ["SubcSwiftProbe"]),
    ],
    targets: [
        .target(name: "SubcClient"),
        .executableTarget(name: "SubcSwiftProbe", dependencies: ["SubcClient"]),
        .testTarget(
            name: "SubcClientTests",
            dependencies: ["SubcClient"],
            resources: [.copy("Fixtures/wire_vectors.json")]
        ),
    ]
)
