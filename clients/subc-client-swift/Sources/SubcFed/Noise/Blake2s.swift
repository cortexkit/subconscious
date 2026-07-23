import Foundation

/// Small BLAKE2s-256 implementation kept local because CryptoKit does not
/// expose BLAKE2s, while it is part of the pinned Noise protocol name.
struct FedBLAKE2s {
    static let digestLength = 32
    static let blockLength = 64

    private static let iv: [UInt32] = [
        0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A,
        0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19
    ]

    private static let sigma: [[Int]] = [
        [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
        [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
        [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
        [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
        [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
        [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
        [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
        [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
        [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0]
    ]

    static func hash(_ input: Data) -> Data {
        var h = iv
        // Digest length 32, key length 0, fanout 1, depth 1.
        h[0] ^= 0x01010020

        let inputStart = input.startIndex
        var offset = 0
        var counter: UInt64 = 0
        while input.count - offset > blockLength {
            let start = inputStart + offset
            let block = input.subdata(in: start..<(start + blockLength))
            counter += UInt64(blockLength)
            compress(&h, block: block, counter: counter, final: false)
            offset += blockLength
        }

        var finalBlock = Data(repeating: 0, count: blockLength)
        if offset < input.count {
            let start = inputStart + offset
            finalBlock.replaceSubrange(0..<(input.count - offset), with: input[start..<input.endIndex])
            counter += UInt64(input.count - offset)
        }
        compress(&h, block: finalBlock, counter: counter, final: true)

        var output = Data(capacity: digestLength)
        for word in h {
            var little = word.littleEndian
            withUnsafeBytes(of: &little) { output.append(contentsOf: $0) }
        }
        return output
    }

    static func hmac(key: Data, message: Data) -> Data {
        var normalized = key
        if normalized.count > blockLength { normalized = hash(normalized) }
        if normalized.count < blockLength {
            normalized.append(Data(repeating: 0, count: blockLength - normalized.count))
        }
        let ipad = Data(normalized.map { $0 ^ 0x36 })
        let opad = Data(normalized.map { $0 ^ 0x5c })
        return hash(opad + hash(ipad + message))
    }

    static func hkdf(chainingKey: Data, inputKeyMaterial: Data, outputCount: Int) -> [Data] {
        precondition(outputCount > 0)
        let tempKey = hmac(key: chainingKey, message: inputKeyMaterial)
        var previous = Data()
        var outputs: [Data] = []
        outputs.reserveCapacity(outputCount)
        for index in 1...outputCount {
            var message = previous
            message.append(UInt8(index))
            previous = hmac(key: tempKey, message: message)
            outputs.append(previous)
        }
        return outputs
    }

    private static func compress(_ h: inout [UInt32], block: Data, counter: UInt64, final: Bool) {
        var message = [UInt32](repeating: 0, count: 16)
        for index in 0..<16 {
            let offset = block.startIndex + index * 4
            message[index] = UInt32(block[offset])
                | (UInt32(block[offset + 1]) << 8)
                | (UInt32(block[offset + 2]) << 16)
                | (UInt32(block[offset + 3]) << 24)
        }

        var v = h + iv
        v[12] ^= UInt32(truncatingIfNeeded: counter)
        v[13] ^= UInt32(truncatingIfNeeded: counter >> 32)
        if final { v[14] ^= UInt32.max }

        @inline(__always)
        func rotateRight(_ value: UInt32, _ amount: UInt32) -> UInt32 {
            value >> amount | value << (32 - amount)
        }

        @inline(__always)
        func quarterRound(_ vector: inout [UInt32], _ ai: Int, _ bi: Int, _ ci: Int, _ di: Int, _ x: UInt32, _ y: UInt32) {
            var a = vector[ai]
            var b = vector[bi]
            var c = vector[ci]
            var d = vector[di]
            a = a &+ b &+ x
            d = rotateRight(d ^ a, 16)
            c = c &+ d
            b = rotateRight(b ^ c, 12)
            a = a &+ b &+ y
            d = rotateRight(d ^ a, 8)
            c = c &+ d
            b = rotateRight(b ^ c, 7)
            vector[ai] = a
            vector[bi] = b
            vector[ci] = c
            vector[di] = d
        }

        for round in 0..<10 {
            let s = sigma[round]
            quarterRound(&v, 0, 4, 8, 12, message[s[0]], message[s[1]])
            quarterRound(&v, 1, 5, 9, 13, message[s[2]], message[s[3]])
            quarterRound(&v, 2, 6, 10, 14, message[s[4]], message[s[5]])
            quarterRound(&v, 3, 7, 11, 15, message[s[6]], message[s[7]])
            quarterRound(&v, 0, 5, 10, 15, message[s[8]], message[s[9]])
            quarterRound(&v, 1, 6, 11, 12, message[s[10]], message[s[11]])
            quarterRound(&v, 2, 7, 8, 13, message[s[12]], message[s[13]])
            quarterRound(&v, 3, 4, 9, 14, message[s[14]], message[s[15]])
        }

        for index in 0..<8 {
            h[index] ^= v[index] ^ v[index + 8]
        }
    }
}

extension Data {
    init(hex: String) throws {
        let chars = Array(hex.utf8)
        guard chars.count.isMultiple(of: 2) else { throw FedNoiseError.invalidKeyLength }
        var result = Data(capacity: chars.count / 2)
        for index in stride(from: 0, to: chars.count, by: 2) {
            guard let high = Self.hexNibble(chars[index]), let low = Self.hexNibble(chars[index + 1]) else {
                throw FedNoiseError.invalidKeyLength
            }
            result.append((high << 4) | low)
        }
        self = result
    }

    var lowercaseHex: String {
        map { String(format: "%02x", $0) }.joined()
    }

    private static func hexNibble(_ byte: UInt8) -> UInt8? {
        switch byte {
        case 48...57: return byte - 48
        case 65...70: return byte - 55
        case 97...102: return byte - 87
        default: return nil
        }
    }
}
