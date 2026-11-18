import CryptoKit
import Foundation

public enum FedRemotePairingClass: String, Sendable {
    case phone, app

    var wireByte: UInt8 { self == .phone ? 1 : 2 }
}

enum FedRemotePairMode: UInt8, Sendable {
    case direct = 0, deviceRelay = 1, targetRelay = 2

    static func relay(device: Data, target: Data) throws -> Self {
        guard device.count == 32, target.count == 32, device != target else { throw FedRemotePairingError.invalidTarget }
        return FedDialLadder.relayInitiatesOpen(localPublicKey: device, responderPublicKey: target) ? .deviceRelay : .targetRelay
    }

    func prologue(status: Bool = false) throws -> Data {
        guard !status || self != .targetRelay else { throw FedRemotePairingError.statusUnavailable }
        return Data((status ? "ck-remote-pair-status/v1" : "ck-remote-pair/v1").utf8) + Data([rawValue])
    }
}

struct FedRemotePairWindow: Sendable {
    let id: Data
    let generation: UInt64
    let deviceClass: FedRemotePairingClass
    let confirmerKey: Data
    let confirmerName: String

    init(id: Data, generation: UInt64, deviceClass: FedRemotePairingClass, confirmerKey: Data, confirmerName: String) {
        self.id = id
        self.generation = generation
        self.deviceClass = deviceClass
        self.confirmerKey = confirmerKey
        self.confirmerName = confirmerName
    }

    init(_ frame: FedFrame) throws {
        guard frame.typeName == FedRemotePairCodec.window,
            case .string(let id) = frame.header["window_id"], let id = FedRemotePairCodec.hex(id, count: 16),
            case .string(let generation) = frame.header["generation"], let generation = UInt64(generation),
            case .string(let kind) = frame.header["class"], let kind = FedRemotePairingClass(rawValue: kind),
            case .string(let key) = frame.header["confirmer_key"], let key = FedRemotePairCodec.hex(key, count: 32),
            case .string(let name) = frame.header["confirmer_name"] else { throw FedRemotePairingError.protocolViolation }
        self.init(id: id, generation: generation, deviceClass: kind, confirmerKey: key, confirmerName: name)
    }

    func context(device: Data, target: Data, mode: FedRemotePairMode, handshakeHash: Data) -> Data {
        let name = Data(confirmerName.utf8)
        return Data("ck-remote-pair/v1".utf8) + id + Self.bigEndian(generation)
            + confirmerKey + device + target + Data([deviceClass.wireByte, mode.rawValue])
            + Self.bigEndian(UInt16(name.count)) + name + handshakeHash
    }

    private static func bigEndian<T: FixedWidthInteger>(_ value: T) -> Data {
        withUnsafeBytes(of: value.bigEndian) { Data($0) }
    }
}

enum FedRemotePairCrypto {
    static func commit(context: Data, initiator: Data) -> Data {
        FedBLAKE2s.hash(Data("ck-remote-pair-commit-v1".utf8) + context + initiator)
    }

    static func transcript(context: Data, commit: Data, responder: Data, initiator: Data) -> Data {
        FedBLAKE2s.hash(Data("ck-remote-pair-transcript-v1".utf8) + context + commit + responder + initiator)
    }

    static func codeDigest(transcript: Data) -> Data {
        FedBLAKE2s.hash(Data("ck-remote-pair-code-v1".utf8) + transcript)
    }

    static func code(digest: Data) -> String {
        let number = digest.prefix(8).reduce(UInt64(0)) { ($0 << 8) | UInt64($1) } % 1_000_000
        let digits = String(number)
        return String(repeating: "0", count: 6 - digits.count) + digits
    }

    static func printedCode(_ code: String) -> String { code.prefix(3) + " " + code.suffix(3) }

    static func attestationHash(_ handshakeHash: Data) -> Data {
        FedAppAttestCodec.clientDataHash(challenge: Data(SHA256.hash(
            data: Data("ck-remote-pair-attest-v1".utf8) + handshakeHash
        )))
    }
}
