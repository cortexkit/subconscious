import Foundation

/// The additive registry deliberately leaves the original public frame enums unchanged.
enum FedSubscriptionCodec {
    static let feature = "subscribe-v1"
    static let maximumID: UInt64 = 9_007_199_254_740_991
    static let frameTypes: Set<String> = ["subscribe", "open", "subscription_refused", "notice", "end", "unsubscribe"]
    static let emptyEndReasons: Set<String> = [
        "completed", "unsubscribed", "lagged", "module_restarted", "not_exposed", "revoked", "rotating", "draining",
    ]

    static func validateHeader(_ header: FedJSONObject, type: String, complete: Bool, features: Set<String>) throws {
        func invalid(_ field: String) -> FedFrameError { .invalidHeaderField(type: type, field: field) }
        guard complete, features.contains(feature) else { throw invalid("type") }
        var keys: Set<String> = ["type", "id"]
        switch type {
        case "subscribe": keys.formUnion(["module", "op"])
        case "subscription_refused": keys.insert("code")
        case "end": keys.insert("reason")
        default: break
        }
        guard Set(header.keys) == keys else { throw invalid("keys") }
        guard case .integer(let id) = header["id"], id <= maximumID else { throw invalid("id") }
        if type == "subscribe" {
            guard case .string(let module) = header["module"],
                  FedCatalogCodec.isValidIdentifier(module), !module.contains(".") else { throw invalid("module") }
            guard case .string(let op) = header["op"], FedCatalogCodec.isValidOperationName(op) else { throw invalid("op") }
        }
        for field in ["code", "reason"] where keys.contains(field) {
            guard case .string(let text) = header[field], !text.isEmpty else { throw invalid(field) }
        }
    }

    static func validateDirection(_ frame: FedFrame, fromOrigin: Bool) throws {
        guard let type = frame.typeName, frameTypes.contains(type) else { return }
        guard fromOrigin == (type == "subscribe" || type == "unsubscribe") else {
            throw FedFailure.protocolViolation(byeCode: "fed_bad_frame")
        }
    }

    static func validateBody(_ body: Data, type: String, header: FedJSONObject) throws {
        let reason: String = if case .string(let text) = header["reason"] { text } else { "" }
        if ["open", "subscription_refused", "unsubscribe"].contains(type)
            || type == "end" && emptyEndReasons.contains(reason) {
            guard body.isEmpty else { throw FedFrameError.bodylessFrame(type: type, declared: UInt32(clamping: body.count)) }
        }
        if type == "end", header["reason"] == .string("module_error") {
            _ = try moduleError(body)
        }
    }

    static func moduleError(_ body: Data) throws -> FedSubscriptionModuleError {
        do {
            let object = try FedJSONObject(jsonData: body)
            guard case .string(let code) = object["code"], !code.isEmpty else {
                throw FedFrameError.invalidHeaderField(type: "end", field: "body.code")
            }
            var message: String?
            if let value = object["message"] {
                guard case .string(let text) = value else {
                    throw FedFrameError.invalidHeaderField(type: "end", field: "body.message")
                }
                message = text
            }
            return FedSubscriptionModuleError(code: code, message: message, detail: object["detail"])
        } catch let error as FedFrameError { throw error }
        catch { throw FedFrameError.invalidHeaderField(type: "end", field: "body") }
    }
}
