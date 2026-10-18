import Foundation

public enum FedPairingCrypto {
    public static func derivePSK(secret: Data, computerStaticPublicKey: Data) throws -> Data {
        guard secret.count == 32, computerStaticPublicKey.count == 32 else { throw FedNoiseError.invalidKeyLength }
        let extract = FedBLAKE2s.hmac(key: computerStaticPublicKey, message: secret)
        return FedBLAKE2s.hmac(key: extract, message: Data("ck-fed-pair-v1".utf8) + Data([1]))
    }

    public static func confirmationWords(handshakeHash: Data) throws -> [String] {
        guard handshakeHash.count == 32 else { throw FedNoiseError.invalidMessage }
        return words(digest: FedBLAKE2s.hash(Data("ck-fed-pair-words-v1".utf8) + handshakeHash))
    }

    static let wordlistURL = Bundle.module.url(forResource: "bip39-english", withExtension: "txt")!

    static let wordlist: [String] = {
        // The unmodified BIP-39 English index table is pinned by SHA-256 in tests.
        let text = try! String(contentsOf: wordlistURL, encoding: .utf8)
        let words = text.split(separator: "\n").map(String.init)
        precondition(words.count == 2048)
        return words
    }()

    static func indices(digest: Data) -> [Int] {
        precondition(digest.count == 32)
        let bytes = Array(digest)
        return (0..<4).map { word in
            (0..<11).reduce(0) { value, bit in
                let offset = word * 11 + bit
                return (value << 1) | Int((bytes[offset / 8] >> (7 - offset % 8)) & 1)
            }
        }
    }

    static func words(digest: Data) -> [String] { indices(digest: digest).map { wordlist[$0] } }
}
