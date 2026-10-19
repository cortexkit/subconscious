import Foundation

/// Wire ordering is separate from the public outcome: a locally ended watch still
/// consumes raced frames until its peer terminal releases the reservation.
struct FedSubscriptionWireState {
    enum Phase { case unused, opening, open, terminal }
    private(set) var phase: Phase = .unused
    private var cancellationDeliveredBeforeOpen = false

    mutating func unsubscribe(deliveredToServer: Bool = false) {
        if phase == .opening, deliveredToServer { cancellationDeliveredBeforeOpen = true }
    }

    mutating func receive(_ frame: FedFrame) throws {
        let type = frame.typeName
        switch (phase, type) {
        case (.unused, "subscribe"):
            phase = .opening
        case (.opening, "open") where !cancellationDeliveredBeforeOpen:
            phase = .open
        case (.opening, "subscription_refused"):
            if cancellationDeliveredBeforeOpen, frame.header["code"] != .string("fed_cancelled") {
                throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
            }
            phase = .terminal
        case (.open, "notice"):
            break
        case (.open, "end"):
            phase = .terminal
        default:
            throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
        }
    }
}
