import Foundation
import Network
import XCTest
@testable import SubcFed

/// Public-direct candidates: the hygiene table that decides which addresses may
/// be dialed across the internet, and the client wiring that must refuse a bad
/// candidate before any carrier (socket) is opened.
final class FedPublicDirectCandidateTests: XCTestCase {
    // MARK: - Hygiene: refused IPv4 ranges

    /// Every non-global IPv4 range is refused as `addressClassNotAllowed`. Each
    /// row is an address inside the named range.
    func testPublicHygieneRefusesNonGlobalIPv4Ranges() {
        let refused: [(String, String)] = [
            ("0.0.0.0", "0.0.0.0/8"),
            ("0.255.255.255", "0.0.0.0/8"),
            ("10.0.0.1", "10.0.0.0/8"),
            ("10.255.255.254", "10.0.0.0/8"),
            ("100.64.0.1", "100.64.0.0/10"),
            ("100.127.255.254", "100.64.0.0/10"),
            ("127.0.0.1", "127.0.0.0/8"),
            ("169.254.1.1", "169.254.0.0/16"),
            ("172.16.0.1", "172.16.0.0/12"),
            ("172.31.255.254", "172.16.0.0/12"),
            ("192.0.0.8", "192.0.0.0/24"),
            ("192.0.2.1", "192.0.2.0/24"),
            ("192.168.1.10", "192.168.0.0/16"),
            ("198.18.0.1", "198.18.0.0/15"),
            ("198.19.255.254", "198.18.0.0/15"),
            ("198.51.100.7", "198.51.100.0/24"),
            ("203.0.113.9", "203.0.113.0/24"),
            ("224.0.0.1", "224.0.0.0/4 multicast"),
            ("239.255.255.250", "224.0.0.0/4 multicast"),
            ("240.0.0.1", "240.0.0.0/4 reserved"),
            ("255.255.255.255", "broadcast"),
        ]
        for (host, range) in refused {
            XCTAssertEqual(
                FedPublicCandidateHygiene.classify(host: host, peerVerified: true),
                .addressClassNotAllowed,
                "\(host) is in \(range) and must not be dialed as public-direct"
            )
            XCTAssertEqual(
                FedPublicCandidateHygiene.classify(address: IPv4Address(host)!, peerVerified: true),
                .addressClassNotAllowed,
                "\(host) (\(range)) refused through the IPAddress entry point too"
            )
        }
    }

    // MARK: - Hygiene: refused IPv6 ranges

    func testPublicHygieneRefusesNonGlobalIPv6Ranges() {
        let refused: [(String, String)] = [
            ("::", "unspecified"),
            ("::1", "loopback"),
            ("fe80::1", "fe80::/10 link-local"),
            ("febf:ffff::1", "fe80::/10 link-local"),
            ("fc00::1", "fc00::/7 unique local"),
            ("fd12:3456:789a::1", "fc00::/7 unique local"),
            ("ff02::1", "ff00::/8 multicast"),
            ("ff0e::1", "ff00::/8 multicast"),
            ("2001:db8::1", "2001:db8::/32 documentation"),
            ("2001:db8:ffff::1", "2001:db8::/32 documentation"),
            ("fec0::1", "fec0::/10 site-local"),
            ("feff:ffff::1", "fec0::/10 site-local"),
        ]
        for (host, range) in refused {
            XCTAssertEqual(
                FedPublicCandidateHygiene.classify(host: host, peerVerified: true),
                .addressClassNotAllowed,
                "\(host) is in \(range) and must not be dialed as public-direct"
            )
            XCTAssertEqual(
                FedPublicCandidateHygiene.classify(address: IPv6Address(host)!, peerVerified: true),
                .addressClassNotAllowed,
                "\(host) (\(range)) refused through the IPAddress entry point too"
            )
        }
    }

    /// `::ffff:a.b.c.d` reaches the IPv4 host a.b.c.d, so a mapped form of a
    /// refused IPv4 address must be refused as well; otherwise the IPv6 spelling
    /// would be a way around the IPv4 table.
    func testPublicHygieneRefusesIPv4MappedFormsOfRefusedIPv4() {
        for host in ["::ffff:10.0.0.1", "::ffff:127.0.0.1", "::ffff:192.168.1.10",
                     "::ffff:100.64.0.1", "::ffff:169.254.1.1", "::ffff:0.0.0.0"] {
            XCTAssertEqual(
                FedPublicCandidateHygiene.classify(host: host, peerVerified: true),
                .addressClassNotAllowed,
                "\(host) maps to a refused IPv4 address"
            )
        }
        // A mapped GLOBAL IPv4 address is held to the same rules and so accepted.
        XCTAssertNil(FedPublicCandidateHygiene.classify(host: "::ffff:79.152.99.84", peerVerified: true))
    }

    /// 64:ff9b::a.b.c.d (the NAT64 well-known prefix) reaches a.b.c.d through
    /// the network's NAT64 gateway, so it is judged by the IPv4 table.
    func testPublicHygieneJudgesNAT64WellKnownPrefixByIPv4Table() {
        for host in ["64:ff9b::a00:1", "64:ff9b::10.0.0.1", "64:ff9b::7f00:1",
                     "64:ff9b::c0a8:10a", "64:ff9b::a9fe:101", "64:ff9b::"] {
            XCTAssertEqual(
                FedPublicCandidateHygiene.classify(host: host, peerVerified: true),
                .addressClassNotAllowed,
                "\(host) reaches a refused IPv4 address through NAT64"
            )
        }
        // 64:ff9b::4f98:6354 is 79.152.99.84, a global address.
        XCTAssertNil(FedPublicCandidateHygiene.classify(host: "64:ff9b::4f98:6354", peerVerified: true))
        XCTAssertNil(FedPublicCandidateHygiene.classify(host: "64:ff9b::79.152.99.84", peerVerified: true))
    }

    /// The NAT64 local-use prefix's embedding layout is chosen per network, so
    /// it is refused outright, even when it seems to carry a global address.
    func testPublicHygieneRefusesNAT64LocalUsePrefix() {
        for host in ["64:ff9b:1::4f98:6354", "64:ff9b:1:ffff::1", "64:ff9b:1::a00:1"] {
            XCTAssertEqual(
                FedPublicCandidateHygiene.classify(host: host, peerVerified: true),
                .addressClassNotAllowed,
                "\(host) is in 64:ff9b:1::/48"
            )
        }
    }

    /// 6to4 (2002::/16) is deprecated and can embed any IPv4 address, so it is
    /// refused outright, whatever it embeds.
    func testPublicHygieneRefuses6to4() {
        for host in ["2002:a00:1::1", "2002:7f00:1::1", "2002:4f98:6354::1", "2002::1"] {
            XCTAssertEqual(
                FedPublicCandidateHygiene.classify(host: host, peerVerified: true),
                .addressClassNotAllowed,
                "\(host) is in 2002::/16"
            )
        }
    }

    /// The deprecated IPv4-compatible form ::a.b.c.d is refused outright.
    func testPublicHygieneRefusesIPv4CompatibleForm() {
        for host in ["::8.8.8.8", "::4f98:6354", "::10.0.0.1", "::2"] {
            XCTAssertEqual(
                FedPublicCandidateHygiene.classify(host: host, peerVerified: true),
                .addressClassNotAllowed,
                "\(host) is an IPv4-compatible address"
            )
        }
    }

    /// Network's IPv4 parser accepts legacy shorthand ("1.2.3" parses as
    /// 1.2.0.3, "0x7f.1" as 127.0.0.1, "01.2.3.4" as 1.2.3.4). None of it is a
    /// public-direct host: only plain four-part decimal is accepted, so the
    /// string dialed can only mean the address that was classified.
    func testPublicHygieneRefusesNonCanonicalIPv4Literals() {
        for host in ["1.2.3", "01.2.3.4", "1.2.3.4.5", "0x7f.1", "127.1", "010.0.0.1",
                     "79.152.25428", "079.152.99.84", "256.1.1.1", "79.152.99.84.", ".79.152.99.84",
                     "79..99.84", "7 9.152.99.84"] {
            XCTAssertEqual(
                FedPublicCandidateHygiene.classify(host: host, peerVerified: true),
                .invalidAddress,
                "\(host) is not a plain dotted-quad literal"
            )
        }
        // The plain form of the same addresses is still judged normally.
        XCTAssertNil(FedPublicCandidateHygiene.classify(host: "1.2.0.3", peerVerified: true))
        XCTAssertEqual(
            FedPublicCandidateHygiene.classify(host: "127.0.0.1", peerVerified: true),
            .addressClassNotAllowed
        )
    }

    // MARK: - Hygiene: accepted addresses

    func testPublicHygieneAcceptsGlobalAddresses() {
        let globalV4 = ["79.152.99.84", "8.8.8.8"]
        let globalV6 = ["2606:4700:4700::1111", "2001:4860:4860::8888"]
        for host in globalV4 {
            XCTAssertNil(FedPublicCandidateHygiene.classify(host: host, peerVerified: true), host)
            XCTAssertNil(FedPublicCandidateHygiene.classify(address: IPv4Address(host)!, peerVerified: true), host)
        }
        for host in globalV6 {
            XCTAssertNil(FedPublicCandidateHygiene.classify(host: host, peerVerified: true), host)
            XCTAssertNil(FedPublicCandidateHygiene.classify(address: IPv6Address(host)!, peerVerified: true), host)
        }
        // Neighbours just outside the refused ranges are global and accepted, so
        // the range edges are not drawn too wide.
        for host in ["1.0.0.1", "11.0.0.1", "100.63.255.255", "100.128.0.1", "126.255.255.255",
                     "128.0.0.1", "169.253.255.255", "172.15.255.255", "172.32.0.1",
                     "192.0.1.1", "192.0.3.1", "192.167.255.255", "192.169.0.1",
                     "198.17.255.255", "198.20.0.1", "198.51.99.1", "203.0.112.1",
                     "223.255.255.254", "2001:db9::1", "2001:dc8::1"] {
            XCTAssertNil(FedPublicCandidateHygiene.classify(host: host, peerVerified: true),
                         "\(host) is globally routable")
        }
    }

    // MARK: - Hygiene: hostnames and unverified peers

    /// The public-direct path never resolves DNS, so anything that is not an IP
    /// literal is refused as `invalidAddress`.
    func testPublicHygieneRefusesHostnamesWithoutDNS() {
        for host in ["example.com", "localhost", "callosum.local", "[2606:4700:4700::1111]",
                     "79.152.99.84:7841", "fe80::1%en0", "2606:4700:4700::1111%en0"] {
            XCTAssertEqual(
                FedPublicCandidateHygiene.classify(host: host, peerVerified: true),
                .invalidAddress,
                "\(host) is not an IP literal"
            )
        }
    }

    func testPublicHygieneRefusesUnverifiedPeer() {
        XCTAssertEqual(
            FedPublicCandidateHygiene.classify(host: "79.152.99.84", peerVerified: false),
            .unverifiedPeerLAN
        )
        XCTAssertEqual(
            FedPublicCandidateHygiene.classify(address: IPv6Address("2606:4700:4700::1111")!, peerVerified: false),
            .unverifiedPeerLAN
        )
    }

    // MARK: - Candidate model

    func testPublicDirectCandidateValidationAndClassMapping() throws {
        let candidate = try FedPublicDirectCandidate(candidateID: " pub-1 ", host: " 79.152.99.84 ", port: 7841)
        XCTAssertEqual(candidate.candidateID, "pub-1")
        XCTAssertEqual(candidate.host, "79.152.99.84")
        XCTAssertEqual(candidate.port, 7841)
        XCTAssertThrowsError(try FedPublicDirectCandidate(candidateID: " ", host: "79.152.99.84", port: 7841))
        XCTAssertThrowsError(try FedPublicDirectCandidate(candidateID: "pub", host: "  ", port: 7841))
        XCTAssertThrowsError(try FedPublicDirectCandidate(candidateID: "pub", host: "79.152.99.84", port: 0))

        let wrapped = FedPeerCandidate.publicDirect(candidate)
        XCTAssertEqual(wrapped.candidateID, "pub-1")
        XCTAssertEqual(wrapped.candidateClass, .publicDirect)
        XCTAssertEqual(wrapped.candidateClass.connectedRung, .publicDirect)
        XCTAssertEqual(FedCandidateClass.lanDirect.connectedRung, .lanDirect)
        XCTAssertEqual(FedCandidateClass.relay.connectedRung, .relay)
    }

    /// Public-direct follows the same single-dialer rule as LAN-direct: with
    /// both sides reachable only the lower key dials, and in the double-NAT case
    /// neither side dials directly.
    func testPublicDirectDialOwnershipMatchesLANDirect() throws {
        let a = try FedPublicTestSupport.publicKey(fromPrivateKey: Data(repeating: 0x01, count: 32))
        let b = try FedPublicTestSupport.publicKey(fromPrivateKey: Data(repeating: 0xFE, count: 32))
        let lower = a.fedLexicographicallyPrecedes(b) ? a : b
        let higher = lower == a ? b : a
        let facts: [FedDialOwnershipFacts] = [
            .localOriginOnly,
            FedDialOwnershipFacts(localPublishesAddress: true, remotePublishesAddress: false),
            FedDialOwnershipFacts(localPublishesAddress: true, remotePublishesAddress: true),
            FedDialOwnershipFacts(localPublishesAddress: false, remotePublishesAddress: false),
        ]
        for fact in facts {
            for (local, remote) in [(lower, higher), (higher, lower)] {
                XCTAssertEqual(
                    FedDialOwnership.initiationRole(
                        for: .publicDirect, localPublicKey: local, responderPublicKey: remote, facts: fact),
                    FedDialOwnership.initiationRole(
                        for: .lanDirect, localPublicKey: local, responderPublicKey: remote, facts: fact),
                    "public-direct ownership must equal LAN-direct for \(fact)"
                )
            }
        }
    }

    // MARK: - Client wiring

    func testPublicDirectWithRefusedAddressNeverOpensCarrier() async throws {
        let dials = PublicDirectDialLog()
        let factory = RecordingDialFactory { candidate, _ in
            await dials.record(candidate.candidateID)
            throw FedFailure.disconnected
        }
        let profile = try FedPublicTestSupport.humanProfile(candidates: [
            .publicDirect(try FedPublicDirectCandidate(candidateID: "pub-1", host: "10.0.0.1", port: 7841)),
        ])
        let client = SubcFedClient(
            profile: profile,
            keyStore: try FedPublicTestSupport.keyStore(),
            stateStore: FedMemoryStateStore(),
            observedNetwork: { try! FedPublicTestSupport.observedHomeLAN() },
            dialFactory: factory
        )
        do {
            try await client.connect()
            XCTFail("expected noEligibleCandidates")
        } catch let failure as FedFailure {
            guard case .noEligibleCandidates(let retained) = failure else {
                return XCTFail("unexpected \(failure)")
            }
            XCTAssertEqual(retained.first?.candidateID, "pub-1")
            XCTAssertEqual(retained.first?.stage, .carrierConnect)
            XCTAssertEqual(retained.first?.reason, .rejected(.addressClassNotAllowed))
        }
        let attempt = await client.lastAttemptID
        let carriers = await client.carrierOperationsStarted
        let dialed = await dials.ids
        XCTAssertNil(attempt)
        XCTAssertEqual(carriers, 0)
        XCTAssertEqual(dialed, [], "a refused public-direct candidate must never reach the dial factory")
    }

    func testPublicDirectForUnverifiedPeerNeverOpensCarrier() async throws {
        let dials = PublicDirectDialLog()
        let factory = RecordingDialFactory { candidate, _ in
            await dials.record(candidate.candidateID)
            throw FedFailure.disconnected
        }
        let profile = try FedPublicTestSupport.humanProfile(
            candidates: [
                .publicDirect(try FedPublicDirectCandidate(candidateID: "pub-1", host: "79.152.99.84", port: 7841)),
            ],
            isVerified: false
        )
        let client = SubcFedClient(
            profile: profile,
            keyStore: try FedPublicTestSupport.keyStore(),
            stateStore: FedMemoryStateStore(),
            observedNetwork: { try! FedPublicTestSupport.observedHomeLAN() },
            dialFactory: factory
        )
        do {
            try await client.connect()
            XCTFail("expected noEligibleCandidates")
        } catch let failure as FedFailure {
            guard case .noEligibleCandidates(let retained) = failure else {
                return XCTFail("unexpected \(failure)")
            }
            XCTAssertEqual(retained.first?.reason, .rejected(.unverifiedPeerLAN))
        }
        let carriers = await client.carrierOperationsStarted
        let dialed = await dials.ids
        XCTAssertEqual(carriers, 0)
        XCTAssertEqual(dialed, [])
    }

    /// The embedding's candidate order is the dial order: the SDK neither
    /// promotes nor demotes public-direct relative to other candidates.
    func testPublicDirectIsDialedInEmbeddingOrder() async throws {
        let publicCandidate = FedPeerCandidate.publicDirect(
            try FedPublicDirectCandidate(candidateID: "pub-1", host: "79.152.99.84", port: 7841))
        let lanCandidate = FedPeerCandidate.lanDirect(
            try FedLANDirectCandidate(candidateID: "lan-1", host: "192.168.1.10", port: 7700))

        for order in [[publicCandidate, lanCandidate], [lanCandidate, publicCandidate]] {
            let dials = PublicDirectDialLog()
            let factory = RecordingDialFactory { candidate, _ in
                await dials.record(candidate.candidateID)
                throw FedFailure.disconnected
            }
            let client = SubcFedClient(
                profile: try FedPublicTestSupport.humanProfile(candidates: order),
                keyStore: try FedPublicTestSupport.keyStore(),
                stateStore: FedMemoryStateStore(),
                observedNetwork: { try! FedPublicTestSupport.observedHomeLAN() },
                dialFactory: factory
            )
            _ = try? await client.connect()
            await client.disconnect()
            let dialed = await dials.ids
            XCTAssertEqual(dialed, order.map(\.candidateID))
        }
    }

    /// Public-direct reachability depends on the network the device is on, so a
    /// suppressed failure holds while the network is unchanged and is lifted as
    /// soon as the observed network changes.
    func testPublicDirectSuppressionIsLiftedByNetworkChange() async throws {
        let dials = PublicDirectDialLog()
        let factory = RecordingDialFactory { candidate, _ in
            await dials.record(candidate.candidateID)
            // A timeout is a suppressing failure (only transport failures are not).
            throw FedFailure.candidateTimedOut(stage: .carrierConnect)
        }
        let network = ObservedNetworkBox(try FedPublicTestSupport.observedHomeLAN())
        let client = SubcFedClient(
            profile: try FedPublicTestSupport.humanProfile(candidates: [
                .publicDirect(try FedPublicDirectCandidate(candidateID: "pub-1", host: "79.152.99.84", port: 7841)),
            ]),
            keyStore: try FedPublicTestSupport.keyStore(),
            stateStore: FedMemoryStateStore(),
            observedNetwork: { network.current },
            dialFactory: factory
        )

        _ = try? await client.connect()
        var dialed = await dials.ids
        XCTAssertEqual(dialed, ["pub-1"], "first connect dials the candidate")

        // Same network: the timed-out candidate stays suppressed.
        do {
            try await client.connect()
            XCTFail("expected noEligibleCandidates while suppressed")
        } catch let failure as FedFailure {
            guard case .noEligibleCandidates(let retained) = failure else {
                return XCTFail("unexpected \(failure)")
            }
            XCTAssertEqual(retained.first?.reason, .timedOut(.carrierConnect))
        }
        dialed = await dials.ids
        XCTAssertEqual(dialed, ["pub-1"], "no redial on the same network")

        // New network: suppression is lifted and the candidate is dialed again.
        network.current = FedObservedNetworkSnapshot(subnets: [
            try FedObservedPrivateSubnet(ipv4: IPv4Address("10.20.0.0")!, prefixLength: 16),
        ])
        _ = try? await client.connect()
        dialed = await dials.ids
        XCTAssertEqual(dialed, ["pub-1", "pub-1"], "a network change lifts the suppression")
    }

    /// When this side does not own direct dialing, a public-direct candidate is
    /// withheld before any attempt or carrier, exactly like LAN-direct.
    func testPublicDirectRespectsSingleDialerRule() async throws {
        let dials = PublicDirectDialLog()
        let factory = RecordingDialFactory { candidate, _ in
            await dials.record(candidate.candidateID)
            throw FedFailure.disconnected
        }
        // Same key pair as the LAN-direct ownership test: the local key sorts
        // above the responder's, so with both sides reachable it does not dial.
        let keyStore = try FedMemoryPrivateKeyStore(noisePrivateKey: Data(repeating: 0xF0, count: 32))
        let responderPublic = try FedPublicTestSupport.publicKey(fromPrivateKey: Data(repeating: 0x10, count: 32))
        let profile = try FedPublicTestSupport.humanProfile(
            candidates: [
                .publicDirect(try FedPublicDirectCandidate(candidateID: "pub-1", host: "79.152.99.84", port: 7841)),
            ],
            dialOwnership: FedDialOwnershipFacts(localPublishesAddress: true, remotePublishesAddress: true),
            responderPublicKey: responderPublic
        )
        let client = SubcFedClient(
            profile: profile,
            keyStore: keyStore,
            stateStore: FedMemoryStateStore(),
            observedNetwork: { try! FedPublicTestSupport.observedHomeLAN() },
            dialFactory: factory
        )
        do {
            try await client.connect()
            XCTFail("expected notDialOwner")
        } catch let failure as FedFailure {
            XCTAssertEqual(failure, .notDialOwner)
        }
        let carriers = await client.carrierOperationsStarted
        let dialed = await dials.ids
        XCTAssertEqual(carriers, 0)
        XCTAssertEqual(dialed, [])
    }
}

private final class ObservedNetworkBox: @unchecked Sendable {
    private let lock = NSLock()
    private var snapshot: FedObservedNetworkSnapshot

    init(_ snapshot: FedObservedNetworkSnapshot) { self.snapshot = snapshot }

    var current: FedObservedNetworkSnapshot {
        get { lock.lock(); defer { lock.unlock() }; return snapshot }
        set { lock.lock(); defer { lock.unlock() }; snapshot = newValue }
    }
}

private actor PublicDirectDialLog {
    private(set) var ids: [String] = []
    func record(_ id: String) { ids.append(id) }
}
