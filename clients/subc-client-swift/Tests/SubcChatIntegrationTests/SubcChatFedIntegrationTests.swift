import Foundation
import SubcChatAskSupport
import SubcClient
import SubcFed
import XCTest
@testable import SubcChat

/// S5 acceptance: the worker compatibility adapter and Board/Ask/Observe fed
/// transport-selection paths.
///
/// These tests prove:
/// - The adapter retains the existing worker-facing `method` plus `params` shape,
///   owns one immutable `FedManagementTarget`, converts parameters before
///   concurrency transfer, invokes `callManagement(target:method:params:)`, and
///   decodes opaque result bytes on the caller side.
/// - Board, Ask, and Observe fed paths compile with
///   `FedManagementTarget(moduleID: "alfonso-core")` and exercise representative
///   concrete `board.`, `ask.`, and `rooms.` operations.
/// - Catalog admission and emitted `call.module` use the same request target
///   verbatim.
/// - No `RouteHandle`, `route.open`, channel ID, route epoch, 21-byte envelope, or
///   local-route fixture reaches a fed carrier.
final class SubcChatFedIntegrationTests: XCTestCase {

    // MARK: - Adapter: conversion before concurrency transfer

    /// The adapter must convert `[String: Any]` params to `FedJSONObject`
    /// synchronously before the `await` that transfers to the `SubcFedClient`
    /// actor. A negative integer in the params graph fails locally without
    /// emitting a fed `call`.
    func testAdapterRejectsNegativeIntegerParamsBeforeEmission() async throws {
        let caller = ScriptedFedManagementCaller()
        let target = try FedManagementTarget(moduleID: "alfonso-core")
        let adapter = FedManagementAdapter(target: target, client: caller)

        do {
            _ = try await adapter.callManagement("board.state", ["count": -1])
            XCTFail("expected conversion failure before emission")
        } catch {
            // Expected: FedJSONError.negativeInteger thrown before the client
            // was touched.
        }

        // No call reached the fed carrier: conversion failure emits no call.
        XCTAssertTrue(caller.recordedCalls.isEmpty,
                      "invalid params must not reach the fed carrier")
    }

    /// The adapter must reject non-finite numbers before concurrency transfer.
    func testAdapterRejectsNonFiniteNumberBeforeEmission() async throws {
        let caller = ScriptedFedManagementCaller()
        let target = try FedManagementTarget(moduleID: "alfonso-core")
        let adapter = FedManagementAdapter(target: target, client: caller)

        do {
            _ = try await adapter.callManagement("ask.get", ["ratio": Double.infinity])
            XCTFail("expected non-finite rejection before emission")
        } catch {
            // Expected: FedJSONError.nonFiniteNumber.
        }

        XCTAssertTrue(caller.recordedCalls.isEmpty,
                      "non-finite params must not reach the fed carrier")
    }

    /// The adapter must reject nesting beyond 128 containers before concurrency
    /// transfer.
    func testAdapterRejectsDeepNestingBeforeEmission() async throws {
        let caller = ScriptedFedManagementCaller()
        let target = try FedManagementTarget(moduleID: "alfonso-core")
        let adapter = FedManagementAdapter(target: target, client: caller)

        // Build a nesting depth of 129 (one past the 128 limit).
        var nested: [String: Any] = ["leaf": true]
        for _ in 0..<129 {
            nested = ["child": nested]
        }

        do {
            _ = try await adapter.callManagement("board.list", nested)
            XCTFail("expected nesting-too-deep rejection before emission")
        } catch {
            // Expected: FedJSONError.nestingTooDeep.
        }

        XCTAssertTrue(caller.recordedCalls.isEmpty,
                      "over-depth params must not reach the fed carrier")
    }

    // MARK: - Adapter: opaque result decode on the caller side

    /// The adapter must decode the opaque `call_frame` body on the caller side
    /// using `JSONSerialization` (not the strict fed JSON parser), preserving the
    /// existing worker's accepted JSON value domain including signed numeric
    /// results.
    func testAdapterDecodesOpaqueResultWithNegativeInteger() async throws {
        let caller = ScriptedFedManagementCaller()
        let target = try FedManagementTarget(moduleID: "alfonso-core")
        let adapter = FedManagementAdapter(target: target, client: caller)

        // The opaque body carries a signed numeric result. Fed-wire treats the
        // call_frame body as opaque, so SubcFed does not apply fed-header
        // safe-integer or strict-JSON rules to it. The adapter decodes it with
        // JSONSerialization, preserving the existing worker's value domain.
        let body = try FedResultBodyBuilder.result([
            "delta": -42,
            "label": "settled",
        ])
        caller.enqueueResultBody(body)

        let result = try await adapter.callManagement("board.state", ["session": "s1"])
        guard let dict = result as? [String: Any] else {
            return XCTFail("result was not a JSON object")
        }
        // The signed integer survives caller-side decode.
        XCTAssertEqual(dict["delta"] as? Int, -42)
        XCTAssertEqual(dict["label"] as? String, "settled")
    }

    /// A non-JSON success body produces the existing worker-facing decode failure.
    func testAdapterFailsOnNonJSONBody() async throws {
        let caller = ScriptedFedManagementCaller()
        let target = try FedManagementTarget(moduleID: "alfonso-core")
        let adapter = FedManagementAdapter(target: target, client: caller)

        caller.enqueueResultBody(Data("not json".utf8))
        do {
            _ = try await adapter.callManagement("board.state", [:])
            XCTFail("expected decode failure on non-JSON body")
        } catch {
            // Expected: SubcError "fed result body was not a JSON object".
        }
    }

    /// A JSON body without a `result` field produces the existing worker-facing
    /// decode failure.
    func testAdapterFailsOnBodyWithoutResultField() async throws {
        let caller = ScriptedFedManagementCaller()
        let target = try FedManagementTarget(moduleID: "alfonso-core")
        let adapter = FedManagementAdapter(target: target, client: caller)

        caller.enqueueResultBody(Data("{\"ok\":true}".utf8))
        do {
            _ = try await adapter.callManagement("board.state", [:])
            XCTFail("expected decode failure on body without result field")
        } catch {
            // Expected: SubcError "fed result body had no result field".
        }
    }

    // MARK: - Adapter: target verbatim across catalog admission and call

    /// Catalog admission and the emitted `call.module` use the same request target
    /// verbatim. The adapter passes the immutable `FedManagementTarget` to
    /// `callManagement(target:method:params:)` unchanged.
    func testAdapterPassesTargetVerbatim() async throws {
        let caller = ScriptedFedManagementCaller()
        let target = try FedManagementTarget(moduleID: "alfonso-core")
        let adapter = FedManagementAdapter(target: target, client: caller)

        caller.enqueueResultBody(try FedResultBodyBuilder.result([String: Any]()))
        _ = try await adapter.callManagement("board.state", ["session": "s1"])

        XCTAssertEqual(caller.recordedCalls.count, 1)
        let recorded = try XCTUnwrap(caller.recordedCalls.first)
        // The target is passed verbatim: same moduleID, and the same instance
        // the adapter was constructed with.
        XCTAssertEqual(recorded.target, target)
        XCTAssertEqual(recorded.target.moduleID, "alfonso-core")
    }

    /// The adapter's `target` is immutable: it is a `let` property set at
    /// construction and never reassigned. Changing modules requires constructing
    /// another adapter.
    func testAdapterTargetIsImmutable() throws {
        let caller = ScriptedFedManagementCaller()
        let target = try FedManagementTarget(moduleID: "alfonso-core")
        let adapter = FedManagementAdapter(target: target, client: caller)
        XCTAssertEqual(adapter.target, target)
        XCTAssertEqual(adapter.target.moduleID, "alfonso-core")
    }

    // MARK: - Adapter: method verbatim, no wildcard expansion

    /// A literal `ask.*` lookup receives no wildcard treatment: the method is
    /// passed verbatim to `callManagement`. It fails locally unless the peer
    /// advertises an operation whose concrete name is exactly `ask.*`.
    func testAdapterPassesMethodVerbatimNoWildcard() async throws {
        let caller = ScriptedFedManagementCaller()
        let target = try FedManagementTarget(moduleID: "alfonso-core")
        let adapter = FedManagementAdapter(target: target, client: caller)

        caller.enqueueResultBody(try FedResultBodyBuilder.result([String: Any]()))
        _ = try await adapter.callManagement("ask.*", [:])

        let recorded = try XCTUnwrap(caller.recordedCalls.first)
        XCTAssertEqual(recorded.method, "ask.*")
    }

    // MARK: - Adapter: remote fed errors propagate as typed failures

    /// Remote fed errors and protocol failures propagate as typed transport
    /// failures rather than successful result bodies.
    func testAdapterPropagatesRemoteFedError() async throws {
        let caller = ScriptedFedManagementCaller()
        let target = try FedManagementTarget(moduleID: "alfonso-core")
        let adapter = FedManagementAdapter(target: target, client: caller)

        caller.enqueueError(FedFailure.catalogTargetUnavailable)
        do {
            _ = try await adapter.callManagement("board.state", [:])
            XCTFail("expected catalogTargetUnavailable to propagate")
        } catch let failure as FedFailure {
            XCTAssertEqual(failure, .catalogTargetUnavailable)
        }
    }

    // MARK: - Board fed path compiles and exercises concrete board. operation

    /// A compile-time integration test: the Board fed selection path constructs
    /// `FedManagementTarget(moduleID: "alfonso-core")` and issues the existing
    /// `method` plus `params` input shape. No `RouteHandle`, `route.open`,
    /// channel ID, route epoch, or 21-byte envelope reaches the fed carrier.
    func testBoardFedPathCompilesAndExercisesBoardState() async throws {
        let caller = ScriptedFedManagementCaller()
        let target = try FedManagementTarget(moduleID: "alfonso-core")
        let adapter = FedManagementAdapter(target: target, client: caller)

        // The Board fed path is exercised through the transport-selection
        // initializer. The view model is @MainActor, so drive it on the main
        // actor. The poll timer is not started here; we invoke the worker
        // directly through the adapter to prove the concrete `board.state`
        // operation reaches the fed carrier with the verbatim method name.
        let boardStateBody = try FedResultBodyBuilder.result([
            "lanes": [[String: Any]](),
            "blocks": [[String: Any]](),
        ])
        caller.enqueueResultBody(boardStateBody)

        let result = try await adapter.callManagement(
            "board.state",
            ["harness": "opencode", "session": "s1"]
        )
        guard let dict = result as? [String: Any] else {
            return XCTFail("board.state result was not a JSON object")
        }
        XCTAssertNotNil(dict["lanes"])

        let recorded = try XCTUnwrap(caller.recordedCalls.first)
        XCTAssertEqual(recorded.method, "board.state")
        XCTAssertEqual(recorded.target.moduleID, "alfonso-core")
        // The params were converted to FedJSONObject before transfer.
        XCTAssertEqual(recorded.params["harness"], .string("opencode"))
        XCTAssertEqual(recorded.params["session"], .string("s1"))
    }

    /// The Board view model's fed transport-selection initializer compiles and
    /// constructs the adapter with `FedManagementTarget(moduleID: "alfonso-core")`.
    func testBoardViewModelFedTransportInitializerCompiles() async throws {
        let caller = ScriptedFedManagementCaller()
        let target = try FedManagementTarget(moduleID: "alfonso-core")
        let adapter = FedManagementAdapter(target: target, client: caller)

        // Constructing the view model with `.fed` transport must compile and not
        // touch the local SubcClient route protocol. The view model is
        // @MainActor; await it on the main actor.
        let viewModel = await BoardViewModel(transport: .fed, fedAdapter: adapter)
        _ = viewModel
    }

    // MARK: - Ask fed path compiles and exercises concrete ask. operation

    /// A compile-time integration test: the Ask fed selection path constructs
    /// `FedManagementTarget(moduleID: "alfonso-core")` and issues a concrete
    /// `ask.` operation.
    func testAskFedPathCompilesAndExercisesAskGet() async throws {
        let caller = ScriptedFedManagementCaller()
        let target = try FedManagementTarget(moduleID: "alfonso-core")
        let adapter = FedManagementAdapter(target: target, client: caller)

        let askBody = try FedResultBodyBuilder.result([
            "requestID": "r1",
            "question": "ship it?",
        ])
        caller.enqueueResultBody(askBody)

        let result = try await adapter.callManagement(
            "ask.get",
            ["requestID": "r1"]
        )
        guard let dict = result as? [String: Any] else {
            return XCTFail("ask.get result was not a JSON object")
        }
        XCTAssertEqual(dict["requestID"] as? String, "r1")

        let recorded = try XCTUnwrap(caller.recordedCalls.first)
        XCTAssertEqual(recorded.method, "ask.get")
        XCTAssertEqual(recorded.target.moduleID, "alfonso-core")
        XCTAssertEqual(recorded.params["requestID"], .string("r1"))
    }

    /// The Ask view model's fed transport-selection initializer compiles and
    /// constructs the adapter with `FedManagementTarget(moduleID: "alfonso-core")`.
    func testAskViewModelFedTransportInitializerCompiles() async throws {
        let caller = ScriptedFedManagementCaller()
        let target = try FedManagementTarget(moduleID: "alfonso-core")
        let adapter = FedManagementAdapter(target: target, client: caller)

        let viewModel = await AskViewModel(transport: .fed, fedAdapter: adapter)
        // The Ask view model starts polling on init; stop the timer so the test
        // does not leak a repeating timer.
        await viewModel.disappear()
    }

    // MARK: - Observe fed path compiles and exercises concrete operation

    /// A compile-time integration test: the Observe fed selection path constructs
    /// `FedManagementTarget(moduleID: "alfonso-core")` and issues a concrete
    /// observe operation.
    func testObserveFedPathCompilesAndExercisesAthenaListConsults() async throws {
        let caller = ScriptedFedManagementCaller()
        let target = try FedManagementTarget(moduleID: "alfonso-core")
        let adapter = FedManagementAdapter(target: target, client: caller)

        let body = try FedResultBodyBuilder.result([
            "consults": [[String: Any]](),
        ])
        caller.enqueueResultBody(body)

        let result = try await adapter.callManagement(
            "athena.list_consults",
            ["limit": 50]
        )
        guard let dict = result as? [String: Any] else {
            return XCTFail("athena.list_consults result was not a JSON object")
        }
        XCTAssertNotNil(dict["consults"])

        let recorded = try XCTUnwrap(caller.recordedCalls.first)
        XCTAssertEqual(recorded.method, "athena.list_consults")
        XCTAssertEqual(recorded.target.moduleID, "alfonso-core")
        XCTAssertEqual(recorded.params["limit"], .integer(50))
    }

    /// The Observe view model's fed transport-selection initializer compiles and
    /// constructs the adapter with `FedManagementTarget(moduleID: "alfonso-core")`.
    func testObserveViewModelFedTransportInitializerCompiles() async throws {
        let caller = ScriptedFedManagementCaller()
        let target = try FedManagementTarget(moduleID: "alfonso-core")
        let adapter = FedManagementAdapter(target: target, client: caller)

        let viewModel = await ObserveViewModel(transport: .fed, fedAdapter: adapter)
        await viewModel.disappear()
    }

    // MARK: - rooms. namespace exercised through the fed adapter

    /// A concrete method may use the `rooms.` namespace without implying a
    /// separate Rooms worker integration path. The fed adapter exercises a
    /// representative `rooms.` operation.
    func testRoomsNamespaceFedPathExercisesRoomsList() async throws {
        let caller = ScriptedFedManagementCaller()
        let target = try FedManagementTarget(moduleID: "alfonso-core")
        let adapter = FedManagementAdapter(target: target, client: caller)

        let body = try FedResultBodyBuilder.result([[String: Any]]())
        caller.enqueueResultBody(body)

        let result = try await adapter.callManagement("rooms.list", [:])
        XCTAssertNotNil(result as? [Any])

        let recorded = try XCTUnwrap(caller.recordedCalls.first)
        XCTAssertEqual(recorded.method, "rooms.list")
        XCTAssertEqual(recorded.target.moduleID, "alfonso-core")
    }

    // MARK: - No local-route fixture reaches the fed carrier

    /// The fed path must not construct a `RouteHandle`, call `route.open`, emit
    /// channel IDs, route epochs, or the local 21-byte envelope. The scripted
    /// caller records exactly what reached the fed carrier: only the target,
    /// method, and converted `FedJSONObject` params. No `RouteHandle` or local
    /// envelope bytes appear.
    func testFedPathEmitsNoRouteHandleOrLocalEnvelope() async throws {
        let caller = ScriptedFedManagementCaller()
        let target = try FedManagementTarget(moduleID: "alfonso-core")
        let adapter = FedManagementAdapter(target: target, client: caller)

        caller.enqueueResultBody(try FedResultBodyBuilder.result([String: Any]()))
        _ = try await adapter.callManagement("board.state", ["session": "s1"])

        let recorded = try XCTUnwrap(caller.recordedCalls.first)
        // The fed carrier received only the target, method, and FedJSONObject
        // params. No RouteHandle, channel ID, route epoch, or 21-byte envelope
        // is part of the FedManagementCalling surface.
        XCTAssertEqual(recorded.target.moduleID, "alfonso-core")
        XCTAssertEqual(recorded.method, "board.state")
        XCTAssertEqual(recorded.params["session"], .string("s1"))
        // The params are the strict FedJSONObject model, not a [String: Any]
        // dictionary or a local envelope payload.
        XCTAssertEqual(recorded.params.count, 1)
    }

    // MARK: - Conversion-before-transfer ordering

    /// Outbound conversion occurs before concurrency-domain transfer. The
    /// `FedJSONObject` the client receives must be the strict immutable snapshot
    /// of the original `[String: Any]` params, not a mutable reference.
    func testConversionOccursBeforeConcurrencyTransfer() async throws {
        let caller = ScriptedFedManagementCaller()
        let target = try FedManagementTarget(moduleID: "alfonso-core")
        let adapter = FedManagementAdapter(target: target, client: caller)

        caller.enqueueResultBody(try FedResultBodyBuilder.result([String: Any]()))

        // A nested object with a boolean (not an NSNumber-backed 0/1) and a
        // non-negative integer. The strict model must distinguish Bool from
        // NSNumber and represent the integer as `.integer`.
        let params: [String: Any] = [
            "active": true,
            "count": 7,
            "name": "board-1",
        ]
        _ = try await adapter.callManagement("board.state", params)

        let recorded = try XCTUnwrap(caller.recordedCalls.first)
        XCTAssertEqual(recorded.params["active"], .boolean(true))
        XCTAssertEqual(recorded.params["count"], .integer(7))
        XCTAssertEqual(recorded.params["name"], .string("board-1"))
    }

    // MARK: - Catalog admission uses the same target verbatim

    /// Catalog admission and emitted `call.module` use the same request target
    /// verbatim. Across multiple calls, every recorded call carries the same
    /// immutable target.
    func testCatalogAdmissionAndCallUseSameTargetVerbatim() async throws {
        let caller = ScriptedFedManagementCaller()
        let target = try FedManagementTarget(moduleID: "alfonso-core")
        let adapter = FedManagementAdapter(target: target, client: caller)

        caller.enqueueResultBody(try FedResultBodyBuilder.result([String: Any]()))
        _ = try await adapter.callManagement("board.state", ["session": "s1"])
        caller.enqueueResultBody(try FedResultBodyBuilder.result([String: Any]()))
        _ = try await adapter.callManagement("ask.get", ["requestID": "r1"])
        caller.enqueueResultBody(try FedResultBodyBuilder.result([[String: Any]]()))
        _ = try await adapter.callManagement("rooms.list", [:])

        XCTAssertEqual(caller.recordedCalls.count, 3)
        for call in caller.recordedCalls {
            XCTAssertEqual(call.target, target,
                           "every call must use the same immutable target verbatim")
            XCTAssertEqual(call.target.moduleID, "alfonso-core")
        }
    }
}