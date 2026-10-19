import Foundation

/// Open strings allow a newer peer to explain an outcome without breaking older apps.
public struct FedSubscriptionReason: RawRepresentable, Sendable, Hashable, Codable {
    public let rawValue: String
    public init(rawValue: String) { self.rawValue = rawValue }
    public init(from decoder: Decoder) throws { self.init(rawValue: try decoder.singleValueContainer().decode(String.self)) }
    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(rawValue)
    }
    public static let unsupported = Self(rawValue: "unsupported")
    public static let catalogUnavailable = Self(rawValue: "catalog_unavailable")
    public static let notWatch = Self(rawValue: "not_watch")
    public static let subscriptionLimit = Self(rawValue: "subscription_limit")
    public static let sessionLost = Self(rawValue: "session_lost")
    public static let multipleConsumers = Self(rawValue: "multiple_consumers")
    public static let completed = Self(rawValue: "completed")
    public static let moduleError = Self(rawValue: "module_error")
    public static let unsubscribed = Self(rawValue: "unsubscribed")
    public static let lagged = Self(rawValue: "lagged")
    public static let moduleRestarted = Self(rawValue: "module_restarted")
    public static let notExposed = Self(rawValue: "not_exposed")
    public static let revoked = Self(rawValue: "revoked")
    public static let rotating = Self(rawValue: "rotating")
    public static let draining = Self(rawValue: "draining")
}

public struct FedSubscriptionSource: RawRepresentable, Sendable, Hashable, Codable {
    public let rawValue: String
    public init(rawValue: String) { self.rawValue = rawValue }
    public init(from decoder: Decoder) throws { self.init(rawValue: try decoder.singleValueContainer().decode(String.self)) }
    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(rawValue)
    }
    public static let local = Self(rawValue: "local")
    public static let peer = Self(rawValue: "peer")
}

public struct FedSubscriptionFailure: Error, Sendable, Equatable {
    public typealias Reason = FedSubscriptionReason
    public typealias Source = FedSubscriptionSource
    public let reason: Reason
    public let source: Source
    public let underlying: FedFailure?
    public init(reason: Reason, source: Source = .local, underlying: FedFailure? = nil) {
        self.reason = reason
        self.source = source
        self.underlying = underlying
    }
}

public struct FedSubscriptionModuleError: Sendable, Equatable {
    public let code: String
    public let message: String?
    public let detail: FedJSONValue?
}

public struct FedSubscriptionEnd: Sendable, Equatable {
    public typealias Reason = FedSubscriptionReason
    public typealias Source = FedSubscriptionSource
    public let reason: Reason
    public let source: Source
    public let body: Data
    public let moduleError: FedSubscriptionModuleError?
}

public struct FedNotice: Sendable, Equatable {
    /// The exact module payload, including binary bytes; SubcFed adds no cursor.
    public let body: Data
}

/// A transient, single-consumer watch. The app owns cursors and reopening after loss.
public final class FedSubscription: AsyncSequence, Sendable {
    public typealias Element = FedNotice
    private let storage: FedSubscriptionStorage
    private let cancelWatch: @Sendable () async -> Void

    init(storage: FedSubscriptionStorage, cancel: @escaping @Sendable () async -> Void) {
        self.storage = storage
        self.cancelWatch = cancel
    }

    deinit {
        let cancel = cancelWatch
        Task { await cancel() }
    }

    /// Settles locally as `unsubscribed` and schedules one best-effort unsubscribe.
    /// Accepted notices remain drainable. This is idempotent and never waits for acknowledgement.
    public func cancel() async { await cancelWatch() }

    /// Observes the first terminal decision independently of notice consumption.
    /// Session loss throws `FedSubscriptionFailure`; cancelling this wait does not cancel the watch.
    public func end() async throws -> FedSubscriptionEnd { try await storage.end() }

    /// Only the first iterator may consume notices; subsequent iterators throw `multiple_consumers`.
    public func makeAsyncIterator() -> AsyncIterator {
        AsyncIterator(subscription: self, admitted: storage.claimIterator())
    }

    public struct AsyncIterator: AsyncIteratorProtocol {
        private let subscription: FedSubscription
        private let admitted: Bool
        init(subscription: FedSubscription, admitted: Bool) {
            self.subscription = subscription
            self.admitted = admitted
        }
        public mutating func next() async throws -> FedNotice? {
            guard admitted else { throw FedSubscriptionFailure(reason: .multipleConsumers) }
            return try await withTaskCancellationHandler {
                if Task.isCancelled {
                    await subscription.cancel()
                    throw CancellationError()
                }
                let value: FedNotice?
                do { value = try await subscription.storage.next() }
                catch {
                    if Task.isCancelled { await subscription.cancel(); throw CancellationError() }
                    throw error
                }
                if Task.isCancelled {
                    await subscription.cancel()
                    throw CancellationError()
                }
                return value
            } onCancel: { [subscription] in
                Task { await subscription.cancel() }
            }
        }
    }
}
