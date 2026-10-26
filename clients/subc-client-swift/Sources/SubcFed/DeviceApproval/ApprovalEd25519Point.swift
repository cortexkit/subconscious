import Foundation

// Public-input arithmetic only. CryptoKit's Ed25519 key initializer checks length,
// not point encoding. Validate canonical compressed points before displaying the target fingerprint.
enum ApprovalEd25519Point {
    private struct Field: Equatable {
        static let modulus: [UInt64] = [65_517] + Array(repeating: 65_535, count: 14) + [32_767]
        let limbs: [UInt64]

        init(_ integer: UInt64) { limbs = [integer] + Array(repeating: 0, count: 15) }
        init(limbs: [UInt64]) { self.limbs = limbs }
        init(bytes: [UInt8]) {
            limbs = stride(from: 0, to: 32, by: 2).map { UInt64(bytes[$0]) | (UInt64(bytes[$0 + 1]) << 8) }
        }

        static func less(_ a: [UInt64], _ b: [UInt64]) -> Bool {
            a.reversed().lexicographicallyPrecedes(b.reversed())
        }

        static func difference(_ a: [UInt64], _ b: [UInt64]) -> [UInt64] {
            var result = a, borrow: Int64 = 0
            for i in 0..<16 {
                let value = Int64(a[i]) - Int64(b[i]) - borrow
                result[i] = UInt64(value & 65_535)
                borrow = value < 0 ? 1 : 0
            }
            return result
        }

        static func reduced(_ input: [UInt64]) -> Field {
            var work = input + Array(repeating: 0, count: max(0, 32 - input.count))
            // 2^256 = 38 (mod 2^255 - 19). Products fit in UInt64 at radix 2^16.
            for i in (16..<32).reversed() { work[i - 16] += work[i] * 38; work[i] = 0 }
            while true {
                for i in 0..<15 { work[i + 1] += work[i] >> 16; work[i] &= 65_535 }
                let high = work[15] >> 15
                work[15] &= 32_767
                if high == 0 { break }
                work[0] += high * 19
            }
            let result = Array(work.prefix(16))
            return Field(limbs: less(result, modulus) ? result : difference(result, modulus))
        }

        static func + (a: Field, b: Field) -> Field { reduced(zip(a.limbs, b.limbs).map(+)) }
        static func - (a: Field, b: Field) -> Field { a + Field(limbs: difference(modulus, b.limbs)) }
        static func * (a: Field, b: Field) -> Field {
            var product = [UInt64](repeating: 0, count: 32)
            for i in 0..<16 { for j in 0..<16 { product[i + j] += a.limbs[i] * b.limbs[j] } }
            return reduced(product)
        }

        func power(_ exponent: [UInt8]) -> Field {
            var result = Field(1)
            for byte in exponent.reversed() {
                for bit in (0..<8).reversed() {
                    result = result * result
                    if (byte >> bit) & 1 == 1 { result = result * self }
                }
            }
            return result
        }
    }

    static func isWeak(_ encoded: Data) -> Bool {
        var yBytes = Array(encoded)
        yBytes[31] &= 127
        let torsion = [
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0100000000000000000000000000000000000000000000000000000000000000",
            "ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
            "26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc05",
            "c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a",
            "edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
            "eeffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
        ]
        return torsion.contains(approvalHex(yBytes))
    }

    static func decodes(_ encoded: Data) -> Bool {
        var yBytes = Array(encoded)
        guard yBytes.count == 32 else { return false }
        let signBit = yBytes[31] >> 7
        yBytes[31] &= 127
        let y = Field(bytes: yBytes)
        guard Field.less(y.limbs, Field.modulus) else { return false }
        let d = Field(bytes: Array(DeviceApprovalGrammar.bytes("a3785913ca4deb75abd841414d0a700098e879777940c78c73fe6f2bee6c0352")))
        let square = y * y
        let denominator = d * square + Field(1)
        guard denominator != Field(0) else { return false }
        let inverse = denominator.power([235] + Array(repeating: 255, count: 30) + [127])
        let xSquared = (square - Field(1)) * inverse
        if xSquared == Field(0) { return signBit == 0 }
        // Euler's criterion checks whether x² has a square root in the field.
        return xSquared.power([246] + Array(repeating: 255, count: 30) + [63]) == Field(1)
    }
}
