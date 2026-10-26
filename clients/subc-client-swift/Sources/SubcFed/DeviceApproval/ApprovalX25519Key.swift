import Foundation

// The exact libsodium has_small_order blocklist, not a Diffie-Hellman probe or
// a full point-order test. Bit 255 is ignored when comparing these encodings.
enum ApprovalX25519Key {
    static let blocklistHex = [
        "0000000000000000000000000000000000000000000000000000000000000000",
        "0100000000000000000000000000000000000000000000000000000000000000",
        "e0eb7a7c3b41b8ae1656e3faf19fc46ada098deb9c32b1fd866205165f49b800",
        "5f9c95bca3508c24b1d0b1559c83ef5b04445cc4581c8e86d8224eddd09f1157",
        "ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
        "edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
        "eeffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
    ]
    private static let blocklist = blocklistHex.map(DeviceApprovalGrammar.bytes)

    static func hasSmallOrder(_ bytes: Data) -> Bool {
        var masked = bytes
        masked[31] &= 0x7f
        return blocklist.contains(masked)
    }
}
