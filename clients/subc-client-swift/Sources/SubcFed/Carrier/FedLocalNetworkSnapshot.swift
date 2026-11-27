import Darwin
import Foundation
import Network

extension FedObservedNetworkSnapshot {
    /// Observe local private subnets now, rather than trusting a snapshot saved
    /// by another process before the device moved between networks.
    static func localInterfaces() -> Self {
        var interfaces: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&interfaces) == 0, let interfaces else { return Self() }
        defer { freeifaddrs(interfaces) }

        var subnets: [FedObservedPrivateSubnet] = []
        var current: UnsafeMutablePointer<ifaddrs>? = interfaces
        while let entry = current?.pointee {
            current = entry.ifa_next
            guard entry.ifa_flags & UInt32(IFF_UP) != 0,
                  let address = entry.ifa_addr, let mask = entry.ifa_netmask else { continue }
            let subnet: FedObservedPrivateSubnet?
            switch Int32(address.pointee.sa_family) {
            case AF_INET:
                let bytes = address.withMemoryRebound(to: sockaddr_in.self, capacity: 1) {
                    withUnsafeBytes(of: $0.pointee.sin_addr, Array.init)
                }
                guard bytes[0] == 10 || (bytes[0] == 172 && (16...31).contains(bytes[1]))
                    || (bytes[0] == 192 && bytes[1] == 168) else { continue }
                let bits = netmaskBytes(mask, offset: MemoryLayout<sockaddr_in>.offset(of: \.sin_addr)!, count: bytes.count)
                guard let prefix = prefixLength(bits),
                      let network = IPv4Address(Data(zip(bytes, bits).map(&))) else { continue }
                subnet = try? FedObservedPrivateSubnet(ipv4: network, prefixLength: prefix)
            case AF_INET6:
                let bytes = address.withMemoryRebound(to: sockaddr_in6.self, capacity: 1) {
                    withUnsafeBytes(of: $0.pointee.sin6_addr, Array.init)
                }
                guard bytes[0] & 0xfe == 0xfc else { continue }
                let bits = netmaskBytes(mask, offset: MemoryLayout<sockaddr_in6>.offset(of: \.sin6_addr)!, count: bytes.count)
                guard let prefix = prefixLength(bits),
                      let network = IPv6Address(Data(zip(bytes, bits).map(&))) else { continue }
                subnet = try? FedObservedPrivateSubnet(ipv6: network, prefixLength: prefix)
            default:
                continue
            }
            if let subnet { subnets.append(subnet) }
        }
        return Self(subnets: subnets)
    }

    private static func netmaskBytes(_ mask: UnsafePointer<sockaddr>, offset: Int, count: Int) -> [UInt8] {
        // Darwin can omit trailing zero bytes from a netmask sockaddr.
        let length = min(count, Int(mask.pointee.sa_len) - offset)
        guard length > 0 else { return Array(repeating: 0, count: count) }
        let bytes = UnsafeRawBufferPointer(start: UnsafeRawPointer(mask).advanced(by: offset), count: length)
        return Array(bytes) + Array(repeating: 0, count: count - length)
    }

    private static func prefixLength(_ mask: [UInt8]) -> Int? {
        let prefix = mask.reduce(0) { $0 + $1.nonzeroBitCount }
        // A zero or non-contiguous netmask is not useful evidence of a subnet.
        guard prefix > 0, mask.enumerated().allSatisfy({ offset, byte in
            let bits = min(8, max(0, prefix - offset * 8))
            return byte == UInt8.max << (8 - bits)
        }) else { return nil }
        return prefix
    }
}
