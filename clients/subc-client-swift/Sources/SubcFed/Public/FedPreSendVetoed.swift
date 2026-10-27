/// The caller's pre-send check refused a management call before its first write.
/// No request bytes were handed to the transport. A mutation is durably settled
/// as not sent before this error is returned.
public struct FedPreSendVetoed: Error, Sendable, Equatable {
    public init() {}
}
